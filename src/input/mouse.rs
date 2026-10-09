// src/input/mouse.rs
//
// Pure mouse event -> PTY report encoder for programs that turn on mouse
// tracking (vim, htop, tmux, fzf, ...). Covers the classic X10/normal
// encoding, its UTF-8 extension (mode 1005) and SGR (mode 1006), which is
// what nearly every modern program asks for.

use alacritty_terminal::term::TermMode;
use winit::keyboard::ModifiersState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    /// Motion with no button held.
    None,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press,
    Release,
    Motion,
}

/// Which mouse events the program asked for.
#[derive(Clone, Copy, Debug, Default)]
pub struct MouseMode {
    pub click: bool,
    pub drag: bool,
    pub motion: bool,
    pub sgr: bool,
    pub utf8: bool,
}

impl MouseMode {
    pub fn from_term(mode: TermMode) -> Self {
        Self {
            click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            drag: mode.contains(TermMode::MOUSE_DRAG),
            motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr: mode.contains(TermMode::SGR_MOUSE),
            utf8: mode.contains(TermMode::UTF8_MOUSE),
        }
    }

    /// True when the program wants any mouse reports at all.
    pub fn active(&self) -> bool {
        self.click || self.drag || self.motion
    }

    /// Whether a motion event should be reported, given whether any button
    /// is currently held.
    pub fn reports_motion(&self, button_held: bool) -> bool {
        self.motion || (self.drag && button_held)
    }
}

/// Encodes one report. `col`/`row` are 0-based cell coordinates.
pub fn encode(
    button: Button,
    action: Action,
    mods: ModifiersState,
    col: usize,
    row: usize,
    mode: MouseMode,
) -> Option<Vec<u8>> {
    let wheel = matches!(button, Button::WheelUp | Button::WheelDown);
    if wheel && action == Action::Release {
        return None;
    }

    let mut code: u32 = match button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::None => 3,
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    };
    if action == Action::Motion {
        code += 32;
    }
    if mods.shift_key() {
        code += 4;
    }
    if mods.alt_key() {
        code += 8;
    }
    if mods.control_key() {
        code += 16;
    }

    if mode.sgr {
        let fin = if action == Action::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{fin}", col + 1, row + 1).into_bytes());
    }

    // Normal encoding can't say which button was released.
    if action == Action::Release {
        code = (code & !0b11) | 3;
    }

    let mut out = b"\x1b[M".to_vec();
    out.push(32 + code as u8);
    for coord in [col, row] {
        let value = 32 + 1 + coord as u32;
        if mode.utf8 {
            // Mode 1005: coordinates above 95 become UTF-8 sequences
            // (limited to 2015, the largest two-byte value usable).
            if value > 2047 {
                return None;
            }
            let ch = char::from_u32(value)?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        } else {
            // Plain X10 can't address cells past column/row 223.
            if value > 255 {
                return None;
            }
            out.push(value as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SGR: MouseMode = MouseMode {
        click: true,
        drag: false,
        motion: false,
        sgr: true,
        utf8: false,
    };
    const NORMAL: MouseMode = MouseMode {
        click: true,
        drag: false,
        motion: false,
        sgr: false,
        utf8: false,
    };

    #[test]
    fn sgr_press_and_release() {
        let none = ModifiersState::empty();
        assert_eq!(
            encode(Button::Left, Action::Press, none, 0, 0, SGR),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
        assert_eq!(
            encode(Button::Right, Action::Release, none, 9, 4, SGR),
            Some(b"\x1b[<2;10;5m".to_vec())
        );
    }

    #[test]
    fn sgr_wheel_motion_and_modifiers() {
        assert_eq!(
            encode(
                Button::WheelDown,
                Action::Press,
                ModifiersState::CONTROL,
                2,
                3,
                SGR
            ),
            Some(b"\x1b[<81;3;4M".to_vec())
        );
        assert_eq!(
            encode(
                Button::Left,
                Action::Motion,
                ModifiersState::empty(),
                5,
                6,
                SGR
            ),
            Some(b"\x1b[<32;6;7M".to_vec())
        );
        assert_eq!(
            encode(
                Button::WheelUp,
                Action::Release,
                ModifiersState::empty(),
                0,
                0,
                SGR
            ),
            None
        );
    }

    #[test]
    fn normal_encoding_offsets_by_32_and_hides_release_button() {
        let none = ModifiersState::empty();
        assert_eq!(
            encode(Button::Middle, Action::Press, none, 0, 0, NORMAL),
            Some(vec![0x1b, b'[', b'M', 33, 33, 33])
        );
        assert_eq!(
            encode(Button::Middle, Action::Release, none, 1, 2, NORMAL),
            Some(vec![0x1b, b'[', b'M', 35, 34, 35])
        );
        assert_eq!(
            encode(Button::Left, Action::Press, none, 300, 0, NORMAL),
            None
        );
    }

    #[test]
    fn utf8_mode_extends_coordinates() {
        let mode = MouseMode {
            utf8: true,
            ..NORMAL
        };
        let out = encode(
            Button::Left,
            Action::Press,
            ModifiersState::empty(),
            300,
            0,
            mode,
        )
        .unwrap();
        assert_eq!(&out[..4], &[0x1b, b'[', b'M', 32]);
        assert_eq!(std::str::from_utf8(&out[4..]).unwrap(), "\u{14d}!");
    }

    #[test]
    fn motion_reporting_rules() {
        let drag = MouseMode { drag: true, ..SGR };
        assert!(drag.reports_motion(true));
        assert!(!drag.reports_motion(false));
        let any = MouseMode {
            motion: true,
            ..SGR
        };
        assert!(any.reports_motion(false));
    }
}
