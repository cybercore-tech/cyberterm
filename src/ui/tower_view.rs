// src/ui/tower_view.rs
//
// The Tower (Ctrl+Shift+S): every agent session at once. This draws its
// left side -- the sessions, two lines each (state, name, agent, how long
// since it last did something; then its branch and what it changed), the
// selected one highlighted, keys or a question in the footer. The right
// side is the selected session's flight log, drawn by
// src/ui/flight_panel.rs.

use crate::agent_home::{Mark, SessionRow};
use crate::frame::Frame;

pub struct Colors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub dim: [u8; 3],
    pub accent: [u8; 3],
    pub ok: [u8; 3],
    pub bad: [u8; 3],
    pub warn: [u8; 3],
    pub selected: [u8; 3],
}

pub struct View<'a> {
    pub rows: &'a [SessionRow],
    pub selected: usize,
    /// The key hints on the bottom row (hidden while the window shows
    /// a question or a message across it).
    pub keys: bool,
    /// The sessions haven't been read yet.
    pub loading: bool,
}

/// Rows each session takes in the list.
const PER_SESSION: usize = 3;

fn put_runs(
    f: &mut Frame,
    row: usize,
    col: usize,
    end: usize,
    runs: &[(&str, [u8; 3])],
    bg: [u8; 3],
) -> usize {
    let mut c = col;
    for (text, fg) in runs {
        if c >= end {
            break;
        }
        let piece: String = text.chars().take(end - c).collect();
        c = f.put(row, c, &piece, *fg, bg);
    }
    c
}

/// `s` shortened in the middle, so both ends show: a task's attempts
/// share their start and differ at the end (the agent).
fn fit_middle(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    if width < 5 {
        return fit(s, width);
    }
    let tail = (width - 1) * 11 / 20;
    let head = width - 1 - tail;
    let start: String = s.chars().take(head).collect();
    let end: String = s.chars().skip(n - tail).collect();
    format!("{start}…{end}")
}

fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut t: String = s.chars().take(width - 1).collect();
    t.push('…');
    t
}

