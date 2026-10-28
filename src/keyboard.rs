//! Keyboard encodings beyond the legacy xterm bytes: the kitty keyboard
//! protocol (progressive enhancement) and xterm modifyOtherKeys.
//!
//! The kitty encoder follows the reference implementation's decision order
//! (kitty `key_encoding.c`): text-producing keys are sent as text unless "report
//! all keys as escape codes" is active; Enter/Tab/Backspace without modifiers stay
//! legacy bytes; everything else becomes `CSI key[:shifted[:base]] ; mods[:event]
//! [; text] u` or the `CSI 1;mods {ABCDEFHPQS}` / `CSI n ~` functional forms.
//! Lock modifiers (caps/num lock) are not reported: winit does not expose them.
use winit::keyboard::{Key, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey};

pub const DISAMBIGUATE: u8 = 1;
pub const EVENT_TYPES: u8 = 2;
pub const ALTERNATE_KEYS: u8 = 4;
pub const ALL_AS_ESCAPES: u8 = 8;
pub const ASSOCIATED_TEXT: u8 = 16;

const SHIFT: u32 = 1;
const ALT: u32 = 2;
const CTRL: u32 = 4;
const SUPER: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Repeat,
    Release,
}

/// One native key event, independent of the window system types that own it.
#[derive(Debug, Clone, Copy)]
pub struct KeyInput<'a> {
    /// Logical key including modifiers (e.g. `A` for shift+a).
    pub key: &'a Key,
    /// Logical key without modifiers (e.g. `a` for shift+a).
    pub base: &'a Key,
    /// Text the event produces, if any.
    pub text: Option<&'a str>,
    pub physical: PhysicalKey,
    pub location: KeyLocation,
    pub mods: ModifiersState,
    pub action: KeyAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Code {
    /// A text key: unshifted Unicode codepoint.
    Text(u32),
    /// A functional key: kitty number and CSI final byte.
    Functional(u32, u8),
    /// Enter/Tab/Backspace/Escape: legacy bytes unless disambiguated.
    Special(u32),
    /// Shift/Ctrl/Alt/Super and lock keys: only reported as escapes.
    Modifier(u32, u32),
}

