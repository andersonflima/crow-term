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
    /// `CSI > q` / `CSI > 0 q`: XTVERSION.
    Version,
    /// `CSI > c` / `CSI > 0 c`: secondary device attributes (the sequence is
    /// withheld from the parser, which would report its own crate version).
    SecondaryAttributes,
    /// `CSI 16 t`: character cell size in pixels.
    CellSize,
    /// `DCS + q Pt ST`: XTGETTCAP, hex-encoded capability names.
    Termcap(Vec<String>),
    /// `DCS $ q Pt ST`: DECRQSS for the setting named by `Pt`.
    StatusString(Vec<u8>),
    /// `CSI ? 996 n`: light/dark color scheme report (answered `CSI ? 997 ; 1|2 n`).
    ColorScheme,
    /// `CSI ? 2031 $ p`: DECRQM for color scheme update notifications
    /// (current state attached).
    ColorSchemeUpdatesMode(bool),
}

/// Longest DCS query payload examined; longer strings are forwarded as-is.
const MAX_DCS_QUERY: usize = 4096;
/// Largest OSC 52 string forwarded: base64 of a 1 MiB copy plus its header.
/// Larger copies are discarded before the parser decodes them.
pub const MAX_OSC52_BYTES: usize = 4 * (1024 * 1024usize).div_ceil(3) + 64;

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
    /// DCS whose payload may still be an XTGETTCAP/DECRQSS query.
    Dcs,
    /// Any other DCS string: forwarded until ESC, CAN, SUB or 8-bit ST.
    DcsPass,
    /// OSC whose number is not known yet.
    Osc,
    /// OSC 52 (clipboard) held whole until its terminator, within the cap.
    Osc52,
    /// Any other OSC string: forwarded until BEL, ESC, CAN or SUB.
    OscPass,
    /// Oversized OSC 52: discarded until its terminator.
    OscDrop,
}

#[derive(Debug)]
pub struct Interceptor {
    state: State,
    /// The escape sequence in progress, without the C0 controls inside it
    /// (those are forwarded immediately: the parser executes them in place).
    held: Vec<u8>,
    /// The sequence outgrew `MAX_HELD`: it is dropped whole and never acted on.
    overflow: bool,
    marker: Option<u8>,
    /// Any parameter byte seen (a marker is only valid before them).
    started: bool,
    /// First subparameter of each parameter, saturating like the parser.
    values: Vec<u16>,
    current: u16,
    /// The current parameter's first subparameter is already in `values`.
    field_done: bool,
    /// Parameter + subparameter separators; at 32 the parser ignores the CSI.
    separators: usize,
    intermediates: usize,
    /// Last intermediate byte of the CSI in progress (`$` of DECRQM).
    intermediate: Option<u8>,
    ignored: bool,
    /// DCS payload after `ESC P` while it may still be a query.
    payload: Vec<u8>,
    alt_screen: bool,
    kitty_depth: [u16; 2],
    /// xterm modifyOtherKeys level (0, 1 or 2), not tracked by the VT parser.
    pub modify_other_keys: u8,
    /// Mode 2031: the application wants `CSI ? 997 ; 1|2 n` whenever the
    /// light/dark color scheme changes.
    pub color_scheme_updates: bool,
}

impl Default for Interceptor {
    fn default() -> Self {
        Self {
            state: State::Ground,
            held: Vec::new(),
            overflow: false,
            marker: None,
            started: false,
            values: Vec::new(),
            current: 0,
            field_done: false,
            separators: 0,
            intermediates: 0,
            intermediate: None,
            ignored: false,
            payload: Vec::new(),
            alt_screen: false,
            kitty_depth: [0; 2],
            modify_other_keys: 0,
            color_scheme_updates: false,
        }
    }
}

