// src/input/keyboard.rs
//
// Pure key event -> PTY byte encoder. No I/O and no window state, so every
// rule here is unit-tested headlessly.
//
// Two encodings:
// - Legacy (xterm): what every program understands. Modifiers on special
//   keys use xterm's `CSI 1;<mods> X` / `CSI <n>;<mods> ~` forms, cursor keys
//   honour DECCKM (application cursor mode -> SS3), Ctrl folds characters
//   into C0 control codes and Alt prefixes ESC.
// - Kitty keyboard protocol
//   (https://sw.kovidgoyal.net/kitty/keyboard-protocol/): used only once a
//   program pushes flags with `CSI > flags u`. alacritty_terminal tracks the
//   flag stack and answers `CSI ? u` queries; this file produces the bytes.

use alacritty_terminal::term::TermMode;
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    Press,
    Repeat,
    Release,
}

/// Everything about one key event the encoder needs.
pub struct KeyInput<'a> {
    /// Logical key with modifiers applied (Shift+a -> "A").
    pub key: &'a Key,
    /// The same key without modifiers (Shift+a -> "a"). Kitty key codes and
    /// Ctrl combinations are defined on the unshifted key.
    pub base: &'a Key,
    /// Text the key produces, if any (includes dead-key/compose results).
    pub text: Option<&'a str>,
    pub location: KeyLocation,
    pub state: KeyState,
}

/// Terminal modes that change key encoding, read from `Term::mode()`.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyMode {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub disambiguate: bool,
    pub report_event_types: bool,
    pub report_alternate_keys: bool,
    pub report_all_keys: bool,
    pub report_associated_text: bool,
}

impl KeyMode {
    pub fn from_term(mode: TermMode) -> Self {
        Self {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            report_event_types: mode.contains(TermMode::REPORT_EVENT_TYPES),
            report_alternate_keys: mode.contains(TermMode::REPORT_ALTERNATE_KEYS),
            report_all_keys: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
            report_associated_text: mode.contains(TermMode::REPORT_ASSOCIATED_TEXT),
        }
    }

    fn kitty(&self) -> bool {
        self.disambiguate
            || self.report_event_types
            || self.report_alternate_keys
            || self.report_all_keys
            || self.report_associated_text
    }
}

pub fn encode(input: &KeyInput<'_>, mods: ModifiersState, mode: KeyMode) -> Option<Vec<u8>> {
    if mode.kitty() {
        encode_kitty(input, mods, mode)
    } else if input.state == KeyState::Release {
        None
    } else {
        encode_legacy(input, mods, mode)
    }
}

/// xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4) + super(8).
fn modifier_param(mods: ModifiersState) -> u8 {
    1 + mods.shift_key() as u8
        + 2 * mods.alt_key() as u8
        + 4 * mods.control_key() as u8
        + 8 * mods.super_key() as u8
}

/// How a special key is spelled in CSI sequences: either `CSI <n> ~` or a
/// final letter (`CSI A`, `SS3 P`, ...).
#[derive(Clone, Copy)]
enum Csi {
    Tilde(u32),
    Letter(u8),
}

fn special_key(named: NamedKey) -> Option<Csi> {
    use NamedKey::*;
    Some(match named {
        ArrowUp => Csi::Letter(b'A'),
        ArrowDown => Csi::Letter(b'B'),
        ArrowRight => Csi::Letter(b'C'),
        ArrowLeft => Csi::Letter(b'D'),
        Home => Csi::Letter(b'H'),
        End => Csi::Letter(b'F'),
        F1 => Csi::Letter(b'P'),
        F2 => Csi::Letter(b'Q'),
        F3 => Csi::Letter(b'R'),
        F4 => Csi::Letter(b'S'),
        Insert => Csi::Tilde(2),
        Delete => Csi::Tilde(3),
        PageUp => Csi::Tilde(5),
        PageDown => Csi::Tilde(6),
        F5 => Csi::Tilde(15),
        F6 => Csi::Tilde(17),
        F7 => Csi::Tilde(18),
        F8 => Csi::Tilde(19),
        F9 => Csi::Tilde(20),
        F10 => Csi::Tilde(21),
        F11 => Csi::Tilde(23),
        F12 => Csi::Tilde(24),
        F13 => Csi::Tilde(25),
        F14 => Csi::Tilde(26),
        F15 => Csi::Tilde(28),
        F16 => Csi::Tilde(29),
        F17 => Csi::Tilde(31),
        F18 => Csi::Tilde(32),
        F19 => Csi::Tilde(33),
        F20 => Csi::Tilde(34),
        _ => return None,
    })
}

