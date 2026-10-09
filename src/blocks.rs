// src/blocks.rs
//
// Command blocks: where each command's prompt, command line and output sit
// on the grid. The prompt mark shell integration leaves on every prompt
// row (column 0) is the anchor -- it scrolls, reflows and ages out with
// the text -- and a block runs from its prompt to the next prompt.
//
//   $ cargo test            <- prompt rows (marked), command on the last
//   running 3 tests            one (plus any rows it soft-wraps onto)
//   ...                     <- output, up to the next prompt
//   $ _                     <- next block
//
// Metadata (command text, exit code, timing) comes from `ShellState`,
// matched by the mark's id.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::Term;

use crate::shell::{is_mark, BlockMeta, ShellState};

const ID_PREFIX: &str = "cyberterm-prompt-";

/// Where one block sits (grid lines; negative = scrollback).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSpan {
    pub mark: u64,
    /// First prompt row.
    pub prompt: i32,
    /// Last row of the prompt + command line.
    pub command_end: i32,
    /// One past the last output row.
    pub end: i32,
}

impl BlockSpan {
    pub fn output_lines(&self) -> std::ops::Range<i32> {
        self.command_end + 1..self.end
    }

    pub fn contains(&self, line: i32) -> bool {
        (self.prompt..self.end).contains(&line)
    }
}

/// The prompt mark on a row, if any.
fn mark_on<T>(term: &Term<T>, line: i32) -> Option<u64> {
    let link = term.grid()[Line(line)][Column(0)].hyperlink()?;
    if !is_mark(link.uri()) {
        return None;
    }
    link.id().strip_prefix(ID_PREFIX)?.parse().ok()
}

fn wraps<T>(term: &Term<T>, line: i32) -> bool {
    let cols = term.columns();
    term.grid()[Line(line)][Column(cols - 1)]
        .flags
        .contains(Flags::WRAPLINE)
}

/// The last line with content: the cursor's, at least.
fn last_line<T>(term: &Term<T>) -> i32 {
    term.grid().cursor.point.line.0
}

/// Completes a block that starts at `prompt` with mark `mark`.
fn span_from<T>(term: &Term<T>, prompt: i32, mark: u64) -> BlockSpan {
    let bottom = last_line(term);
    let mut command_end = prompt;
    while command_end < bottom && mark_on(term, command_end + 1) == Some(mark) {
        command_end += 1;
    }
    while command_end < bottom && wraps(term, command_end) {
        command_end += 1;
    }
    let mut end = command_end + 1;
    while end <= bottom && mark_on(term, end).is_none_or(|m| m == mark) {
        end += 1;
    }
    BlockSpan {
        mark,
        prompt,
        command_end,
        end,
    }
}

/// The block containing `line`, if shell integration marked one above it.
pub fn block_at<T>(term: &Term<T>, line: i32) -> Option<BlockSpan> {
    let top = term.grid().topmost_line().0;
    let mut l = line.min(last_line(term));
    let mark = loop {
        if let Some(m) = mark_on(term, l) {
            break m;
        }
        if l <= top {
            return None;
        }
        l -= 1;
    };
    // Back up to the first row of this prompt.
    while l > top && mark_on(term, l - 1) == Some(mark) {
        l -= 1;
    }
    let span = span_from(term, l, mark);
    span.contains(line).then_some(span)
}

/// Blocks overlapping lines `from..=to`, in order.
pub fn spans_in<T>(term: &Term<T>, from: i32, to: i32) -> Vec<BlockSpan> {
    let mut out = Vec::new();
    let mut line = from;
    let last = last_line(term).min(to);
    if let Some(first) = block_at(term, from) {
        line = first.end;
        out.push(first);
    }
    while line <= last {
        match mark_on(term, line) {
            Some(mark) => {
                let span = span_from(term, line, mark);
                line = span.end.max(line + 1);
                out.push(span);
            }
            None => line += 1,
        }
    }
    out
}

/// The block for a mark id, searching from the bottom (recent first).
pub fn span_of<T>(term: &Term<T>, mark: u64) -> Option<BlockSpan> {
    let top = term.grid().topmost_line().0;
    let mut l = last_line(term);
    while l >= top {
        if mark_on(term, l) == Some(mark) {
            while l > top && mark_on(term, l - 1) == Some(mark) {
                l -= 1;
            }
            return Some(span_from(term, l, mark));
        }
        l -= 1;
    }
    None
}

/// The output of a block as plain text.
pub fn output_text<T>(term: &Term<T>, span: &BlockSpan) -> String {
    let lines = span.output_lines();
    if lines.is_empty() {
        return String::new();
    }
    crate::frame::lines_text(term, lines.start, lines.end - 1)
}

/// Metadata for a mark, if it ran a command.
pub fn meta(shell: &ShellState, mark: u64) -> Option<&BlockMeta> {
    shell
        .blocks
        .iter()
        .rev()
        .find(|b| b.mark == mark)
        .filter(|b| b.is_command())
}