fn ago(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// The key hints, as many as fit: the least needed go first, then the
/// words for the rest.
fn keys_line(cols: usize) -> String {
    const KEYS: [(&str, &str); 7] = [
        ("↵", "jump"),
        ("m", "merge"),
        ("d", "discard"),
        ("c", "changes"),
        ("n", "new"),
        ("Esc", "close"),
        ("↑↓", "select"),
    ];
    let line = |n: usize, words: bool| {
        let parts: Vec<String> = KEYS[..n]
            .iter()
            .map(|(k, w)| {
                if words {
                    format!("{k} {w}")
                } else {
                    k.to_string()
                }
            })
            .collect();
        format!(" {}", parts.join(" · "))
    };
    (1..=KEYS.len())
        .rev()
        .map(|n| line(n, true))
        .chain(std::iter::once(line(KEYS.len(), false)))
        .find(|l| l.chars().count() < cols)
        .unwrap_or_default()
}

/// The first session row shown, keeping `selected` on screen.
pub fn first_visible(selected: usize, rows: usize) -> usize {
    let fit = (rows.saturating_sub(4) / PER_SESSION).max(1);
    selected.saturating_sub(fit - 1)
}

pub fn build(v: &View<'_>, cols: usize, rows: usize, c: &Colors) -> Frame {
    let mut f = Frame::blank(cols, rows, c.fg, c.bg);
    if cols < 24 || rows < 6 {
        return f;
    }

    // Header: counts that matter.
    let waiting = v.rows.iter().filter(|r| r.mark == Mark::Waiting).count();
    let working = v.rows.iter().filter(|r| r.mark == Mark::Working).count();
    let done = v.rows.iter().filter(|r| r.mark == Mark::Done).count();
    let n = v.rows.len();
    let mut header: Vec<(String, [u8; 3])> = vec![
        (" ◆ TOWER".into(), c.accent),
        (
            format!("  {n} session{}", if n == 1 { "" } else { "s" }),
            c.dim,
        ),
    ];
    if waiting > 0 {
        header.push((
            format!(
                " · {waiting} need{} you",
                if waiting == 1 { "s" } else { "" }
            ),
            c.warn,
        ));
    }
    if working > 0 {
        header.push((format!(" · {working} working"), c.accent));
    }
    if done > 0 {
        header.push((format!(" · {done} done"), c.ok));
    }
    let runs: Vec<(&str, [u8; 3])> = header.iter().map(|(t, col)| (t.as_str(), *col)).collect();
    put_runs(&mut f, 0, 0, cols, &runs, c.bg);
    f.put(1, 0, &"─".repeat(cols), c.dim, c.bg);

    // Footer.
    let text = if v.keys {
        keys_line(cols)
    } else {
        String::new()
    };
    put_runs(&mut f, rows - 1, 0, cols, &[(&text, c.dim)], c.bg);

    let body = 2..rows - 1;
    if v.rows.is_empty() {
        let text = if v.loading {
            " Reading sessions…"
        } else {
            " No agent sessions yet. n starts one."
        };
        put_runs(&mut f, body.start + 1, 0, cols, &[(text, c.dim)], c.bg);
        return f;
    }

    let first = first_visible(v.selected, rows);
    let mut row = body.start;
    for (i, s) in v.rows.iter().enumerate().skip(first) {
        if row + 1 >= body.end {
            break;
        }
        let bg = if i == v.selected { c.selected } else { c.bg };
        if bg != c.bg {
            for r in row..row + 2 {
                f.fill(r, 0, c.fg, bg);
            }
        }
        let (glyph, glyph_c, state, state_c) = match s.mark {
            Mark::Working => (
                "◆",
                c.accent,
                s.idle_ms
                    .map(|ms| format!("working · {}", ago(ms)))
                    .unwrap_or_else(|| "working".into()),
                c.fg,
            ),
            Mark::Waiting => ("◆!", c.warn, "needs you".into(), c.warn),
            Mark::Done => (
                "✓",
                c.ok,
                match s.changes {
                    Some((0, 0, _)) | None => "done".to_string(),
                    _ => "done · ready to land".to_string(),
                },
                c.ok,
            ),
            Mark::Stopped => ("○", c.dim, "stopped".into(), c.dim),
        };
        // Line one: mark, name, agent; the state on the right.
        let state_w = state.chars().count();
        let right = cols.saturating_sub(state_w + 1);
        let name_w = right.saturating_sub(4 + 10).max(6);
        let name = fit_middle(&s.id, name_w);
        let at = put_runs(&mut f, row, 1, right, &[(glyph, glyph_c)], bg);
        let at = at.max(4);
        let at = put_runs(&mut f, row, at, right, &[(&name, c.fg)], bg);
        put_runs(
            &mut f,
            row,
            at + 2,
            right,
            &[(&fit(&s.agent, 8), c.dim)],
            bg,
        );
        put_runs(&mut f, row, right, cols, &[(&state, state_c)], bg);

        // Line two: branch, then the change counts and a bar.
        let mut line: Vec<(String, [u8; 3])> = Vec::new();
        let stats = match s.changes {
            Some((0, 0, _)) if s.commits == 0 => vec![("no changes yet".to_string(), c.dim)],
            Some((a, r, files)) => {
                let mut v = vec![
                    (format!("+{a}"), c.ok),
                    (" ".into(), c.dim),
                    (format!("−{r}"), c.bad),
                    (
                        format!(" · {files} file{}", if files == 1 { "" } else { "s" }),
                        c.dim,
                    ),
                ];
                if s.commits > 0 {
                    v.push((
                        format!(
                            " · {} commit{}",
                            s.commits,
                            if s.commits == 1 { "" } else { "s" }
                        ),
                        c.dim,
                    ));
                }
                v
            }
            None => Vec::new(),
        };
        let stats_w: usize = stats.iter().map(|(t, _)| t.chars().count()).sum();
        let place_w = cols.saturating_sub(4 + stats_w + 3).max(8);
        line.push((fit(&s.place, place_w), c.dim));
        let runs: Vec<(&str, [u8; 3])> = line.iter().map(|(t, col)| (t.as_str(), *col)).collect();
        put_runs(&mut f, row + 1, 4, cols, &runs, bg);
        let runs: Vec<(&str, [u8; 3])> = stats.iter().map(|(t, col)| (t.as_str(), *col)).collect();
        put_runs(
            &mut f,
            row + 1,
            cols.saturating_sub(stats_w + 1).max(4),
            cols,
            &runs,
            bg,
        );
        // Attempts at one task: a bar down their left edge, joined
        // across the gap to the next attempt.
        if s.group.is_some() {
            let next_same = v
                .rows
                .get(i + 1)
                .is_some_and(|n| n.group.is_some() && n.group == s.group);
            let reach = if next_same { PER_SESSION } else { 2 };
            for r in row..(row + reach).min(body.end) {
                let cell_bg = if r < row + 2 { bg } else { c.bg };
                f.put(r, 0, "▎", c.accent, cell_bg);
            }
        }
        row += PER_SESSION;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Colors {
        Colors {
            fg: [200; 3],
            bg: [0; 3],
            dim: [100; 3],
            accent: [0, 200, 200],
            ok: [0, 200, 0],
            bad: [200, 0, 0],
            warn: [200, 200, 0],
            selected: [40; 3],
        }
    }

    fn row(
        id: &str,
        mark: Mark,
        changes: Option<(usize, usize, usize)>,
        commits: usize,
    ) -> SessionRow {
        SessionRow {
            id: id.into(),
            agent: "claude".into(),
            mark,
            idle_ms: Some(125_000),
            place: format!("agent/{id}"),
            changes,
            running: mark != Mark::Stopped,
            commits,
            group: None,
        }
    }

    fn text(f: &Frame) -> Vec<String> {
        (0..f.rows)
            .map(|r| f.row_text(r).0.trim_end().to_string())
            .collect()
    }

    #[test]
    fn sessions_states_and_changes_show() {
        let rows = [
            row("fix-login", Mark::Working, Some((42, 7, 3)), 1),
            row("add-auth", Mark::Waiting, Some((0, 0, 0)), 0),
            row("bump-deps", Mark::Done, Some((13, 1, 2)), 2),
        ];
        let v = View {
            rows: &rows,
            selected: 1,
            keys: true,
            loading: false,
        };
        let t = text(&build(&v, 60, 14, &colors()));
        let all = t.join("\n");
        assert!(
            t[0].contains("◆ TOWER  3 sessions · 1 needs you · 1 working · 1 done"),
            "{all}"
        );
        assert!(
            t[2].contains("◆  fix-login  claude") && t[2].ends_with("working · 2m"),
            "{all}"
        );
        assert!(
            t[3].contains("agent/fix-login") && t[3].contains("+42 −7 · 3 files · 1 commit"),
            "{all}"
        );
        assert!(
            t[5].contains("◆! add-auth") && t[5].ends_with("needs you"),
            "{all}"
        );
        assert!(t[6].contains("no changes yet"), "{all}");
        assert!(t[8].ends_with("done · ready to land"), "{all}");
        assert!(t[13].contains("↵ jump · m merge · d discard"), "{all}");
        // Narrow: the hints that fit, never cut mid-word.
        for cols in [30, 45, 60, 90] {
            let k = keys_line(cols);
            assert!(k.chars().count() < cols, "{cols}: {k}");
            assert!(k.contains("↵ jump"), "{cols}: {k}");
        }
        assert!(keys_line(90).ends_with("↑↓ select"));
        // The selected session is highlighted, both its lines.
        let f = build(&v, 60, 14, &colors());
        assert_eq!(f.row(5)[30].bg, [40; 3]);
        assert_eq!(f.row(6)[30].bg, [40; 3]);
        assert_eq!(f.row(2)[30].bg, [0; 3]);
    }

    #[test]
    fn long_names_keep_both_ends() {
        assert_eq!(
            fit_middle("make-the-health-test-demo-2", 14),
            "make-t…-demo-2"
        );
        assert_eq!(fit_middle("short", 14), "short");
    }

    #[test]
    fn attempts_at_one_task_are_joined_by_a_bar() {
        let mut rows = vec![
            row("fix-it-claude", Mark::Done, Some((5, 1, 1)), 1),
            row("fix-it-codex", Mark::Working, Some((9, 3, 2)), 0),
            row("other", Mark::Stopped, None, 0),
        ];
        rows[0].group = Some("fix-it-1".into());
        rows[1].group = Some("fix-it-1".into());
        let v = View {
            rows: &rows,
            selected: 2,
            keys: true,
            loading: false,
        };
        let f = build(&v, 60, 14, &colors());
        let bar = |r: usize| f.row(r)[0].ch;
        // Both lines of each attempt, and the gap between them.
        for r in 2..7 {
            assert_eq!(bar(r), '▎', "row {r}");
        }
        // Not past the last attempt, nor on the ungrouped session.
        assert_eq!(bar(7), ' ');
        assert_eq!(bar(8), ' ');
    }

    #[test]
    fn questions_empty_and_scrolling() {
        let rows: Vec<SessionRow> = (0..10)
            .map(|i| row(&format!("s{i}"), Mark::Stopped, None, 0))
            .collect();
        let v = View {
            rows: &rows,
            selected: 9,
            keys: false,
            loading: false,
        };
        let t = text(&build(&v, 50, 12, &colors()));
        let all = t.join("\n");
        // The selected session stays on screen.
        assert!(all.contains("○  s9"), "{all}");
        assert!(!all.contains("s0 "), "{all}");
        // Keys hidden: the bottom row is left for the window's question.
        assert_eq!(t[11], "", "{all}");

        let none: [SessionRow; 0] = [];
        let v = View {
            rows: &none,
            selected: 0,
            keys: true,
            loading: false,
        };
        let t = text(&build(&v, 50, 10, &colors()));
        assert!(t
            .join("\n")
            .contains("No agent sessions yet. n starts one."));
        assert!(t[9].contains("↵ jump"));
    }
}