fn with_alt(mods: ModifiersState, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if mods.alt_key() {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

fn encode_legacy(input: &KeyInput<'_>, mods: ModifiersState, mode: KeyMode) -> Option<Vec<u8>> {
    match input.key {
        Key::Named(named) => encode_legacy_named(*named, input, mods, mode),
        Key::Character(c) => encode_legacy_char(c, input, mods),
        _ => None,
    }
}

fn encode_legacy_named(
    named: NamedKey,
    input: &KeyInput<'_>,
    mods: ModifiersState,
    mode: KeyMode,
) -> Option<Vec<u8>> {
    let numpad = input.location == KeyLocation::Numpad;
    match named {
        NamedKey::Enter if numpad && mode.app_keypad => return Some(b"\x1bOM".to_vec()),
        NamedKey::Enter => return Some(with_alt(mods, b"\r")),
        NamedKey::Tab if mods.shift_key() => return Some(with_alt(mods, b"\x1b[Z")),
        NamedKey::Tab => return Some(with_alt(mods, b"\t")),
        NamedKey::Backspace if mods.control_key() => return Some(with_alt(mods, b"\x08")),
        NamedKey::Backspace => return Some(with_alt(mods, b"\x7f")),
        NamedKey::Escape => return Some(with_alt(mods, b"\x1b")),
        NamedKey::Space if mods.control_key() => return Some(with_alt(mods, b"\x00")),
        NamedKey::Space => return Some(with_alt(mods, b" ")),
        _ => {}
    }

    let m = modifier_param(mods);
    Some(match special_key(named)? {
        Csi::Letter(letter) if m == 1 => {
            // Cursor keys switch to SS3 in application cursor mode; F1-F4
            // are always SS3 when unmodified.
            let ss3 = matches!(letter, b'P'..=b'S') || mode.app_cursor;
            if ss3 {
                vec![0x1b, b'O', letter]
            } else {
                vec![0x1b, b'[', letter]
            }
        }
        Csi::Letter(letter) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
        Csi::Tilde(n) if m == 1 => format!("\x1b[{n}~").into_bytes(),
        Csi::Tilde(n) => format!("\x1b[{n};{m}~").into_bytes(),
    })
}

/// C0 control code for `Ctrl+<char>`, following xterm's table (Ctrl+@/Space
/// -> NUL, letters -> 1-26, `[ \ ] ^ _` -> 27-31, Ctrl+? -> DEL, and the
/// digit-row aliases 2-8 that VT220-era keyboards established).
fn ctrl_code(ch: char) -> Option<u8> {
    Some(match ch {
        'a'..='z' => ch as u8 - b'a' + 1,
        'A'..='Z' => ch as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '~' | '6' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

fn encode_legacy_char(c: &str, input: &KeyInput<'_>, mods: ModifiersState) -> Option<Vec<u8>> {
    if mods.control_key() {
        let first = c.chars().next()?;
        let code = ctrl_code(first).or_else(|| match input.base {
            // Non-Latin layouts: Ctrl+<Cyrillic letter> should still work
            // as the Latin letter in the same position would.
            Key::Character(b) => b.chars().next().and_then(ctrl_code),
            _ => None,
        });
        if let Some(code) = code {
            return Some(with_alt(mods, &[code]));
        }
    }

    let text = if mods.alt_key() || mods.control_key() {
        c
    } else {
        input.text.unwrap_or(c)
    };
    if text.is_empty() {
        return None;
    }
    Some(with_alt(mods, text.as_bytes()))
}

// ---------------------------------------------------------------------------
// Kitty keyboard protocol
// ---------------------------------------------------------------------------

/// Kitty's code for a functional key and how it terminates.
fn kitty_functional(named: NamedKey, location: KeyLocation) -> Option<(u32, u8)> {
    use NamedKey::*;
    let right = location == KeyLocation::Right;
    Some(match named {
        Escape => (27, b'u'),
        Enter if location == KeyLocation::Numpad => (57414, b'u'),
        Enter => (13, b'u'),
        Tab => (9, b'u'),
        Backspace => (127, b'u'),
        Space => (32, b'u'),
        Insert => (2, b'~'),
        Delete => (3, b'~'),
        ArrowLeft => (1, b'D'),
        ArrowRight => (1, b'C'),
        ArrowUp => (1, b'A'),
        ArrowDown => (1, b'B'),
        PageUp => (5, b'~'),
        PageDown => (6, b'~'),
        Home => (1, b'H'),
        End => (1, b'F'),
        CapsLock => (57358, b'u'),
        ScrollLock => (57359, b'u'),
        NumLock => (57360, b'u'),
        PrintScreen => (57361, b'u'),
        Pause => (57362, b'u'),
        ContextMenu => (57363, b'u'),
        F1 => (1, b'P'),
        F2 => (1, b'Q'),
        // Kitty moved F3 off `CSI R`, which collides with cursor position
        // reports.
        F3 => (13, b'~'),
        F4 => (1, b'S'),
        F5 => (15, b'~'),
        F6 => (17, b'~'),
        F7 => (18, b'~'),
        F8 => (19, b'~'),
        F9 => (20, b'~'),
        F10 => (21, b'~'),
        F11 => (23, b'~'),
        F12 => (24, b'~'),
        F13 => (57376, b'u'),
        F14 => (57377, b'u'),
        F15 => (57378, b'u'),
        F16 => (57379, b'u'),
        F17 => (57380, b'u'),
        F18 => (57381, b'u'),
        F19 => (57382, b'u'),
        F20 => (57383, b'u'),
        F21 => (57384, b'u'),
        F22 => (57385, b'u'),
        F23 => (57386, b'u'),
        F24 => (57387, b'u'),
        MediaPlay => (57428, b'u'),
        MediaPause => (57429, b'u'),
        MediaPlayPause => (57430, b'u'),
        MediaStop => (57432, b'u'),
        MediaTrackNext => (57435, b'u'),
        MediaTrackPrevious => (57436, b'u'),
        AudioVolumeDown => (57438, b'u'),
        AudioVolumeUp => (57439, b'u'),
        AudioVolumeMute => (57440, b'u'),
        Shift if right => (57447, b'u'),
        Shift => (57441, b'u'),
        Control if right => (57448, b'u'),
        Control => (57442, b'u'),
        Alt | AltGraph if right => (57449, b'u'),
        Alt | AltGraph => (57443, b'u'),
        Super | Meta if right => (57450, b'u'),
        Super | Meta => (57444, b'u'),
        Hyper if right => (57451, b'u'),
        Hyper => (57445, b'u'),
        _ => return None,
    })
}

fn kitty_keypad_char(ch: char) -> Option<u32> {
    Some(match ch {
        '0'..='9' => 57399 + (ch as u32 - '0' as u32),
        '.' | ',' => 57409,
        '/' => 57410,
        '*' => 57411,
        '-' => 57412,
        '+' => 57413,
        '=' => 57415,
        _ => return None,
    })
}

fn is_modifier_key(named: NamedKey) -> bool {
    use NamedKey::*;
    matches!(
        named,
        Shift | Control | Alt | AltGraph | Super | Meta | Hyper | CapsLock | NumLock | ScrollLock
    )
}

fn encode_kitty(input: &KeyInput<'_>, mods: ModifiersState, mode: KeyMode) -> Option<Vec<u8>> {
    // Without event-type reporting, repeats look like presses and releases
    // aren't sent at all.
    let release = input.state == KeyState::Release;
    if release && !mode.report_event_types {
        return None;
    }

    // Kitty treats Shift on its own as not changing a text key's meaning;
    // any other modifier does.
    let text_mods = mods.control_key() || mods.alt_key() || mods.super_key();
    let numpad = input.location == KeyLocation::Numpad;

    let (code, final_byte, is_text_key) = match input.key {
        Key::Named(named) => {
            if is_modifier_key(*named) && !mode.report_all_keys {
                return None;
            }
            let (code, fin) = kitty_functional(*named, input.location)?;
            (code, fin, *named == NamedKey::Space)
        }
        Key::Character(c) => {
            let base = match input.base {
                Key::Character(b) => b.as_str(),
                _ => c.as_str(),
            };
            let ch = base.chars().next()?;
            let keypad = if numpad { kitty_keypad_char(ch) } else { None };
            let code = keypad.unwrap_or_else(|| ch.to_lowercase().next().unwrap_or(ch) as u32);
            (code, b'u', keypad.is_none())
        }
        _ => return None,
    };

    // Keys that still go out as plain legacy bytes unless every key is to be
    // reported: text keys with no meaning-changing modifier, and Enter /
    // Tab / Backspace without modifiers (so `reset` stays typeable after a
    // program crashes with the protocol still enabled).
    if !mode.report_all_keys {
        let legacy_text = is_text_key && !text_mods;
        let legacy_control = matches!(code, 13 | 9 | 127)
            && final_byte == b'u'
            && !numpad
            && modifier_param(mods) == 1;
        if legacy_text || legacy_control {
            if release {
                return None;
            }
            return encode_legacy(input, mods, KeyMode::default());
        }
    }

    let m = modifier_param(mods);
    let event = match input.state {
        KeyState::Press => None,
        _ if !mode.report_event_types => None,
        KeyState::Repeat => Some(2),
        KeyState::Release => Some(3),
    };

    let mut key_field = code.to_string();
    if mode.report_alternate_keys && final_byte == b'u' && mods.shift_key() {
        if let Key::Character(shifted) = input.key {
            if let Some(s) = shifted.chars().next() {
                if s as u32 != code {
                    key_field.push_str(&format!(":{}", s as u32));
                }
            }
        }
    }

    let text_field = if mode.report_all_keys && mode.report_associated_text && !release {
        input
            .text
            .filter(|t| t.chars().all(|c| !c.is_control()))
            .map(|t| {
                t.chars()
                    .map(|c| (c as u32).to_string())
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .filter(|t| !t.is_empty())
    } else {
        None
    };

    let mut mods_field = String::new();
    if m != 1 || event.is_some() || text_field.is_some() {
        mods_field = m.to_string();
        if let Some(ev) = event {
            mods_field.push_str(&format!(":{ev}"));
        }
    }

    let mut out = String::from("\x1b[");
    if final_byte == b'u' || final_byte == b'~' {
        out.push_str(&key_field);
        if !mods_field.is_empty() {
            out.push(';');
            out.push_str(&mods_field);
        }
        if let Some(text) = text_field {
            out.push(';');
            out.push_str(&text);
        }
    } else if !mods_field.is_empty() {
        out.push_str("1;");
        out.push_str(&mods_field);
    }
    out.push(final_byte as char);
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press<'a>(key: &'a Key, base: &'a Key, text: Option<&'a str>) -> KeyInput<'a> {
        KeyInput {
            key,
            base,
            text,
            location: KeyLocation::Standard,
            state: KeyState::Press,
        }
    }

    fn legacy(key: Key, mods: ModifiersState) -> Option<Vec<u8>> {
        legacy_mode(key, mods, KeyMode::default())
    }

    fn legacy_mode(key: Key, mods: ModifiersState, mode: KeyMode) -> Option<Vec<u8>> {
        let text = match &key {
            Key::Character(c) => Some(c.to_string()),
            _ => None,
        };
        let base = match &key {
            Key::Character(c) => Key::Character(c.to_lowercase().into()),
            other => other.clone(),
        };
        encode(&press(&key, &base, text.as_deref()), mods, mode)
    }

    fn kitty_flags(bits: u8) -> KeyMode {
        KeyMode {
            disambiguate: bits & 1 != 0,
            report_event_types: bits & 2 != 0,
            report_alternate_keys: bits & 4 != 0,
            report_all_keys: bits & 8 != 0,
            report_associated_text: bits & 16 != 0,
            ..KeyMode::default()
        }
    }

    const NONE: ModifiersState = ModifiersState::empty();
    const CTRL: ModifiersState = ModifiersState::CONTROL;
    const SHIFT: ModifiersState = ModifiersState::SHIFT;
    const ALT: ModifiersState = ModifiersState::ALT;

    #[test]
    fn plain_characters_pass_through_as_utf8() {
        assert_eq!(
            legacy(Key::Character("a".into()), NONE),
            Some(b"a".to_vec())
        );
        assert_eq!(
            legacy(Key::Character("é".into()), NONE),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn ctrl_folds_into_c0_codes() {
        assert_eq!(legacy(Key::Character("c".into()), CTRL), Some(vec![0x03]));
        assert_eq!(legacy(Key::Character("[".into()), CTRL), Some(vec![0x1b]));
        assert_eq!(legacy(Key::Character("_".into()), CTRL), Some(vec![0x1f]));
        assert_eq!(legacy(Key::Character("2".into()), CTRL), Some(vec![0x00]));
        assert_eq!(legacy(Key::Named(NamedKey::Space), CTRL), Some(vec![0x00]));
        // Ctrl+Shift+letter is still the letter's control code.
        assert_eq!(
            legacy(Key::Character("D".into()), CTRL | SHIFT),
            Some(vec![0x04])
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            legacy(Key::Character("f".into()), ALT),
            Some(vec![0x1b, b'f'])
        );
        assert_eq!(
            legacy(Key::Named(NamedKey::Backspace), ALT),
            Some(vec![0x1b, 0x7f])
        );
        assert_eq!(
            legacy(Key::Character("c".into()), CTRL | ALT),
            Some(vec![0x1b, 0x03])
        );
    }

    #[test]
    fn special_keys_use_vt_sequences() {
        let cases: &[(NamedKey, &[u8])] = &[
            (NamedKey::Enter, b"\r"),
            (NamedKey::Backspace, b"\x7f"),
            (NamedKey::Tab, b"\t"),
            (NamedKey::ArrowUp, b"\x1b[A"),
            (NamedKey::Home, b"\x1b[H"),
            (NamedKey::Delete, b"\x1b[3~"),
            (NamedKey::F1, b"\x1bOP"),
            (NamedKey::F5, b"\x1b[15~"),
        ];
        for (key, bytes) in cases {
            assert_eq!(
                legacy(Key::Named(*key), NONE),
                Some(bytes.to_vec()),
                "{key:?}"
            );
        }
    }

    #[test]
    fn modifiers_on_special_keys_use_xterm_parameters() {
        assert_eq!(
            legacy(Key::Named(NamedKey::ArrowLeft), CTRL),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            legacy(Key::Named(NamedKey::ArrowRight), SHIFT | ALT),
            Some(b"\x1b[1;4C".to_vec())
        );
        assert_eq!(
            legacy(Key::Named(NamedKey::Delete), CTRL),
            Some(b"\x1b[3;5~".to_vec())
        );
        assert_eq!(
            legacy(Key::Named(NamedKey::F2), SHIFT),
            Some(b"\x1b[1;2Q".to_vec())
        );
        assert_eq!(
            legacy(Key::Named(NamedKey::Tab), SHIFT),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            legacy(Key::Named(NamedKey::Backspace), CTRL),
            Some(vec![0x08])
        );
    }

    #[test]
    fn application_cursor_mode_switches_arrows_to_ss3() {
        let mode = KeyMode {
            app_cursor: true,
            ..KeyMode::default()
        };
        assert_eq!(
            legacy_mode(Key::Named(NamedKey::ArrowUp), NONE, mode),
            Some(b"\x1bOA".to_vec())
        );
        // Modified arrows keep the CSI form even in DECCKM.
        assert_eq!(
            legacy_mode(Key::Named(NamedKey::ArrowUp), CTRL, mode),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn releases_are_silent_in_legacy_mode() {
        let key = Key::Character("a".into());
        let input = KeyInput {
            state: KeyState::Release,
            ..press(&key, &key, Some("a"))
        };
        assert_eq!(encode(&input, NONE, KeyMode::default()), None);
    }

    #[test]
    fn unmapped_named_key_returns_none() {
        assert_eq!(legacy(Key::Named(NamedKey::CapsLock), NONE), None);
    }

    #[test]
    fn kitty_disambiguate_escapes_modified_keys_only() {
        let mode = kitty_flags(1);
        assert_eq!(
            legacy_mode(Key::Character("a".into()), NONE, mode),
            Some(b"a".to_vec())
        );
        assert_eq!(
            legacy_mode(Key::Character("A".into()), SHIFT, mode),
            Some(b"A".to_vec())
        );
        assert_eq!(
            legacy_mode(Key::Character("c".into()), CTRL, mode),
            Some(b"\x1b[99;5u".to_vec())
        );
        assert_eq!(
            legacy_mode(Key::Named(NamedKey::Escape), NONE, mode),
            Some(b"\x1b[27u".to_vec())
        );
        // Enter/Tab/Backspace stay legacy so a crashed program can't leave
        // the shell untypeable.
        assert_eq!(
            legacy_mode(Key::Named(NamedKey::Enter), NONE, mode),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            legacy_mode(Key::Named(NamedKey::Enter), SHIFT, mode),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            legacy_mode(Key::Named(NamedKey::ArrowUp), NONE, mode),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            legacy_mode(Key::Named(NamedKey::ArrowUp), CTRL, mode),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            legacy_mode(Key::Named(NamedKey::F3), NONE, mode),
            Some(b"\x1b[13~".to_vec())
        );
        // Modifier keys alone aren't reported without flag 8.
        assert_eq!(legacy_mode(Key::Named(NamedKey::Shift), SHIFT, mode), None);
    }

    #[test]
    fn kitty_event_types_report_repeat_and_release() {
        let mode = kitty_flags(1 | 2);
        let key = Key::Character("x".into());
        let repeat = KeyInput {
            state: KeyState::Repeat,
            ..press(&key, &key, Some("x"))
        };
        assert_eq!(encode(&repeat, CTRL, mode), Some(b"\x1b[120;5:2u".to_vec()));
        let release = KeyInput {
            state: KeyState::Release,
            ..press(&key, &key, None)
        };
        assert_eq!(
            encode(&release, CTRL, mode),
            Some(b"\x1b[120;5:3u".to_vec())
        );
        // A text key sent as text on press has no release report.
        assert_eq!(encode(&release, NONE, mode), None);
        let up = Key::Named(NamedKey::ArrowUp);
        let release_up = KeyInput {
            state: KeyState::Release,
            ..press(&up, &up, None)
        };
        assert_eq!(
            encode(&release_up, NONE, mode),
            Some(b"\x1b[1;1:3A".to_vec())
        );
    }

    #[test]
    fn kitty_report_all_keys_with_alternates_and_text() {
        let mode = kitty_flags(1 | 4 | 8 | 16);
        let key = Key::Character("A".into());
        let base = Key::Character("a".into());
        assert_eq!(
            encode(&press(&key, &base, Some("A")), SHIFT, mode),
            Some(b"\x1b[97:65;2;65u".to_vec())
        );
        let plain = Key::Character("a".into());
        assert_eq!(
            encode(&press(&plain, &plain, Some("a")), NONE, mode),
            Some(b"\x1b[97;1;97u".to_vec())
        );
        let enter = Key::Named(NamedKey::Enter);
        assert_eq!(
            encode(&press(&enter, &enter, Some("\r")), NONE, mode),
            Some(b"\x1b[13u".to_vec())
        );
        let shift = Key::Named(NamedKey::Shift);
        let left_shift = KeyInput {
            location: KeyLocation::Left,
            ..press(&shift, &shift, None)
        };
        assert_eq!(
            encode(&left_shift, SHIFT, mode),
            Some(b"\x1b[57441;2u".to_vec())
        );
    }

    #[test]
    fn kitty_numpad_keys_have_their_own_codes() {
        let mode = kitty_flags(8);
        let key = Key::Character("5".into());
        let input = KeyInput {
            location: KeyLocation::Numpad,
            ..press(&key, &key, Some("5"))
        };
        assert_eq!(encode(&input, NONE, mode), Some(b"\x1b[57404u".to_vec()));
    }
}
