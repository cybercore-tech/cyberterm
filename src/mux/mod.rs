// src/mux/mod.rs
//
// The session daemon: shells that outlive their window. `server.rs` is the
// daemon, `client.rs` the window side, `protocol.rs` the wire format and
// `snapshot.rs` how a window rebuilds a pane it attaches to.

pub mod client;
pub mod protocol;
pub mod server;
pub mod snapshot;

use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle};

use protocol::TermSettings;

/// The terminal config both the daemon and a window's replica use for a
/// pane, so they parse its output identically.
pub fn term_config(settings: &TermSettings) -> TermConfig {
    TermConfig {
        scrolling_history: settings.scrollback,
        kitty_keyboard: settings.kitty_keyboard,
        default_cursor_style: CursorStyle {
            shape: match settings.cursor_shape.as_str() {
                "beam" => CursorShape::Beam,
                "underline" => CursorShape::Underline,
                _ => CursorShape::Block,
            },
            blinking: settings.cursor_blinking,
        },
        ..TermConfig::default()
    }
}