const ESC: u8 = 0x1b;
/// vte's `MAX_PARAMS`: a CSI with this many separators is ignored.
const MAX_PARAMS: usize = 32;

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
            if matches!(self.state, State::DcsPass | State::OscPass | State::OscDrop) {
                // Bulk-forward (or discard) long strings: sixel, tmux passthrough, OSC.
                let end = bytes[index..]
                    .iter()
                    .position(|&byte| match self.state {
                        State::DcsPass => matches!(byte, ESC | 0x18 | 0x1a | 0x9c),
                        _ => matches!(byte, ESC | 0x18 | 0x1a | 0x07),
                    })
                    .map_or(bytes.len(), |offset| index + offset);
                if end > index {
                    if self.state != State::OscDrop {
                        sink(Out::Bytes(&bytes[index..end]));
                    }
                    index = end;
                    continue;
                }
            }
            let byte = bytes[index];
            index += 1;
            self.byte(byte, sink);
        }
    }

    /// Forward a held, unfinished sequence (end of stream).
    pub fn flush(&mut self, sink: &mut impl FnMut(Out<'_>)) {
        self.pass(sink);
    }

    fn begin_escape(&mut self) {
        self.reset_sequence();
        self.held.push(ESC);
        self.state = State::Escape;
    }

    fn reset_sequence(&mut self) {
        self.held.clear();
        self.overflow = false;
        self.marker = None;
        self.started = false;
        self.values.clear();
        self.current = 0;
        self.field_done = false;
        self.separators = 0;
        self.intermediates = 0;
        self.intermediate = None;
        self.ignored = false;
        self.payload.clear();
    }

    fn hold(&mut self, byte: u8) {
        if self.overflow {
            return;
        }
        if self.held.len() >= MAX_HELD {
            // Never forward bytes that were not examined: drop the sequence.
            self.overflow = true;
            self.held = Vec::new();
            return;
        }
        self.held.push(byte);
    }

    /// Forward the held sequence unchanged (unless oversized) and return to ground.
    fn pass(&mut self, sink: &mut impl FnMut(Out<'_>)) {
        if !self.overflow && !self.held.is_empty() {
            sink(Out::Bytes(&self.held));
        }
        self.reset_sequence();
        self.state = State::Ground;
    }

    /// Drop the held sequence; its C0 controls were already forwarded.
    fn drop_sequence(&mut self) {
        self.reset_sequence();
        self.state = State::Ground;
    }

    fn end_field(&mut self) {
        if !self.field_done {
            if self.values.len() <= MAX_PARAMS {
                self.values.push(self.current);
            }
            self.field_done = true;
        }
        self.current = 0;
    }

    fn byte(&mut self, byte: u8, sink: &mut impl FnMut(Out<'_>)) {
        match byte {
            ESC => {
                // ESC terminates a DCS string; the parser then reads the ST.
                let query = (self.state == State::Dcs)
                    .then(|| self.dcs_query())
                    .flatten();
                if self.state == State::OscDrop {
                    self.drop_sequence();
                }
                self.pass(sink);
                if let Some(query) = query {
                    sink(Out::Query(query));
                }
                self.begin_escape();
                return;
            }
            0x18 | 0x1a => {
                if self.state == State::OscDrop {
                    // The parser executes CAN/SUB after ending the string.
                    self.drop_sequence();
                    sink(Out::Bytes(&[byte]));
                    return;
                }
                self.hold(byte);
                self.pass(sink);
                return;
            }
            // 8-bit ST ends a DCS string in the parser (UTF-8 mode keeps it).
            0x9c if matches!(self.state, State::Dcs | State::DcsPass) => {
                let query = (self.state == State::Dcs)
                    .then(|| self.dcs_query())
                    .flatten();
                if self.state == State::DcsPass {
                    sink(Out::Bytes(&[byte]));
                } else {
                    self.hold(byte);
                }
                self.pass(sink);
                if let Some(query) = query {
                    sink(Out::Query(query));
                }
                return;
            }
            _ if self.state == State::OscPass => {
                sink(Out::Bytes(&[byte]));
                if byte == 0x07 {
                    self.reset_sequence();
                    self.state = State::Ground;
                }
                return;
            }
            _ if self.state == State::OscDrop => {
                if byte == 0x07 {
                    self.drop_sequence();
                }
                return;
            }
            _ if self.state == State::Osc52 => {
                if byte == 0x07 {
                    self.held.push(byte);
                    self.pass(sink);
                } else if self.held.len() >= MAX_OSC52_BYTES {
                    self.held = Vec::new();
                    self.state = State::OscDrop;
                } else {
                    self.held.push(byte);
                }
                return;
            }
            _ if self.state == State::Osc => {
                self.hold(byte);
                if byte == 0x07 {
                    self.pass(sink);
                    return;
                }
                // C0 controls inside an OSC string are ignored by the parser.
                if matches!(byte, 0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f) {
                    return;
                }
                self.payload.push(byte);
                if self.payload == b"52;" {
                    self.state = State::Osc52;
                } else if !b"52;".starts_with(&self.payload) {
                    sink(Out::Bytes(&self.held));
                    self.reset_sequence();
                    self.state = State::OscPass;
                }
                return;
            }
            // Inside a DCS string the parser does not execute C0 controls.
            _ if self.state == State::DcsPass => {
                sink(Out::Bytes(&[byte]));
                return;
            }
            _ if self.state == State::Dcs => {
                self.hold(byte);
                self.payload.push(byte);
                let candidate = self.payload.len() > 2
                    || matches!(self.payload.as_slice(), [b'+' | b'$'] | b"+q" | b"$q");
                if !candidate || self.payload.len() > MAX_DCS_QUERY {
                    if !self.overflow {
                        sink(Out::Bytes(&self.held));
                    }
                    self.reset_sequence();
                    self.state = State::DcsPass;
                }
                return;
            }
            // Executed by the parser in every escape/CSI state.
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                sink(Out::Bytes(&[byte]));
                return;
            }
            _ => {}
        }
        match self.state {
            State::Ground
            | State::Dcs
            | State::DcsPass
            | State::Osc
            | State::Osc52
            | State::OscPass
            | State::OscDrop => {
                unreachable!("ground and DCS bytes are handled above")
            }
            State::Escape => match byte {
                // The parser ignores DEL and bytes >= 0x80 after ESC and stays
                // in its escape state: the next byte still starts the sequence.
                0x7f..=0xff => self.hold(byte),
                b'[' => {
                    self.hold(byte);
                    self.state = State::Csi;
                }
                b']' => {
                    self.hold(byte);
                    self.state = State::Osc;
                }
                b'P' => {
                    self.hold(byte);
                    self.state = State::Dcs;
                }
                b'c' => {
                    // RIS: the parser resets screens, keyboard stacks and modes.
                    self.hold(byte);
                    self.alt_screen = false;
                    self.kitty_depth = [0; 2];
                    self.modify_other_keys = 0;
                    self.color_scheme_updates = false;
                    self.pass(sink);
                }
                _ => {
                    self.hold(byte);
                    self.pass(sink);
                }
            },
            State::Csi => {
                self.hold(byte);
                match byte {
                    b'<'..=b'?' => {
                        if self.marker.is_none() && !self.started && self.intermediates == 0 {
                            self.marker = Some(byte);
                        } else {
                            self.ignored = true;
                        }
                    }
                    b'0'..=b';' if self.intermediates > 0 => self.ignored = true,
                    b'0'..=b'9' => {
                        self.started = true;
                        if !self.field_done {
                            self.current = self
                                .current
                                .saturating_mul(10)
                                .saturating_add(u16::from(byte - b'0'));
                        }
                    }
                    b':' => {
                        self.started = true;
                        self.separators += 1;
                        self.end_field();
                    }
                    b';' => {
                        self.started = true;
                        self.separators += 1;
                        self.end_field();
                        self.field_done = false;
                    }
                    0x20..=0x2f => {
                        self.intermediates += 1;
                        self.intermediate = Some(byte);
                    }
                    0x40..=0x7e => {
                        self.end_field();
                        self.dispatch(byte, sink);
                    }
                    _ => {}
                }
            }
        }
    }

    fn dcs_query(&self) -> Option<Query> {
        let body = self.payload.get(2..)?;
        match self.payload.get(..2)? {
            b"+q" => Some(Query::Termcap(
                body.split(|&byte| byte == b';')
                    .map(|name| String::from_utf8_lossy(name).into_owned())
                    .collect(),
            )),
            b"$q" => Some(Query::StatusString(body.to_vec())),
            _ => None,
        }
    }

    fn dispatch(&mut self, final_byte: u8, sink: &mut impl FnMut(Out<'_>)) {
        if self.overflow {
            self.drop_sequence();
            return;
        }
        // The parser answers DA2 for `CSI > c` with up to one extra
        // intermediate (the marker counts as one of its two).
        if !self.ignored
            && self.marker == Some(b'>')
            && final_byte == b'c'
            && self.intermediates <= 1
            && self.separators < MAX_PARAMS
        {
            self.drop_sequence();
            sink(Out::Query(Query::SecondaryAttributes));
            return;
        }
        // DECRQM for mode 2031, which the parser does not know (it would
        // answer "not recognized").
        if !self.ignored
            && self.marker == Some(b'?')
            && self.intermediates == 1
            && self.intermediate == Some(b'$')
            && final_byte == b'p'
            && self.values.as_slice() == [2031]
        {
            self.drop_sequence();
            sink(Out::Query(Query::ColorSchemeUpdatesMode(
                self.color_scheme_updates,
            )));
            return;
        }
        if self.ignored || self.intermediates > 0 || self.separators >= MAX_PARAMS {
            self.pass(sink);
            return;
        }
        let params = std::mem::take(&mut self.values);
        let first = params.first().copied().unwrap_or(0);
        let screen = usize::from(self.alt_screen);
        match (self.marker, final_byte) {
            (Some(b'>'), b'u') => {
                if self.kitty_depth[screen] >= KITTY_STACK_LIMIT {
                    self.drop_sequence();
                    return;
                }
                self.kitty_depth[screen] += 1;
            }
            (Some(b'<'), b'u') => {
                let count = if first == 0 { 1 } else { first };
                self.kitty_depth[screen] = self.kitty_depth[screen].saturating_sub(count);
            }
            (Some(b'?'), b'h' | b'l') if params.contains(&1049) || params.contains(&2031) => {
                if params.contains(&1049) {
                    self.alt_screen = final_byte == b'h';
                }
                if params.contains(&2031) {
                    self.color_scheme_updates = final_byte == b'h';
                }
            }
            (Some(b'?'), b'n') if first == 996 && params.len() == 1 => {
                self.drop_sequence();
                sink(Out::Query(Query::ColorScheme));
                return;
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
            (Some(b'>'), b'q') if first == 0 && params.len() <= 1 => {
                self.pass(sink);
                sink(Out::Query(Query::Version));
                return;
            }
            (None, b't') if first == 16 && params.len() == 1 => {
                self.pass(sink);
                sink(Out::Query(Query::CellSize));
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
        let input = b"plain \x1b[1;31mred\x1b[0m \x1b]2;title\x07 \x1bPq#0;2\x1b\\ \xe7\x95\x8c\x1b(B\x1b[?1049h\x1b[?1049l";
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
    fn color_scheme_query_and_mode_2031_are_tracked_across_splits() {
        let input: &[u8] =
            b"\x1b[?2031$p\x1b[?996n\x1b[?2031h\x1b[?2031$p\x1b[?2031l\x1b[?2031$pdone";
        for split in 0..input.len() {
            let mut interceptor = Interceptor::default();
            let (fed, queries) = run(&mut interceptor, &[&input[..split], &input[split..]]);
            assert_eq!(
                queries,
                vec![
                    Query::ColorSchemeUpdatesMode(false),
                    Query::ColorScheme,
                    Query::ColorSchemeUpdatesMode(true),
                    Query::ColorSchemeUpdatesMode(false),
                ],
                "split at {split}"
            );
            // Queries are answered here, never by the parser; the mode
            // switches still reach it.
            assert_eq!(fed, b"\x1b[?2031h\x1b[?2031ldone", "split at {split}");
        }
        let mut interceptor = Interceptor::default();
        run(&mut interceptor, &[b"\x1b[?1049;2031h"]);
        assert!(interceptor.color_scheme_updates);
        // RIS resets the mode like the parser resets its own.
        run(&mut interceptor, &[b"\x1bc"]);
        assert!(!interceptor.color_scheme_updates);
        // Other DECRQM and DSR queries are left to the parser.
        let (fed, queries) = run(&mut Interceptor::default(), &[b"\x1b[?25$p\x1b[?6n"]);
        assert!(queries.is_empty());
        assert_eq!(fed, b"\x1b[?25$p\x1b[?6n");
    }

    #[test]
    fn device_queries_are_recognised_in_order_across_splits() {
        let input: &[u8] =
            b"\x1b[>q\x1bP+q5463;536d756c78\x1b\\\x1b[>0c\x1bP$q m\x1b\\\x1b[16t\x1b[c";
        let expected = vec![
            Query::Version,
            Query::Termcap(vec!["5463".into(), "536d756c78".into()]),
            Query::SecondaryAttributes,
            Query::StatusString(b" m".to_vec()),
            Query::CellSize,
        ];
        let da2 = input.windows(5).position(|w| w == b"\x1b[>0c").unwrap();
        let mut forwarded = input.to_vec();
        forwarded.drain(da2..da2 + 5);
        for split in 0..input.len() {
            let mut interceptor = Interceptor::default();
            let (fed, queries) = run(&mut interceptor, &[&input[..split], &input[split..]]);
            assert_eq!(queries, expected, "split at {split}");
            // Only DA2 is withheld from the parser.
            assert_eq!(fed, forwarded, "split at {split}");
        }
    }

    #[test]
    fn other_dcs_strings_pass_through_without_buffering_or_queries() {
        let mut interceptor = Interceptor::default();
        let mut sixel = b"\x1bPq#0;2;0;0;0\x07".to_vec();
        sixel.extend(std::iter::repeat_n(b'~', 100_000));
        sixel.extend_from_slice(b"\x1b\\done");
        let (fed, queries) = run(&mut interceptor, &[&sixel[..50_000], &sixel[50_000..]]);
        assert_eq!(fed, sixel);
        assert!(queries.is_empty());
        assert!(interceptor.held.is_empty());
        // C0 inside a query string stays inside it (the parser does not ring).
        let (fed, queries) = run(&mut interceptor, &[b"\x1bP+q\x0754\x1b\\"]);
        assert_eq!(fed, b"\x1bP+q\x0754\x1b\\");
        assert_eq!(queries, vec![Query::Termcap(vec!["\x0754".into()])]);
        // CAN aborts a query string: no answer.
        let (fed, queries) = run(&mut interceptor, &[b"\x1bP+q5463\x18x"]);
        assert_eq!(fed, b"\x1bP+q5463\x18x");
        assert!(queries.is_empty());
    }

    #[test]
    fn oversized_osc52_is_discarded_before_the_parser_decodes_it() {
        let mut interceptor = Interceptor::default();
        let small = b"\x1b]52;c;aGk=\x07after";
        for split in 0..small.len() {
            let mut interceptor = Interceptor::default();
            let (fed, _) = run(&mut interceptor, &[&small[..split], &small[split..]]);
            assert_eq!(fed, small, "split at {split}");
        }
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(std::iter::repeat_n(b'A', MAX_OSC52_BYTES));
        big.extend_from_slice(b"\x1b\\after\x1b]52;c;");
        big.extend(std::iter::repeat_n(b'A', MAX_OSC52_BYTES));
        big.extend_from_slice(b"\x18x");
        let (fed, _) = run(&mut interceptor, &[&big[..1000], &big[1000..]]);
        assert_eq!(fed, b"\x1b\\after\x18x");
        assert!(interceptor.held.capacity() < MAX_OSC52_BYTES * 2);
        // Other OSC strings pass through in bulk, also across terminators.
        let osc = b"\x1b]2;title\x07\x1b]8;;https://x\x1b\\\x1b]11;?\x18done";
        let (fed, _) = run(&mut interceptor, &[osc]);
        assert_eq!(fed, osc);
    }

    #[test]
    fn re_review_osc52_c0_in_number_and_da2_intermediate() {
        let mut interceptor = Interceptor::default();
        let mut big = b"\x1b]5\x012;c;".to_vec();
        big.extend(std::iter::repeat_n(b'A', MAX_OSC52_BYTES));
        big.extend_from_slice(b"\x07after");
        let (fed, _) = run(&mut interceptor, &[&big]);
        assert_eq!(fed, b"after");
        let (fed, queries) = run(&mut interceptor, &[b"\x1b[> c\x1b[>0;1 c\x1b[> !c"]);
        assert_eq!(
            queries,
            vec![Query::SecondaryAttributes, Query::SecondaryAttributes]
        );
        // Two extra intermediates: the parser ignores it, so it passes through.
        assert_eq!(fed, b"\x1b[> !c");
    }

    #[test]
    fn eight_bit_st_ends_dcs_like_the_parser() {
        let mut interceptor = Interceptor::default();
        let (fed, queries) = run(&mut interceptor, &[b"\x1bP$qm\x9chello\n"]);
        assert_eq!(fed, b"\x1bP$qm\x9chello\n");
        assert_eq!(queries, vec![Query::StatusString(b"m".to_vec())]);
        let (fed, _) = run(&mut interceptor, &[b"\x1bPq#0\x9cplain"]);
        assert_eq!(fed, b"\x1bPq#0\x9cplain");
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
    fn oversized_sequences_are_dropped_unexamined_bytes_never_reach_the_parser() {
        let mut interceptor = Interceptor::default();
        let mut input = b"\x1b[>".to_vec();
        input.extend(std::iter::repeat_n(b'0', MAX_HELD + 10));
        input.extend_from_slice(b"1\nu after");
        let (fed, _) = run(&mut interceptor, &[&input]);
        assert_eq!(fed, b"\n after");
        assert!(interceptor.held.capacity() <= MAX_HELD + 64);
    }

    /// Shadow of alacritty's keyboard stacks, driven by the real vte parser
    /// over exactly the bytes the scanner forwards.
    #[derive(Default)]
    struct Shadow {
        alt: bool,
        depth: [usize; 2],
        max: usize,
    }
    impl alacritty_terminal::vte::Perform for Shadow {
        fn csi_dispatch(
            &mut self,
            params: &alacritty_terminal::vte::Params,
            intermediates: &[u8],
            ignore: bool,
            action: char,
        ) {
            if ignore || intermediates.len() > 2 {
                return;
            }
            let mut iter = params.iter();
            let screen = usize::from(self.alt);
            match (action, intermediates) {
                ('u', [b'>']) => {
                    self.depth[screen] += 1;
                    self.max = self.max.max(self.depth[screen]);
                }
                ('u', [b'<']) => {
                    let count = match iter.next() {
                        Some(&[value, ..]) if value != 0 => value,
                        _ => 1,
                    };
                    self.depth[screen] = self.depth[screen].saturating_sub(count as usize);
                }
                ('h' | 'l', [b'?']) => {
                    for param in params.iter() {
                        if param[0] == 1049 {
                            self.alt = action == 'h';
                        }
                    }
                }
                _ => {}
            }
        }
        fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
            if byte == b'c' && intermediates.is_empty() {
                *self = Self {
                    max: self.max,
                    ..Self::default()
                };
            }
        }
    }

    #[test]
    fn forwarded_stream_never_exceeds_the_push_bound_under_hostile_input() {
        let tokens: &[&[u8]] = &[
            b"\x1b[>1u",
            b"\x1b[>31u",
            b"\x1b[<u",
            b"\x1b[<255u",
            b"\x1b[<0u",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[?1;1049h",
            b"\x1b[?000000000000000000000000000000000000000000001049h",
            b"\x1b[<255;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1u",
            b"\x1b[<255;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1u",
            b"\x1b[<255:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1:1u",
            b"\x1b[<2:9u",
            b"\x1b[>1\x07u",
            b"\x1b[>\x18u",
            b"\x1b\x80[>1u",
            b"\x1b\x9c[>1u",
            b"\x1b\xc3\xa9[>1u",
            b"\x1b\x7f\xff[>1u",
            b"\x1b[>1$u",
            b"\x1b[>>1u",
            b"\x1b[1>u",
            b"\x1bc",
            b"\x1b]2;\x1b[<9u\x07",
            b"\x1bP+q\x1b[<9u\x1b\\",
            b"\x1b[?2026h",
            b"\x1b[?2026l",
            b"\x1b",
            b"\x7f",
            b"x",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Targeted: fill the stack, then try each token as a "free capacity"
        // trick, repeatedly (a miscounted pop or screen switch shows here).
        let push: &[u8] = b"\x1b[>1u";
        for trick in tokens {
            let mut interceptor = Interceptor::default();
            let mut parser = alacritty_terminal::vte::Parser::new();
            let mut shadow = Shadow::default();
            for _ in 0..20 {
                let mut chunk = push.repeat(KITTY_STACK_LIMIT as usize - 1);
                chunk.extend_from_slice(trick);
                let split = (next() as usize) % (chunk.len() + 1);
                for part in [&chunk[..split], &chunk[split..]] {
                    interceptor.scan(part, &mut |out| {
                        if let Out::Bytes(bytes) = out {
                            parser.advance(&mut shadow, bytes);
                        }
                    });
                }
            }
            assert!(
                shadow.max <= KITTY_STACK_LIMIT as usize,
                "{:?}: parser stack reached {}",
                String::from_utf8_lossy(trick),
                shadow.max
            );
        }
        // Oversized push padding (past the hold limit) must not count as free.
        let mut padded = b"\x1b[>".to_vec();
        padded.extend(std::iter::repeat_n(b'0', MAX_HELD + 1));
        padded.extend_from_slice(b"1u");
        let mut interceptor = Interceptor::default();
        let mut parser = alacritty_terminal::vte::Parser::new();
        let mut shadow = Shadow::default();
        for _ in 0..(KITTY_STACK_LIMIT as usize + 5) {
            interceptor.scan(&padded, &mut |out| {
                if let Out::Bytes(bytes) = out {
                    parser.advance(&mut shadow, bytes);
                }
            });
        }
        assert!(shadow.max <= KITTY_STACK_LIMIT as usize);
        for _ in 0..40 {
            let mut interceptor = Interceptor::default();
            let mut parser = alacritty_terminal::vte::Parser::new();
            let mut shadow = Shadow::default();
            for _ in 0..4000 {
                let mut chunk = Vec::new();
                for _ in 0..(next() % 8 + 1) {
                    chunk.extend_from_slice(tokens[(next() % tokens.len() as u64) as usize]);
                }
                let split = (next() as usize) % (chunk.len() + 1);
                for part in [&chunk[..split], &chunk[split..]] {
                    interceptor.scan(part, &mut |out| {
                        if let Out::Bytes(bytes) = out {
                            parser.advance(&mut shadow, bytes);
                        }
                    });
                }
                assert!(
                    shadow.max <= KITTY_STACK_LIMIT as usize,
                    "parser stack reached {}",
                    shadow.max
                );
            }
        }
    }
}
