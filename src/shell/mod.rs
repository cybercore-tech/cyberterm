// src/shell/mod.rs
//
// Shell integration: the OSC 7/133 tap (`tap.rs`), the scripts users
// source to emit those sequences, and prompt-mark lookups on the grid.

pub mod tap;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::Term;

pub use tap::{BlockMeta, ShellState, TappedPty, MARK_SCHEME};

const ZSH: &str = include_str!("../../shell-integration/cyberterm.zsh");
const BASH: &str = include_str!("../../shell-integration/cyberterm.bash");
const FISH: &str = include_str!("../../shell-integration/cyberterm.fish");

pub fn integration_script(shell: &str) -> Option<&'static str> {
    match shell {
        "zsh" => Some(ZSH),
        "bash" => Some(BASH),
        "fish" => Some(FISH),
        _ => None,
    }
}

/// True for the hidden prompt-mark links, which must never be drawn or
/// opened as real hyperlinks.
pub fn is_mark(uri: &str) -> bool {
    uri.starts_with(MARK_SCHEME)
}

/// Grid lines (negative = scrollback) where a shell prompt starts, oldest
/// first. A multi-line prompt shares one mark id across its rows, so only
/// the first row of each id counts.
pub fn prompt_lines<T>(term: &Term<T>) -> Vec<i32> {
    let grid = term.grid();
    let cols = grid.columns();
    let mut lines = Vec::new();
    let mut previous: Option<String> = None;
    for line in grid.topmost_line().0..=grid.bottommost_line().0 {
        let row = &grid[Line(line)];
        let mark = (0..cols).find_map(|col| {
            let link = row[Column(col)].hyperlink()?;
            is_mark(link.uri()).then(|| link.id().to_string())
        });
        if mark.is_some() && mark != previous {
            lines.push(line);
        }
        previous = mark;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::tests::{feed, test_term};
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[test]
    fn scripts_exist_for_supported_shells() {
        for shell in ["zsh", "bash", "fish"] {
            assert!(integration_script(shell).unwrap().contains("133;A"));
        }
        assert!(integration_script("tcsh").is_none());
    }

    #[test]
    fn scripts_parse_in_their_shells() {
        // Skipped for shells that aren't installed (e.g. fish in CI).
        for (shell, args) in [
            ("zsh", &["-n", "-c"][..]),
            ("bash", &["-n", "-c"]),
            ("fish", &["-n", "-c"]),
        ] {
            let script = integration_script(shell).unwrap();
            let Ok(out) = std::process::Command::new(shell)
                .args(args)
                .arg(script)
                .output()
            else {
                continue;
            };
            assert!(
                out.status.success(),
                "{shell} rejects its integration script: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn prompt_marks_survive_scrolling_into_history() {
        let mut term = test_term(20, 4);
        let shell = Arc::new(Mutex::new(ShellState::default()));
        let mut tap = tap::OscTap::new(shell);
        let mut bytes = Vec::new();
        for i in 0..3 {
            tap.feed(
                format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd{i}\r\n\x1b]133;C\x07out{i}\r\nmore\r\n\x1b]133;D;0\x07")
                    .as_bytes(),
                &mut bytes,
            );
        }
        tap.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07", &mut bytes);
        feed(&mut term, &bytes);

        let lines = prompt_lines(&term);
        // Four prompts, each 3 lines apart; the latest is on screen and
        // the earlier ones have scrolled into history (negative lines).
        assert_eq!(lines.len(), 4);
        assert!(lines[0] < 0);
        assert!(lines.windows(2).all(|w| w[1] - w[0] == 3));
    }
}
