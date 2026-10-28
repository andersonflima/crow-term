//! A small byte scanner in front of the VT parser.
//!
//! It recognises the few control sequences that `alacritty_terminal` does not
//! handle the way Crow Term needs, while forwarding every other byte unchanged
//! and in order. The scanner mirrors the escape/CSI state transitions of the
//! VT parser closely enough that it can never miss a sequence the parser would
//! act on: an ESC always starts a new escape sequence (also inside OSC/DCS
//! strings), C0 controls inside a CSI are executed by the parser and therefore
//! forwarded even when the sequence itself is dropped, and CAN/SUB abort.

/// Upper bound of kitty keyboard pushes per screen. The parser's own stack limit
/// is far larger and its overflow path is unsafe, so pushes beyond this are dropped.
pub const KITTY_STACK_LIMIT: u16 = 256;
/// Bytes of one unfinished sequence held before they are forwarded unexamined.
const MAX_HELD: usize = 64 * 1024;

/// A request that the engine answers itself, in stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `CSI ? 4 m`: report xterm modifyOtherKeys (current level attached).
    ModifyOtherKeys(u8),
}

/// Scanner output, in stream order.
#[derive(Debug)]
pub enum Out<'a> {
    /// Bytes for the VT parser.
    Bytes(&'a [u8]),
    /// A query the engine answers after the preceding bytes were parsed.
    Query(Query),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Csi,
}

#[derive(Debug)]
pub struct Interceptor {
    state: State,
    held: Vec<u8>,
    /// C0 controls seen inside the held sequence; the parser executes them.
    controls: Vec<u8>,
    forwarded: bool,
    marker: Option<u8>,
    params: Vec<u8>,
    intermediates: usize,
    ignored: bool,
    alt_screen: bool,
    kitty_depth: [u16; 2],
    /// xterm modifyOtherKeys level (0, 1 or 2), not tracked by the VT parser.
    pub modify_other_keys: u8,
}

impl Default for Interceptor {
    fn default() -> Self {
        Self {
            state: State::Ground,
            held: Vec::new(),
            controls: Vec::new(),
            forwarded: false,
            marker: None,
            params: Vec::new(),
            intermediates: 0,
            ignored: false,
            alt_screen: false,
            kitty_depth: [0; 2],
            modify_other_keys: 0,
        }
    }
}

const ESC: u8 = 0x1b;

