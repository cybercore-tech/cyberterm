// src/find.rs
//
// Text search through a terminal's scrollback and screen (Ctrl+Shift+F).
// Smart case: a query with no capitals matches any case.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::Term;

/// One hit: a grid line (negative = scrollback) and its column range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub line: i32,
    pub start: usize,
    pub end: usize,
}

/// Most matches kept (a one-letter query in a big scrollback).
const MAX_MATCHES: usize = 20_000;

/// All matches, oldest first.
pub fn find_all<T>(term: &Term<T>, query: &str) -> Vec<Match> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold = !query.chars().any(char::is_uppercase);
    let needle: Vec<char> = if fold {
        query.to_lowercase().chars().collect()
    } else {
        query.chars().collect()
    };
    let grid = term.grid();
    let cols = grid.columns();
    let mut out = Vec::new();
    for line in grid.topmost_line().0..=grid.bottommost_line().0 {
        let row = &grid[Line(line)];
        // Characters of the row with the column each starts at.
        let mut chars = Vec::with_capacity(cols);
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let c = if fold {
                cell.c.to_lowercase().next().unwrap_or(cell.c)
            } else {
                cell.c
            };
            chars.push((c, col));
        }
        if chars.len() < needle.len() {
            continue;
        }
        let mut i = 0;
        while i + needle.len() <= chars.len() {
            if chars[i..i + needle.len()]
                .iter()
                .map(|(c, _)| *c)
                .eq(needle.iter().copied())
            {
                let last = chars[i + needle.len() - 1].1;
                out.push(Match {
                    line,
                    start: chars[i].1,
                    end: last + 1,
                });
                if out.len() >= MAX_MATCHES {
                    return out;
                }
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::tests::{feed, test_term};

    #[test]
    fn finds_across_scrollback_with_smart_case() {
        let mut term = test_term(20, 3);
        feed(
            &mut term,
            b"Error one\r\nok\r\nerror two\r\nfine\r\nERROR three",
        );
        let all = find_all(&term, "error");
        assert_eq!(all.len(), 3);
        assert!(all[0].line < 0, "oldest first, from the scrollback");
        assert_eq!((all[0].start, all[0].end), (0, 5));
        // Capitals make it case-sensitive.
        assert_eq!(find_all(&term, "ERROR").len(), 1);
        assert!(find_all(&term, "").is_empty());
        assert!(find_all(&term, "missing").is_empty());
    }

    #[test]
    fn columns_account_for_wide_characters() {
        let mut term = test_term(20, 2);
        feed(&mut term, "界界 needle".as_bytes());
        let m = find_all(&term, "needle")[0];
        assert_eq!((m.start, m.end), (5, 11));
    }

    #[test]
    fn repeated_matches_on_one_line() {
        let mut term = test_term(20, 2);
        feed(&mut term, b"abab ab");
        assert_eq!(find_all(&term, "ab").len(), 3);
    }
}
