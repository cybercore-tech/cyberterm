// src/clipboard.rs
//
// System clipboard and primary selection.
//
// On Wayland the window's own connection is used (smithay-clipboard), so
// Cyberterm offers selections as the focused client, which every
// compositor honours. arboard is the fallback (X11, macOS, or before the
// window exists): on Wayland it goes through the data-control protocol,
// where Hyprland doesn't keep a primary selection set that way.

use arboard::Clipboard;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Clipboard,
    /// The X11/Wayland primary selection: whatever was last selected,
    /// pasted with middle-click. Falls back to the clipboard on platforms
    /// without one.
    Primary,
}

pub struct ClipboardManager {
    #[cfg(all(unix, not(target_os = "macos")))]
    wayland: Option<smithay_clipboard::Clipboard>,
    fallback: Option<Clipboard>,
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
    /// Always succeeds; operations are silently unavailable when no
    /// backend works (a terminal is fully usable without a clipboard).
    pub fn new() -> Self {
        Self {
            #[cfg(all(unix, not(target_os = "macos")))]
            wayland: None,
            fallback: Clipboard::new().ok(),
        }
    }

    /// Switches to the window's Wayland connection when there is one.
    pub fn attach(&mut self, window: &winit::window::Window) {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
            if let Ok(handle) = window.display_handle() {
                if let RawDisplayHandle::Wayland(wayland) = handle.as_raw() {
                    // SAFETY: the display pointer comes from the live
                    // window, and `App` drops this clipboard before the
                    // window and event loop (field order in `App`).
                    self.wayland = Some(unsafe {
                        smithay_clipboard::Clipboard::new(wayland.display.as_ptr())
                    });
                }
            }
        }
        let _ = window;
    }

    pub fn set(&mut self, kind: Kind, text: &str, clean: bool) {
        let text = if clean {
            clean_box_drawing(text)
        } else {
            text.to_string()
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if let Some(wayland) = &self.wayland {
                match kind {
                    Kind::Clipboard => wayland.store(text),
                    Kind::Primary => wayland.store_primary(text),
                }
                return;
            }
            if kind == Kind::Primary {
                use arboard::{LinuxClipboardKind, SetExtLinux};
                if let Some(fallback) = &mut self.fallback {
                    if let Err(e) = fallback
                        .set()
                        .clipboard(LinuxClipboardKind::Primary)
                        .text(text)
                    {
                        eprintln!("cyberterm: primary selection not set: {e}");
                    }
                }
                return;
            }
        }
        let _ = kind;
        if let Some(fallback) = &mut self.fallback {
            if let Err(e) = fallback.set_text(text) {
                eprintln!("cyberterm: clipboard not set: {e}");
            }
        }
    }

    pub fn get(&mut self, kind: Kind) -> Option<String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if let Some(wayland) = &self.wayland {
                return match kind {
                    Kind::Clipboard => wayland.load(),
                    Kind::Primary => wayland.load_primary(),
                }
                .ok();
            }
            if kind == Kind::Primary {
                use arboard::{GetExtLinux, LinuxClipboardKind};
                return self
                    .fallback
                    .as_mut()?
                    .get()
                    .clipboard(LinuxClipboardKind::Primary)
                    .text()
                    .ok();
            }
        }
        let _ = kind;
        self.fallback.as_mut()?.get_text().ok()
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