impl Interceptor {
    /// Scan `bytes`, passing forwarded bytes and queries to `sink`,
    /// both in stream order. Unfinished sequences are held for the next call.
    pub fn scan(&mut self, bytes: &[u8], sink: &mut impl FnMut(Out<'_>)) {
        let mut index = 0;
        while index < bytes.len() {
            if self.state == State::Ground {
                match bytes[index..].iter().position(|&byte| byte == ESC) {
                    Some(offset) => {
                        if offset > 0 {
                            sink(Out::Bytes(&bytes[index..index + offset]));
                        }
                        index += offset + 1;
                        self.begin_escape();
                    }
                    None => {
                        sink(Out::Bytes(&bytes[index..]));
                        return;
                    }
                }
                continue;
            }
            let byte = bytes[index];
            index += 1;
            self.byte(byte, sink);
        }
    }

    /// Forward a held, unfinished sequence (end of stream).
    pub fn flush(&mut self, sink: &mut impl FnMut(Out<'_>)) {
        if !self.forwarded && !self.held.is_empty() {
            sink(Out::Bytes(&self.held));
        }
        self.reset_sequence();
        self.state = State::Ground;
    }

    fn begin_escape(&mut self) {
        self.reset_sequence();
        self.held.push(ESC);
        self.state = State::Escape;
    }

    fn reset_sequence(&mut self) {
        self.held.clear();
        self.controls.clear();
        self.forwarded = false;
        self.marker = None;
        self.params.clear();
        self.intermediates = 0;
        self.ignored = false;
    }

    fn hold(&mut self, byte: u8, sink: &mut impl FnMut(Out<'_>)) {
        if self.forwarded {
            sink(Out::Bytes(&[byte]));
            return;
        }
        self.held.push(byte);
        if self.held.len() > MAX_HELD {
            sink(Out::Bytes(&self.held));
            self.held.clear();
            self.forwarded = true;
        }
    }

    /// Forward the held sequence unchanged and return to ground.
    fn pass(&mut self, sink: &mut impl FnMut(Out<'_>)) {
        if !self.forwarded && !self.held.is_empty() {
            sink(Out::Bytes(&self.held));
        }
        self.reset_sequence();
        self.state = State::Ground;
    }

    /// Drop the held sequence, forwarding only the C0 controls the parser
    /// would have executed while reading it.
    fn drop_sequence(&mut self, sink: &mut impl FnMut(Out<'_>)) {
        if self.forwarded {
            // Already forwarded in full; nothing can be withheld any more.
        } else if !self.controls.is_empty() {
            sink(Out::Bytes(&self.controls));
        }
        self.reset_sequence();
        self.state = State::Ground;
    }

    fn byte(&mut self, byte: u8, sink: &mut impl FnMut(Out<'_>)) {
        match byte {
            ESC => {
                self.pass(sink);
                self.begin_escape();
                return;
            }
            0x18 | 0x1a => {
                self.hold(byte, sink);
                self.pass(sink);
                return;
            }
            _ => {}
        }
        match self.state {
            State::Ground => unreachable!("ground bytes are handled by scan"),
            State::Escape => match byte {
                0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                    self.controls.push(byte);
                    self.hold(byte, sink);
                }
                0x7f => self.hold(byte, sink),
                b'[' => {
                    self.hold(byte, sink);
                    self.state = State::Csi;
                }
                b'c' => {
                    // RIS: the parser resets screens, keyboard stacks and modes.
                    self.hold(byte, sink);
                    self.alt_screen = false;
                    self.kitty_depth = [0; 2];
                    self.modify_other_keys = 0;
                    self.pass(sink);
                }
                _ => {
                    self.hold(byte, sink);
                    self.pass(sink);
                }
            },
            State::Csi => match byte {
                0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                    self.controls.push(byte);
                    self.hold(byte, sink);
                }
                0x7f => self.hold(byte, sink),
                b'<'..=b'?' => {
                    self.hold(byte, sink);
                    if self.marker.is_none() && self.params.is_empty() && self.intermediates == 0 {
                        self.marker = Some(byte);
                    } else {
                        self.ignored = true;
                    }
                }
                b'0'..=b';' => {
                    self.hold(byte, sink);
                    if self.intermediates > 0 {
                        self.ignored = true;
                    } else if self.params.len() < 256 {
                        self.params.push(byte);
                    }
                }
                0x20..=0x2f => {
                    self.hold(byte, sink);
                    self.intermediates += 1;
                }
                0x40..=0x7e => {
                    self.hold(byte, sink);
                    self.dispatch(byte, sink);
                }
                _ => self.hold(byte, sink),
            },
        }
    }

    /// Parameter values as the VT parser reads them: first subparameter of
    /// each `;`-separated field, saturating, empty fields as zero.
    fn param_values(&self) -> Vec<u16> {
        self.params
            .split(|&byte| byte == b';')
            .map(|field| {
                field
                    .iter()
                    .take_while(|&&byte| byte != b':')
                    .fold(0u16, |value, &digit| {
                        if digit.is_ascii_digit() {
                            value
                                .saturating_mul(10)
                                .saturating_add(u16::from(digit - b'0'))
                        } else {
                            value
                        }
                    })
            })
            .collect()
    }

    fn dispatch(&mut self, final_byte: u8, sink: &mut impl FnMut(Out<'_>)) {
        if self.ignored || self.intermediates > 0 {
            self.pass(sink);
            return;
        }
        let params = self.param_values();
        let first = params.first().copied().unwrap_or(0);
        let screen = usize::from(self.alt_screen);
        match (self.marker, final_byte) {
            (Some(b'>'), b'u') => {
                if self.kitty_depth[screen] >= KITTY_STACK_LIMIT {
                    self.drop_sequence(sink);
                    return;
                }
                self.kitty_depth[screen] += 1;
            }
            (Some(b'<'), b'u') => {
                let count = if first == 0 { 1 } else { first };
                self.kitty_depth[screen] = self.kitty_depth[screen].saturating_sub(count);
            }
            (Some(b'?'), b'h' | b'l') if params.contains(&1049) => {
                self.alt_screen = final_byte == b'h';
            }
            (Some(b'>'), b'm') if first == 4 => {
                if let level @ 0..=2 = params.get(1).copied().unwrap_or(0) {
                    self.modify_other_keys = level as u8;
                }
            }
            (Some(b'?'), b'm') if first == 4 && params.len() == 1 => {
                self.pass(sink);
                sink(Out::Query(Query::ModifyOtherKeys(self.modify_other_keys)));
                return;
            }
            _ => {}
        }
        self.pass(sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(interceptor: &mut Interceptor, chunks: &[&[u8]]) -> (Vec<u8>, Vec<Query>) {
        let mut fed = Vec::new();
        let mut queries = Vec::new();
        for chunk in chunks {
            interceptor.scan(chunk, &mut |out| match out {
                Out::Bytes(bytes) => fed.extend_from_slice(bytes),
                Out::Query(query) => queries.push(query),
            });
        }
        (fed, queries)
    }

    #[test]
    fn ordinary_output_is_forwarded_byte_for_byte_across_chunk_splits() {
        let input = b"plain \x1b[1;31mred\x1b[0m \x1b]2;title\x07 \x1bP+q4d73\x1b\\ \xe7\x95\x8c\x1b(B\x1b[?1049h\x1b[?1049l";
        for split in 0..input.len() {
            let mut interceptor = Interceptor::default();
            let (fed, queries) = run(&mut interceptor, &[&input[..split], &input[split..]]);
            assert_eq!(fed, input, "split at {split}");
            assert!(queries.is_empty());
        }
    }

    #[test]
    fn kitty_pushes_are_bounded_per_screen_and_pops_release_capacity() {
        let mut interceptor = Interceptor::default();
        let push = b"\x1b[>1u";
        let pushes: Vec<u8> = push.repeat(KITTY_STACK_LIMIT as usize + 10);
        let (fed, _) = run(&mut interceptor, &[&pushes]);
        assert_eq!(fed.len(), push.len() * KITTY_STACK_LIMIT as usize);
        // The alternate screen has its own stack in the parser.
        let (fed, _) = run(&mut interceptor, &[b"\x1b[?1049h", push]);
        assert_eq!(fed, b"\x1b[?1049h\x1b[>1u");
        let (fed, _) = run(&mut interceptor, &[b"\x1b[?1049l\x1b[<3u", push]);
        assert_eq!(fed, b"\x1b[?1049l\x1b[<3u\x1b[>1u");
        // Dropped sequences still forward the C0 controls the parser executes.
        let (_, _) = run(&mut interceptor, &[&push.repeat(2)]);
        let (fed, _) = run(&mut interceptor, &[b"\x1b[>1\n\x07u"]);
        assert_eq!(fed, b"\n\x07");
        // RIS clears the parser's stacks.
        let (fed, _) = run(&mut interceptor, &[b"\x1bc", push]);
        assert_eq!(fed, b"\x1bc\x1b[>1u");
    }

    #[test]
    fn modify_other_keys_is_tracked_and_reported_in_order() {
        let mut interceptor = Interceptor::default();
        let (fed, queries) = run(&mut interceptor, &[b"\x1b[>4;2m"]);
        assert_eq!(fed, b"\x1b[>4;2m");
        assert!(queries.is_empty());
        assert_eq!(interceptor.modify_other_keys, 2);
        let (_, queries) = run(&mut interceptor, &[b"\x1b[?4", b"m"]);
        assert_eq!(queries, vec![Query::ModifyOtherKeys(2)]);
        run(&mut interceptor, &[b"\x1b[>4m"]);
        assert_eq!(interceptor.modify_other_keys, 0);
        run(&mut interceptor, &[b"\x1b[>4;1m\x1b[>4;9m"]);
        assert_eq!(interceptor.modify_other_keys, 1);
        run(&mut interceptor, &[b"\x1bc"]);
        assert_eq!(interceptor.modify_other_keys, 0);
    }

    #[test]
    fn malformed_and_aborted_sequences_are_not_interpreted() {
        let mut interceptor = Interceptor::default();
        // Marker after a parameter and CAN abort: the parser ignores both.
        let (fed, _) = run(&mut interceptor, &[b"\x1b[1>4;2m\x1b[>4;2\x18m"]);
        assert_eq!(fed, b"\x1b[1>4;2m\x1b[>4;2\x18m");
        assert_eq!(interceptor.modify_other_keys, 0);
    }

    #[test]
    fn oversized_sequences_are_forwarded_without_unbounded_buffering() {
        let mut interceptor = Interceptor::default();
        let mut input = b"\x1b[".to_vec();
        input.extend(std::iter::repeat_n(b'0', MAX_HELD + 10));
        input.extend_from_slice(b"1u");
        let (fed, _) = run(&mut interceptor, &[&input]);
        assert_eq!(fed, input);
        assert!(interceptor.held.capacity() <= MAX_HELD * 2 + 64);
    }
}