/// `1.2s`, `45s`, `3m07s`, `2h05m`.
pub fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=9 => format!("{:.1}s", ms as f64 / 1000.0),
        10..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::tests::{feed, test_term};
    use crate::shell::tap::OscTap;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// Runs shell-integration-marked output through the tap into a term.
    fn session(
        cols: usize,
        rows: usize,
        script: &[&str],
    ) -> (
        alacritty_terminal::Term<alacritty_terminal::event::VoidListener>,
        ShellState,
    ) {
        let mut term = test_term(cols, rows);
        let shell = Arc::new(Mutex::new(ShellState::default()));
        let mut tap = OscTap::new(shell.clone());
        let mut bytes = Vec::new();
        for chunk in script {
            tap.feed(chunk.as_bytes(), &mut bytes);
        }
        feed(&mut term, &bytes);
        let state = shell.lock().clone();
        (term, state)
    }

    const A: &str = "\x1b]133;A\x07";
    const B: &str = "\x1b]133;B\x07";
    const D0: &str = "\x1b]133;D;0\x07";

    fn cmd(c: &str) -> String {
        format!("\x1b]133;C;cmdline_url={}\x07", c.replace(' ', "%20"))
    }

    #[test]
    fn blocks_cover_prompt_command_and_output() {
        let script = [
            format!("{A}$ {B}echo one"),
            format!("\r\n{}one\r\n{D0}", cmd("echo one")),
            format!("{A}$ {B}ls"),
            format!("\r\n{}a\r\nb\r\nc\r\n\x1b]133;D;2\x07", cmd("ls")),
            format!("{A}$ {B}"),
        ];
        let script: Vec<&str> = script.iter().map(String::as_str).collect();
        let (term, shell) = session(30, 12, &script);
        let spans = spans_in(&term, 0, 11);
        assert_eq!(spans.len(), 3);
        assert_eq!(
            (spans[0].prompt, spans[0].command_end, spans[0].end),
            (0, 0, 2)
        );
        assert_eq!((spans[1].prompt, spans[1].end), (2, 6));
        assert_eq!(output_text(&term, &spans[1]), "a\nb\nc");
        assert_eq!(output_text(&term, &spans[0]), "one");
        // The newest prompt has no output yet.
        assert!(spans[2].output_lines().is_empty());

        let ls = meta(&shell, spans[1].mark).unwrap();
        assert_eq!(ls.command.as_deref(), Some("ls"));
        assert_eq!(ls.exit, Some(2));
        assert!(meta(&shell, spans[2].mark).is_none());

        // Lookups from any row of a block, and by mark.
        assert_eq!(block_at(&term, 4), Some(spans[1]));
        assert_eq!(block_at(&term, 2), Some(spans[1]));
        assert_eq!(span_of(&term, spans[0].mark), Some(spans[0]));
    }

    #[test]
    fn multi_row_prompts_and_wrapped_commands() {
        let long = "x".repeat(25);
        let script = [
            format!("{A}~/proj main\r\n$ {B}echo {long}"),
            format!("\r\n{}{long}\r\n{D0}{A}$ {B}", cmd("echo")),
        ];
        let script: Vec<&str> = script.iter().map(String::as_str).collect();
        let (term, _) = session(20, 12, &script);
        let spans = spans_in(&term, 0, 11);
        // Two prompt rows, the command wraps onto a third.
        assert_eq!((spans[0].prompt, spans[0].command_end), (0, 2));
        assert_eq!(output_text(&term, &spans[0]), long);
    }

    #[test]
    fn blocks_scrolled_into_history_are_still_found() {
        let mut script = Vec::new();
        for i in 0..10 {
            script.push(format!("{A}$ {B}seq {i}\r\n{}", cmd("seq")));
            for j in 0..5 {
                script.push(format!("line{i}-{j}\r\n"));
            }
            script.push(D0.to_string());
        }
        script.push(format!("{A}$ {B}"));
        let script: Vec<&str> = script.iter().map(String::as_str).collect();
        let (term, shell) = session(20, 8, &script);
        let first = shell.blocks[0].mark;
        let span = span_of(&term, first).unwrap();
        assert!(span.prompt < 0);
        assert!(output_text(&term, &span).starts_with("line0-0"));
        // Visible-range query starting mid-output finds the enclosing block.
        let visible = spans_in(&term, 0, 7);
        assert!(visible[0].prompt <= 0);
    }

    #[test]
    fn no_marks_no_blocks() {
        let mut term = test_term(10, 4);
        feed(&mut term, b"plain\r\noutput");
        assert!(spans_in(&term, 0, 3).is_empty());
        assert!(block_at(&term, 1).is_none());
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(1234), "1.2s");
        assert_eq!(format_duration(45_000), "45s");
        assert_eq!(format_duration(187_000), "3m07s");
        assert_eq!(format_duration(7_500_000), "2h05m");
    }
}
