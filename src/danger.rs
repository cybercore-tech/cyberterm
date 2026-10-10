// src/danger.rs
//
// Danger mode helpers: reading the command line about to run from the
// terminal grid, so Enter can be held back for a confirmation when a
// risky command is typed into a production / root pane.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::Term;

/// The logical line the cursor is on (following soft wraps upward),
/// without the prompt when shell integration marked it. Without
/// integration (a remote shell over SSH) the prompt text stays in, which
/// is fine for spotting a dangerous command.
pub fn command_line<T>(term: &Term<T>) -> String {
    let grid = term.grid();
    let cols = grid.columns();
    let cursor = grid.cursor.point.line.0;
    let mut first = cursor;
    while first > grid.topmost_line().0 {
        let above = &grid[Line(first - 1)][Column(cols - 1)];
        if !above.flags.contains(Flags::WRAPLINE) {
            break;
        }
        first -= 1;
    }
    let mut text = String::new();
    for line in first..=cursor {
        let row = &grid[Line(line)];
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            if cell
                .hyperlink()
                .is_some_and(|l| crate::shell::is_mark(l.uri()))
            {
                // Prompt text (between OSC 133 A and B).
                continue;
            }
            text.push(cell.c);
        }
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::tests::{feed, test_term};

    #[test]
    fn reads_the_typed_command_without_the_marked_prompt() {
        let mut term = test_term(30, 4);
        feed(
            &mut term,
            b"old output\r\n\x1b]8;id=cyberterm-prompt-1;cyberterm-mark:prompt?exit=\x1b\\user@prod $ \x1b]8;;\x1b\\rm -rf /var/lib/app",
        );
        assert_eq!(command_line(&term), "rm -rf /var/lib/app");
    }

    #[test]
    fn follows_soft_wraps_and_keeps_unmarked_prompts() {
        let mut term = test_term(10, 4);
        feed(&mut term, b"$ git push --force");
        assert_eq!(command_line(&term), "$ git push --force");
    }
}