fn single_char(key: &Key) -> Option<char> {
    let Key::Character(text) = key else {
        return None;
    };
    let mut chars = text.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

fn lowercase(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

fn keypad_code(physical: PhysicalKey, key: &Key, location: KeyLocation) -> Option<u32> {
    let code = match physical {
        PhysicalKey::Code(code) => code,
        _ => return None,
    };
    let number = match code {
        KeyCode::Numpad0 => 57399,
        KeyCode::Numpad1 => 57400,
        KeyCode::Numpad2 => 57401,
        KeyCode::Numpad3 => 57402,
        KeyCode::Numpad4 => 57403,
        KeyCode::Numpad5 => 57404,
        KeyCode::Numpad6 => 57405,
        KeyCode::Numpad7 => 57406,
        KeyCode::Numpad8 => 57407,
        KeyCode::Numpad9 => 57408,
        KeyCode::NumpadDecimal => 57409,
        KeyCode::NumpadDivide => 57410,
        KeyCode::NumpadMultiply => 57411,
        KeyCode::NumpadSubtract => 57412,
        KeyCode::NumpadAdd => 57413,
        KeyCode::NumpadEnter => 57414,
        KeyCode::NumpadEqual => 57415,
        KeyCode::NumpadComma => 57416,
        _ => return None,
    };
    // With num lock off the digit keys act as navigation keys.
    if location == KeyLocation::Numpad
        && let Key::Named(named) = key
    {
        let navigation = match named {
            NamedKey::ArrowLeft => Some(57417),
            NamedKey::ArrowRight => Some(57418),
            NamedKey::ArrowUp => Some(57419),
            NamedKey::ArrowDown => Some(57420),
            NamedKey::PageUp => Some(57421),
            NamedKey::PageDown => Some(57422),
            NamedKey::Home => Some(57423),
            NamedKey::End => Some(57424),
            NamedKey::Insert => Some(57425),
            NamedKey::Delete => Some(57426),
            _ => None,
        };
        if navigation.is_some() {
            return navigation;
        }
        if *named == NamedKey::Clear {
            return None;
        }
    }
    Some(number)
}

fn classify(input: &KeyInput, keypad: bool) -> Option<Code> {
    // Without disambiguation keypad keys report as their ordinary equivalents.
    if keypad && let Some(number) = keypad_code(input.physical, input.key, input.location) {
        return Some(Code::Functional(number, b'u'));
    }
    if input.location == KeyLocation::Numpad && input.key == &Key::Named(NamedKey::Clear) {
        return Some(Code::Functional(1, b'E'));
    }
    let right = input.location == KeyLocation::Right;
    let named = match input.key {
        Key::Named(named) => *named,
        Key::Character(_) => {
            let c = single_char(input.base).or_else(|| single_char(input.key))?;
            return Some(Code::Text(lowercase(c) as u32));
        }
        _ => return None,
    };
    Some(match named {
        NamedKey::Escape => Code::Special(27),
        NamedKey::Enter => Code::Special(13),
        NamedKey::Tab => Code::Special(9),
        NamedKey::Backspace => Code::Special(127),
        NamedKey::Space => Code::Text(32),
        NamedKey::Insert => Code::Functional(2, b'~'),
        NamedKey::Delete => Code::Functional(3, b'~'),
        NamedKey::ArrowLeft => Code::Functional(1, b'D'),
        NamedKey::ArrowRight => Code::Functional(1, b'C'),
        NamedKey::ArrowUp => Code::Functional(1, b'A'),
        NamedKey::ArrowDown => Code::Functional(1, b'B'),
        NamedKey::PageUp => Code::Functional(5, b'~'),
        NamedKey::PageDown => Code::Functional(6, b'~'),
        NamedKey::Home => Code::Functional(1, b'H'),
        NamedKey::End => Code::Functional(1, b'F'),
        NamedKey::F1 => Code::Functional(1, b'P'),
        NamedKey::F2 => Code::Functional(1, b'Q'),
        NamedKey::F3 => Code::Functional(13, b'~'),
        NamedKey::F4 => Code::Functional(1, b'S'),
        NamedKey::F5 => Code::Functional(15, b'~'),
        NamedKey::F6 => Code::Functional(17, b'~'),
        NamedKey::F7 => Code::Functional(18, b'~'),
        NamedKey::F8 => Code::Functional(19, b'~'),
        NamedKey::F9 => Code::Functional(20, b'~'),
        NamedKey::F10 => Code::Functional(21, b'~'),
        NamedKey::F11 => Code::Functional(23, b'~'),
        NamedKey::F12 => Code::Functional(24, b'~'),
        NamedKey::F13 => Code::Functional(57376, b'u'),
        NamedKey::F14 => Code::Functional(57377, b'u'),
        NamedKey::F15 => Code::Functional(57378, b'u'),
        NamedKey::F16 => Code::Functional(57379, b'u'),
        NamedKey::F17 => Code::Functional(57380, b'u'),
        NamedKey::F18 => Code::Functional(57381, b'u'),
        NamedKey::F19 => Code::Functional(57382, b'u'),
        NamedKey::F20 => Code::Functional(57383, b'u'),
        NamedKey::CapsLock => Code::Modifier(57358, 0),
        NamedKey::ScrollLock => Code::Modifier(57359, 0),
        NamedKey::NumLock => Code::Modifier(57360, 0),
        NamedKey::PrintScreen => Code::Functional(57361, b'u'),
        NamedKey::Pause => Code::Functional(57362, b'u'),
        NamedKey::ContextMenu => Code::Functional(57363, b'u'),
        NamedKey::Shift => Code::Modifier(if right { 57447 } else { 57441 }, SHIFT),
        NamedKey::Control => Code::Modifier(if right { 57448 } else { 57442 }, CTRL),
        NamedKey::Alt => Code::Modifier(if right { 57449 } else { 57443 }, ALT),
        NamedKey::Super | NamedKey::Meta => {
            Code::Modifier(if right { 57450 } else { 57444 }, SUPER)
        }
        _ => return None,
    })
}

fn modifier_bits(mods: ModifiersState) -> u32 {
    (u32::from(mods.shift_key()) * SHIFT)
        | (u32::from(mods.alt_key()) * ALT)
        | (u32::from(mods.control_key()) * CTRL)
        | (u32::from(mods.super_key()) * SUPER)
}

/// US base-layout key of a physical key, for the alternate-keys enhancement.
fn base_layout(physical: PhysicalKey) -> Option<char> {
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    let letters = [
        KeyCode::KeyA,
        KeyCode::KeyB,
        KeyCode::KeyC,
        KeyCode::KeyD,
        KeyCode::KeyE,
        KeyCode::KeyF,
        KeyCode::KeyG,
        KeyCode::KeyH,
        KeyCode::KeyI,
        KeyCode::KeyJ,
        KeyCode::KeyK,
        KeyCode::KeyL,
        KeyCode::KeyM,
        KeyCode::KeyN,
        KeyCode::KeyO,
        KeyCode::KeyP,
        KeyCode::KeyQ,
        KeyCode::KeyR,
        KeyCode::KeyS,
        KeyCode::KeyT,
        KeyCode::KeyU,
        KeyCode::KeyV,
        KeyCode::KeyW,
        KeyCode::KeyX,
        KeyCode::KeyY,
        KeyCode::KeyZ,
    ];
    if let Some(index) = letters.iter().position(|letter| *letter == code) {
        return Some((b'a' + index as u8) as char);
    }
    let digits = [
        KeyCode::Digit0,
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    if let Some(index) = digits.iter().position(|digit| *digit == code) {
        return Some((b'0' + index as u8) as char);
    }
    Some(match code {
        KeyCode::Minus => '-',
        KeyCode::Equal => '=',
        KeyCode::BracketLeft => '[',
        KeyCode::BracketRight => ']',
        KeyCode::Backslash => '\\',
        KeyCode::Semicolon => ';',
        KeyCode::Quote => '\'',
        KeyCode::Backquote => '`',
        KeyCode::Comma => ',',
        KeyCode::Period => '.',
        KeyCode::Slash => '/',
        KeyCode::Space => ' ',
        _ => return None,
    })
}

struct Fields {
    key: u32,
    shifted: u32,
    alternate: u32,
    mods: u32,
    event: Option<u32>,
    text: Option<String>,
}

fn serialize(fields: &Fields, final_byte: u8) -> Vec<u8> {
    let has_mods = fields.mods != 0;
    let second = has_mods || fields.event.is_some();
    let alternates = fields.shifted != 0 || fields.alternate != 0;
    let mut out = String::from("\x1b[");
    if fields.key != 1 || alternates || second || fields.text.is_some() {
        out.push_str(&fields.key.to_string());
    }
    if alternates {
        out.push(':');
        if fields.shifted != 0 {
            out.push_str(&fields.shifted.to_string());
        }
        if fields.alternate != 0 {
            out.push(':');
            out.push_str(&fields.alternate.to_string());
        }
    }
    if second || fields.text.is_some() {
        out.push(';');
        if second {
            out.push_str(&(fields.mods + 1).to_string());
        }
        if let Some(event) = fields.event {
            out.push(':');
            out.push_str(&event.to_string());
        }
    }
    if let Some(text) = &fields.text {
        for (index, c) in text.chars().enumerate() {
            out.push(if index == 0 { ';' } else { ':' });
            out.push_str(&(c as u32).to_string());
        }
    }
    out.push(final_byte as char);
    out.into_bytes()
}

/// Encode one key event under the kitty keyboard protocol with `flags` (non-zero).
pub fn encode_kitty(input: &KeyInput, flags: u8, application_cursor: bool) -> Vec<u8> {
    let disambiguate = flags & DISAMBIGUATE != 0;
    let report_events = flags & EVENT_TYPES != 0;
    let report_alternates = flags & ALTERNATE_KEYS != 0;
    let all_as_escapes = flags & ALL_AS_ESCAPES != 0;
    let embed_text = flags & ASSOCIATED_TEXT != 0;
    let Some(code) = classify(input, disambiguate || all_as_escapes) else {
        return Vec::new();
    };
    let mut mods = modifier_bits(input.mods);
    if let Code::Modifier(_, bit) = code {
        if !all_as_escapes {
            return Vec::new();
        }
        // The modifier's own bit reflects the state including this event.
        if input.action == KeyAction::Release {
            mods &= !bit;
        } else {
            mods |= bit;
        }
    }
    // Text is what the key types; control/alt/super chords type no text.
    let text = input
        .text
        .filter(|text| text.chars().next().is_some_and(|c| !c.is_control()))
        .filter(|_| mods & (CTRL | ALT | SUPER) == 0);
    if input.action == KeyAction::Release && !report_events {
        return Vec::new();
    }
    if !all_as_escapes
        && let Some(text) = text
        && input.action != KeyAction::Release
    {
        return text.as_bytes().to_vec();
    }
    let event = (report_events && input.action != KeyAction::Press).then_some(match input.action {
        KeyAction::Repeat => 2,
        _ => 3,
    });
    let legacy_mode = !report_events && !disambiguate && !all_as_escapes;
    match code {
        Code::Special(number) | Code::Functional(number, _) | Code::Modifier(number, _) => {
            let final_byte = match code {
                Code::Functional(_, final_byte) => final_byte,
                _ => b'u',
            };
            let release = input.action == KeyAction::Release;
            if legacy_mode
                && mods == 0
                && application_cursor
                && number == 1
                && matches!(final_byte, b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'H')
            {
                return format!("\x1bO{}", final_byte as char).into_bytes();
            }
            if let Code::Special(special) = code {
                if mods == 0 {
                    if special == 27 && !disambiguate && !all_as_escapes {
                        return vec![0x1b];
                    }
                    if special != 27 && !all_as_escapes {
                        return if release {
                            Vec::new()
                        } else {
                            vec![special as u8]
                        };
                    }
                } else if legacy_mode
                    && let Some(bytes) = legacy_special_with_modifiers(special, mods)
                {
                    return bytes;
                }
            }
            if legacy_mode && mods == 0 && number == 1 && matches!(final_byte, b'P' | b'Q' | b'S') {
                return format!("\x1bO{}", final_byte as char).into_bytes();
            }
            if legacy_mode && mods == 0 && input.key == &Key::Named(NamedKey::F3) {
                return b"\x1bOR".to_vec();
            }
            serialize(
                &Fields {
                    key: number,
                    shifted: 0,
                    alternate: 0,
                    mods,
                    event,
                    text: None,
                },
                final_byte,
            )
        }
        Code::Text(key) => {
            let shifted = single_char(input.key)
                .map(|c| c as u32)
                .filter(|&c| mods & SHIFT != 0 && c != key)
                .unwrap_or(0);
            let alternate = base_layout(input.physical)
                .map(|c| c as u32)
                .filter(|&c| c != key)
                .unwrap_or(0);
            let add_alternates = report_alternates && (shifted != 0 || alternate != 0);
            let embedded = (embed_text && input.action != KeyAction::Release)
                .then(|| text.map(str::to_owned))
                .flatten();
            serialize(
                &Fields {
                    key,
                    shifted: if add_alternates { shifted } else { 0 },
                    alternate: if add_alternates { alternate } else { 0 },
                    mods,
                    event,
                    text: embedded,
                },
                b'u',
            )
        }
    }
}

fn legacy_special_with_modifiers(special: u32, mods: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if mods & ALT != 0 {
        out.push(0x1b);
    }
    match special {
        13 => out.push(b'\r'),
        27 => out.push(0x1b),
        127 => out.push(if mods & CTRL != 0 { 0x08 } else { 0x7f }),
        9 if mods & SHIFT != 0 => out.extend_from_slice(b"\x1b[Z"),
        9 => out.push(b'\t'),
        _ => return None,
    }
    Some(out)
}

/// xterm modifyOtherKeys (`CSI > 4 ; level m`) for chords that the legacy
/// encoding cannot express. Returns `None` to fall back to legacy bytes.
pub fn encode_modify_other_keys(input: &KeyInput, level: u8) -> Option<Vec<u8>> {
    if level == 0 || input.action == KeyAction::Release {
        return None;
    }
    let mods = modifier_bits(input.mods) & !SUPER;
    if mods == 0 || input.mods.super_key() {
        return None;
    }
    let code = match input.key {
        Key::Named(NamedKey::Enter) => 13,
        Key::Named(NamedKey::Tab) => 9,
        Key::Named(NamedKey::Backspace) => 127,
        Key::Named(NamedKey::Escape) => 27,
        Key::Named(NamedKey::Space) => 32,
        Key::Character(_) => single_char(input.key)? as u32,
        _ => return None,
    };
    let well_defined = match code {
        // Shift alone types the shifted character; Shift+Tab is CSI Z.
        _ if mods == SHIFT => code != 13 && code != 127 && code != 32,
        // Alt alone is the ESC prefix.
        _ if mods == ALT || mods == ALT | SHIFT => true,
        9 | 13 | 27 | 127 => false,
        32 => mods & CTRL != 0 && mods & SHIFT == 0,
        c => char::from_u32(c).is_some_and(|c| ctrl_byte(lowercase(c)).is_some()),
    };
    if level == 1 && well_defined {
        return None;
    }
    if level >= 2 && mods == SHIFT && code != 13 && code != 127 && code != 32 {
        return None;
    }
    Some(format!("\x1b[27;{};{}~", mods + 1, code).into_bytes())
}

/// Legacy C0 byte for Ctrl+key (xterm/VT conventions), if one exists.
pub fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 27,
        '\\' | '4' => 28,
        ']' | '5' => 29,
        '^' | '~' | '6' => 30,
        '_' | '-' | '/' | '7' => 31,
        '?' | '8' => 127,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event<'a>(
        key: &'a Key,
        base: &'a Key,
        text: Option<&'a str>,
        code: KeyCode,
        mods: ModifiersState,
        action: KeyAction,
    ) -> KeyInput<'a> {
        KeyInput {
            key,
            base,
            text,
            physical: PhysicalKey::Code(code),
            location: KeyLocation::Standard,
            mods,
            action,
        }
    }
    fn ch(text: &str) -> Key {
        Key::Character(text.into())
    }
    fn named(key: NamedKey) -> Key {
        Key::Named(key)
    }

    const NONE: ModifiersState = ModifiersState::empty();
    const S: ModifiersState = ModifiersState::SHIFT;
    const C: ModifiersState = ModifiersState::CONTROL;
    const A: ModifiersState = ModifiersState::ALT;

    /// Reference sequences from the kitty keyboard protocol specification and
    /// kitty's `key_encoding.c`, for disambiguate (flag 1).
    #[test]
    fn disambiguate_reference_table() {
        type Case = (
            Key,
            Key,
            Option<&'static str>,
            KeyCode,
            ModifiersState,
            &'static [u8],
        );
        let cases: Vec<Case> = vec![
            (ch("a"), ch("a"), Some("a"), KeyCode::KeyA, NONE, b"a"),
            (ch("A"), ch("a"), Some("A"), KeyCode::KeyA, S, b"A"),
            (ch("a"), ch("a"), None, KeyCode::KeyA, C, b"\x1b[97;5u"),
            (ch("a"), ch("a"), None, KeyCode::KeyA, A, b"\x1b[97;3u"),
            (ch("A"), ch("a"), None, KeyCode::KeyA, C | S, b"\x1b[97;6u"),
            (ch("i"), ch("i"), None, KeyCode::KeyI, C, b"\x1b[105;5u"),
            (ch("m"), ch("m"), None, KeyCode::KeyM, C, b"\x1b[109;5u"),
            (
                ch("["),
                ch("["),
                None,
                KeyCode::BracketLeft,
                C,
                b"\x1b[91;5u",
            ),
            (ch("/"), ch("/"), None, KeyCode::Slash, C, b"\x1b[47;5u"),
            (ch("."), ch("."), None, KeyCode::Period, C, b"\x1b[46;5u"),
            (ch(";"), ch(";"), None, KeyCode::Semicolon, C, b"\x1b[59;5u"),
            (ch(","), ch(","), None, KeyCode::Comma, C, b"\x1b[44;5u"),
            (
                named(NamedKey::Tab),
                named(NamedKey::Tab),
                Some("\t"),
                KeyCode::Tab,
                NONE,
                b"\t",
            ),
            (
                named(NamedKey::Tab),
                named(NamedKey::Tab),
                None,
                KeyCode::Tab,
                C,
                b"\x1b[9;5u",
            ),
            (
                named(NamedKey::Tab),
                named(NamedKey::Tab),
                None,
                KeyCode::Tab,
                S,
                b"\x1b[9;2u",
            ),
            (
                named(NamedKey::Enter),
                named(NamedKey::Enter),
                Some("\r"),
                KeyCode::Enter,
                NONE,
                b"\r",
            ),
            (
                named(NamedKey::Enter),
                named(NamedKey::Enter),
                None,
                KeyCode::Enter,
                S,
                b"\x1b[13;2u",
            ),
            (
                named(NamedKey::Enter),
                named(NamedKey::Enter),
                None,
                KeyCode::Enter,
                C,
                b"\x1b[13;5u",
            ),
            (
                named(NamedKey::Backspace),
                named(NamedKey::Backspace),
                None,
                KeyCode::Backspace,
                NONE,
                b"\x7f",
            ),
            (
                named(NamedKey::Backspace),
                named(NamedKey::Backspace),
                None,
                KeyCode::Backspace,
                C,
                b"\x1b[127;5u",
            ),
            (
                named(NamedKey::Escape),
                named(NamedKey::Escape),
                None,
                KeyCode::Escape,
                NONE,
                b"\x1b[27u",
            ),
            (
                named(NamedKey::Space),
                named(NamedKey::Space),
                Some(" "),
                KeyCode::Space,
                NONE,
                b" ",
            ),
            (
                named(NamedKey::Space),
                named(NamedKey::Space),
                Some(" "),
                KeyCode::Space,
                S,
                b" ",
            ),
            (
                named(NamedKey::Space),
                named(NamedKey::Space),
                None,
                KeyCode::Space,
                C,
                b"\x1b[32;5u",
            ),
            (
                named(NamedKey::ArrowUp),
                named(NamedKey::ArrowUp),
                None,
                KeyCode::ArrowUp,
                NONE,
                b"\x1b[A",
            ),
            (
                named(NamedKey::ArrowUp),
                named(NamedKey::ArrowUp),
                None,
                KeyCode::ArrowUp,
                C,
                b"\x1b[1;5A",
            ),
            (
                named(NamedKey::F1),
                named(NamedKey::F1),
                None,
                KeyCode::F1,
                NONE,
                b"\x1b[P",
            ),
            (
                named(NamedKey::F3),
                named(NamedKey::F3),
                None,
                KeyCode::F3,
                NONE,
                b"\x1b[13~",
            ),
            (
                named(NamedKey::F5),
                named(NamedKey::F5),
                None,
                KeyCode::F5,
                S,
                b"\x1b[15;2~",
            ),
            (
                named(NamedKey::Delete),
                named(NamedKey::Delete),
                None,
                KeyCode::Delete,
                NONE,
                b"\x1b[3~",
            ),
            (
                named(NamedKey::Shift),
                named(NamedKey::Shift),
                None,
                KeyCode::ShiftLeft,
                S,
                b"",
            ),
        ];
        for (key, base, text, code, mods, expected) in &cases {
            let input = event(key, base, *text, *code, *mods, KeyAction::Press);
            assert_eq!(
                encode_kitty(&input, DISAMBIGUATE, false),
                *expected,
                "{key:?} {mods:?}"
            );
        }
    }

    #[test]
    fn event_types_alternates_all_keys_and_text() {
        let a = ch("a");
        let shifted = ch("A");
        // Release events only with flag 2; Enter has no release without flag 8.
        let release = event(&a, &a, Some("a"), KeyCode::KeyA, NONE, KeyAction::Release);
        assert_eq!(encode_kitty(&release, DISAMBIGUATE, false), b"");
        assert_eq!(
            encode_kitty(&release, DISAMBIGUATE | EVENT_TYPES, false),
            b"\x1b[97;1:3u"
        );
        let repeat = event(&a, &a, None, KeyCode::KeyA, C, KeyAction::Repeat);
        assert_eq!(
            encode_kitty(&repeat, DISAMBIGUATE | EVENT_TYPES, false),
            b"\x1b[97;5:2u"
        );
        let enter = named(NamedKey::Enter);
        let enter_release = event(
            &enter,
            &enter,
            None,
            KeyCode::Enter,
            NONE,
            KeyAction::Release,
        );
        assert_eq!(
            encode_kitty(&enter_release, DISAMBIGUATE | EVENT_TYPES, false),
            b""
        );
        assert_eq!(
            encode_kitty(
                &enter_release,
                DISAMBIGUATE | EVENT_TYPES | ALL_AS_ESCAPES,
                false
            ),
            b"\x1b[13;1:3u"
        );
        let up = named(NamedKey::ArrowUp);
        let up_release = event(&up, &up, None, KeyCode::ArrowUp, NONE, KeyAction::Release);
        assert_eq!(
            encode_kitty(&up_release, DISAMBIGUATE | EVENT_TYPES, false),
            b"\x1b[1;1:3A"
        );
        // Alternate keys: shifted key with shift, base-layout key if different.
        let shift_a = event(&shifted, &a, None, KeyCode::KeyA, C | S, KeyAction::Press);
        assert_eq!(
            encode_kitty(&shift_a, DISAMBIGUATE | ALTERNATE_KEYS, false),
            b"\x1b[97:65;6u"
        );
        let cyrillic = ch("ф");
        let layout = event(
            &cyrillic,
            &cyrillic,
            None,
            KeyCode::KeyA,
            C,
            KeyAction::Press,
        );
        assert_eq!(
            encode_kitty(&layout, DISAMBIGUATE | ALTERNATE_KEYS, false),
            b"\x1b[1092::97;5u"
        );
        // All keys as escapes: text keys and plain Enter/Tab/Backspace too.
        let plain = event(&a, &a, Some("a"), KeyCode::KeyA, NONE, KeyAction::Press);
        assert_eq!(encode_kitty(&plain, ALL_AS_ESCAPES, false), b"\x1b[97u");
        assert_eq!(
            encode_kitty(&plain, ALL_AS_ESCAPES | ASSOCIATED_TEXT, false),
            b"\x1b[97;;97u"
        );
        let shift_text = event(&shifted, &a, Some("A"), KeyCode::KeyA, S, KeyAction::Press);
        assert_eq!(
            encode_kitty(&shift_text, ALL_AS_ESCAPES | ASSOCIATED_TEXT, false),
            b"\x1b[97;2;65u"
        );
        let space = named(NamedKey::Space);
        let shift_space = event(
            &space,
            &space,
            Some(" "),
            KeyCode::Space,
            S,
            KeyAction::Press,
        );
        assert_eq!(
            encode_kitty(&shift_space, ALL_AS_ESCAPES, false),
            b"\x1b[32;2u"
        );
        let enter_press = event(
            &enter,
            &enter,
            Some("\r"),
            KeyCode::Enter,
            NONE,
            KeyAction::Press,
        );
        assert_eq!(
            encode_kitty(&enter_press, ALL_AS_ESCAPES, false),
            b"\x1b[13u"
        );
        // Modifier keys are reported only with flag 8, with their own bit.
        let shift = named(NamedKey::Shift);
        let mut shift_press = event(
            &shift,
            &shift,
            None,
            KeyCode::ShiftLeft,
            NONE,
            KeyAction::Press,
        );
        assert_eq!(
            encode_kitty(&shift_press, ALL_AS_ESCAPES, false),
            b"\x1b[57441;2u"
        );
        shift_press.location = KeyLocation::Right;
        shift_press.action = KeyAction::Release;
        shift_press.mods = S;
        assert_eq!(
            encode_kitty(&shift_press, ALL_AS_ESCAPES | EVENT_TYPES, false),
            b"\x1b[57447;1:3u"
        );
    }

    #[test]
    fn keypad_keys_are_distinct_when_disambiguated() {
        let enter = named(NamedKey::Enter);
        let mut input = event(
            &enter,
            &enter,
            Some("\r"),
            KeyCode::NumpadEnter,
            NONE,
            KeyAction::Press,
        );
        input.location = KeyLocation::Numpad;
        assert_eq!(encode_kitty(&input, DISAMBIGUATE, false), b"\x1b[57414u");
        let one = ch("1");
        let mut digit = event(
            &one,
            &one,
            Some("1"),
            KeyCode::Numpad1,
            NONE,
            KeyAction::Press,
        );
        digit.location = KeyLocation::Numpad;
        assert_eq!(encode_kitty(&digit, DISAMBIGUATE, false), b"1");
        let end = named(NamedKey::End);
        let mut nav = event(&end, &end, None, KeyCode::Numpad1, NONE, KeyAction::Press);
        nav.location = KeyLocation::Numpad;
        assert_eq!(encode_kitty(&nav, DISAMBIGUATE, false), b"\x1b[57424u");
    }

    #[test]
    fn modify_other_keys_levels() {
        let dot = ch(".");
        let a = ch("a");
        let enter = named(NamedKey::Enter);
        let ctrl_dot = event(&dot, &dot, None, KeyCode::Period, C, KeyAction::Press);
        let ctrl_a = event(&a, &a, None, KeyCode::KeyA, C, KeyAction::Press);
        let ctrl_enter = event(&enter, &enter, None, KeyCode::Enter, C, KeyAction::Press);
        let plain = event(&a, &a, Some("a"), KeyCode::KeyA, NONE, KeyAction::Press);
        assert_eq!(encode_modify_other_keys(&ctrl_dot, 0), None);
        assert_eq!(
            encode_modify_other_keys(&ctrl_dot, 1).unwrap(),
            b"\x1b[27;5;46~"
        );
        assert_eq!(encode_modify_other_keys(&ctrl_a, 1), None);
        assert_eq!(
            encode_modify_other_keys(&ctrl_a, 2).unwrap(),
            b"\x1b[27;5;97~"
        );
        assert_eq!(
            encode_modify_other_keys(&ctrl_enter, 1).unwrap(),
            b"\x1b[27;5;13~"
        );
        assert_eq!(encode_modify_other_keys(&plain, 2), None);
    }
}
