// src/clipboard.rs
//
// System clipboard and (on Linux) primary selection access. Native on
// Wayland through the wlr data-control protocol, X11 otherwise.

use arboard::Clipboard;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Clipboard,
    /// The X11/Wayland primary selection: whatever was last selected,
    /// pasted with middle-click or Shift+Insert. Falls back to the
    /// clipboard on platforms without one.
    Primary,
}

pub struct ClipboardManager {
    ctx: Clipboard,
}

/// Box-drawing characters copied out of TUIs are swapped for ASCII so a
/// pasted table still lines up in places without those glyphs.
fn clean_box_drawing(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '─' | '━' | '═' => '-',
            '│' | '┃' | '║' => '|',
            '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' | '╭' | '╮' | '╯' | '╰' => {
                '+'
            }
            other => other,
        })
        .collect()
}

impl ClipboardManager {
    /// `None` when no clipboard backend is available (e.g. a display
    /// session without clipboard support) -- callers should treat that as
    /// "clipboard operations are silently unavailable," not a fatal error,
    /// since a terminal is otherwise fully usable without one.
    pub fn try_new() -> Option<Self> {
        Some(Self {
            ctx: Clipboard::new().ok()?,
        })
    }

    pub fn set(&mut self, kind: Kind, text: &str, clean: bool) {
        let text = if clean {
            clean_box_drawing(text)
        } else {
            text.to_string()
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        if kind == Kind::Primary {
            use arboard::{LinuxClipboardKind, SetExtLinux};
            let _ = self
                .ctx
                .set()
                .clipboard(LinuxClipboardKind::Primary)
                .text(text);
            return;
        }
        let _ = kind;
        let _ = self.ctx.set_text(text);
    }

    pub fn get(&mut self, kind: Kind) -> Option<String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        if kind == Kind::Primary {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            return self
                .ctx
                .get()
                .clipboard(LinuxClipboardKind::Primary)
                .text()
                .ok();
        }
        let _ = kind;
        self.ctx.get_text().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_drawing_becomes_ascii() {
        assert_eq!(clean_box_drawing("┌─┐\n│x│"), "+-+\n|x|");
    }
}
