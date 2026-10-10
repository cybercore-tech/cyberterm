// src/ui/flight_panel.rs
//
// The Flight log panel beside an agent's tab: who it is and where it
// stands (working, needs you, done), then what it did -- prompts,
// commands with ✓/✗ and how long they took, edits, other tools -- newest
// at the bottom. A command's output opens under it (Enter); failed
// commands show their last line of output right away.

use crate::flight_log::{clock, Entry, State, Summary};
use crate::frame::Frame;
use std::collections::HashSet;

/// Output lines shown under an opened command.
const OUTPUT_LINES: usize = 12;

pub struct Colors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub dim: [u8; 3],
    pub accent: [u8; 3],
    pub ok: [u8; 3],
    pub bad: [u8; 3],
    pub warn: [u8; 3],
    /// Behind the selected entry.
    pub selected: [u8; 3],
}

pub struct View<'a> {
    /// "Claude Code · fix-the-flaky-login".
    pub title: &'a str,
    pub branch: Option<&'a str>,
    /// The agent is still running.
    pub running: bool,
    pub summary: Summary,
    pub entries: &'a [Entry],
    /// The selected entry; `None` follows the newest.
    pub selected: Option<usize>,
    pub expanded: &'a HashSet<usize>,
    /// The panel has the keyboard.
    pub focused: bool,
    /// Paths are shown relative to this (the worktree).
    pub root: Option<&'a str>,
    /// The key that focuses / closes the panel, for the footer.
    pub key: &'a str,
}

/// One display line: (text, fg) runs, and the entry it belongs to.
type Line = (Vec<(String, [u8; 3])>, Option<usize>);

fn short_path<'a>(path: &'a str, root: Option<&str>) -> &'a str {
    root.and_then(|r| path.strip_prefix(r))
        .map(|p| p.trim_start_matches('/'))
        .filter(|p| !p.is_empty())
        .unwrap_or(path)
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

fn entry_lines(i: usize, e: &Entry, v: &View<'_>, c: &Colors, out: &mut Vec<Line>) {
    let time = |t: u64| (format!(" {} ", &clock(t)[..5]), c.dim);
    let mut push = |spans: Vec<(String, [u8; 3])>| out.push((spans, Some(i)));
    match e {
        Entry::Prompt { t, text } => push(vec![
            time(*t),
            ("» ".into(), c.accent),
            (first_line(text).into(), c.accent),
        ]),
        Entry::Command {
            t,
            command,
            exit,
            failed,
            duration_ms,
            output,
            running,
            ..
        } => {
            let bad = *failed || exit.is_some_and(|x| x != 0);
            let mark = match (exit, bad, running) {
                (Some(x), true, _) if *x != 0 => (format!("✗ {x} "), c.bad),
                (_, true, _) => ("✗ ".into(), c.bad),
                (Some(_), false, _) => ("✓ ".into(), c.ok),
                (None, false, true) if v.running => ("… ".into(), c.warn),
                (None, false, _) => ("· ".into(), c.dim),
            };
            let mut spans = vec![time(*t), mark, (first_line(command).into(), c.fg)];
            if let Some(d) = duration_ms {
                spans.push((format!("  {}", crate::blocks::format_duration(*d)), c.dim));
            }
            push(spans);
            let lines: Vec<&str> = output
                .as_deref()
                .map(|o| o.lines().filter(|l| !l.trim().is_empty()).collect())
                .unwrap_or_default();
            let shown = if v.expanded.contains(&i) {
                OUTPUT_LINES
            } else if bad {
                1
            } else {
                0
            };
            let from = lines.len().saturating_sub(shown);
            for l in &lines[from..] {
                out.push((vec![(format!("         {l}"), c.dim)], Some(i)));
            }
        }
        Entry::Edit { t, path, tool } => {
            let mut spans = vec![
                time(*t),
                ("✎ ".into(), c.accent),
                (short_path(path, v.root).into(), c.fg),
            ];
            if let Some(tool) = tool {
                spans.push((format!("  {tool}"), c.dim));
            }
            push(spans);
        }
        Entry::Tool { t, tool, text } => {
            let mut spans = vec![time(*t), ("○ ".into(), c.dim), (tool.clone(), c.dim)];
            if let Some(text) = text {
                spans.push((format!("  {}", short_path(text, v.root)), c.dim));
            }
            push(spans);
        }
        Entry::Waiting { t, text } => push(vec![
            time(*t),
            ("◆ needs you".into(), c.warn),
            (
                text.as_deref()
                    .map(|x| format!(": {}", first_line(x)))
                    .unwrap_or_default(),
                c.warn,
            ),
        ]),
        Entry::Done { t } => push(vec![time(*t), ("■ done".into(), c.ok)]),
    }
}

/// Writes `spans` on `row`, cut to the panel's width.
fn put_line(f: &mut Frame, row: usize, spans: &[(String, [u8; 3])], bg: [u8; 3]) {
    let cols = f.cols;
    let mut col = 0;
    let total: usize = spans.iter().map(|(t, _)| t.chars().count()).sum();
    let cut = total > cols;
    for (text, fg) in spans {
        if col >= cols {
            break;
        }
        let room = cols - col - usize::from(cut);
        let piece: String = text.chars().take(room).collect();
        col = f.put(row, col, &piece, *fg, bg);
    }
    if cut && cols > 0 {
        let fg = spans.last().map(|s| s.1).unwrap_or([128; 3]);
        f.put(row, cols - 1, "…", fg, bg);
    }
}

pub fn build(v: &View<'_>, cols: usize, rows: usize, c: &Colors) -> Frame {
    let mut f = Frame::blank(cols, rows, c.fg, c.bg);
    if cols < 8 || rows < 6 {
        return f;
    }
    let s = &v.summary;
    let (state, state_color) = match (s.state, v.running) {
        (State::Waiting, true) => ("NEEDS YOU", c.warn),
        (State::Done, _) => ("DONE", c.ok),
        (_, false) => ("STOPPED", c.dim),
        (State::Idle, true) => ("STARTING", c.dim),
        (State::Working, true) => ("WORKING", c.accent),
    };
    put_line(&mut f, 0, &[(" ◆ FLIGHT LOG".into(), c.accent)], c.bg);
    let state_col = cols.saturating_sub(state.chars().count() + 1);
    f.put(0, state_col, state, state_color, c.bg);
    put_line(&mut f, 1, &[(format!(" {}", v.title), c.fg)], c.bg);
    if let Some(b) = v.branch {
        put_line(&mut f, 2, &[(format!(" ⎇ {b}"), c.dim)], c.bg);
    }
    let mut counts = vec![(
        format!(
            " {} command{}",
            s.commands,
            if s.commands == 1 { "" } else { "s" }
        ),
        c.dim,
    )];
    if s.failed > 0 {
        counts.push((format!(" · {} failed", s.failed), c.bad));
    }
    counts.push((
        format!(" · {} edit{}", s.edits, if s.edits == 1 { "" } else { "s" }),
        c.dim,
    ));
    put_line(&mut f, 3, &counts, c.bg);
    put_line(&mut f, 4, &[("─".repeat(cols), c.dim)], c.bg);

    let footer = if v.focused {
        " ↑↓ Enter output · y copy · c changes · Esc back".to_string()
    } else {
        format!(" {} focus · again to close", v.key)
    };
    put_line(&mut f, rows - 1, &[(footer, c.dim)], c.bg);

    let top = 5;
    let room = rows.saturating_sub(top + 1);
    let mut lines: Vec<Line> = Vec::new();
    for (i, e) in v.entries.iter().enumerate() {
        entry_lines(i, e, v, c, &mut lines);
    }
    if lines.is_empty() {
        put_line(
            &mut f,
            top + 1,
            &[(
                " Nothing yet. What the agent does shows up here.".into(),
                c.dim,
            )],
            c.bg,
        );
        return f;
    }
    // Follow the newest entry, unless one is selected: then keep it in view.
    let mut start = lines.len().saturating_sub(room);
    if let Some(sel) = v.selected {
        let first = lines.iter().position(|l| l.1 == Some(sel));
        let last = lines.iter().rposition(|l| l.1 == Some(sel));
        if let (Some(first), Some(last)) = (first, last) {
            if first < start {
                start = first;
            }
            if last >= start + room {
                start = last + 1 - room.min(last + 1);
            }
        }
    }
    let selected = v
        .selected
        .or_else(|| v.focused.then(|| v.entries.len().saturating_sub(1)));
    for (row, (spans, owner)) in lines.iter().skip(start).take(room).enumerate() {
        let bg = if v.focused && *owner == selected {
            c.selected
        } else {
            c.bg
        };
        if bg != c.bg {
            f.fill(top + row, 0, c.fg, bg);
        }
        put_line(&mut f, top + row, spans, bg);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flight_log::summary;

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

    fn cmd(command: &str, exit: Option<i32>, output: Option<&str>) -> Entry {
        Entry::Command {
            t: 0,
            command: command.into(),
            cwd: None,
            exit,
            failed: false,
            duration_ms: Some(1200),
            output: output.map(str::to_string),
            running: exit.is_none(),
        }
    }

    fn text(f: &Frame) -> Vec<String> {
        (0..f.rows)
            .map(|r| f.row_text(r).0.trim_end().to_string())
            .collect()
    }

    fn view<'a>(entries: &'a [Entry], expanded: &'a HashSet<usize>) -> View<'a> {
        View {
            title: "Claude Code · fix-test",
            branch: Some("agent/fix-test"),
            running: true,
            summary: summary(entries),
            entries,
            selected: None,
            expanded,
            focused: false,
            root: Some("/w/api-fix-test"),
            key: "Ctrl+Shift+L",
        }
    }

    #[test]
    fn header_counts_and_entries_show() {
        let entries = vec![
            Entry::Prompt {
                t: 0,
                text: "fix the test".into(),
            },
            cmd(
                "cargo test",
                Some(101),
                Some("running 3 tests\ntest result: FAILED"),
            ),
            Entry::Edit {
                t: 0,
                path: "/w/api-fix-test/src/a.rs".into(),
                tool: Some("Edit".into()),
            },
            cmd("cargo test", Some(0), Some("ok")),
            Entry::Waiting {
                t: 0,
                text: Some("permission to use Bash".into()),
            },
        ];
        let none = HashSet::new();
        let t = text(&build(&view(&entries, &none), 60, 16, &colors()));
        assert!(
            t[0].starts_with(" ◆ FLIGHT LOG") && t[0].ends_with("NEEDS YOU"),
            "{t:#?}"
        );
        assert_eq!(t[2], " ⎇ agent/fix-test");
        assert_eq!(t[3], " 2 commands · 1 failed · 1 edit");
        let all = t.join("\n");
        assert!(all.contains("» fix the test"), "{all}");
        assert!(all.contains("✗ 101 cargo test  1.2s"), "{all}");
        // A failed command shows its last line of output; a passing one doesn't.
        assert!(all.contains("test result: FAILED"), "{all}");
        assert!(!all.contains("         ok"), "{all}");
        assert!(all.contains("✎ src/a.rs  Edit"), "{all}");
        assert!(all.contains("◆ needs you: permission to use Bash"), "{all}");
        assert!(t[15].contains("Ctrl+Shift+L focus"));
    }

    #[test]
    fn opened_output_and_scrolling_follow_the_selection() {
        let mut entries: Vec<Entry> = (0..30)
            .map(|i| cmd(&format!("step {i}"), Some(0), Some("a\nb\nc")))
            .collect();
        entries.push(Entry::Done { t: 0 });
        let none = HashSet::new();
        // Following: the newest entries are in view.
        let t = text(&build(&view(&entries, &none), 50, 12, &colors()));
        assert!(t.iter().any(|l| l.contains("■ done")), "{t:#?}");
        assert!(!t.iter().any(|l| l.contains("step 0 ")), "{t:#?}");
        // Selecting an old entry and opening it brings it, and its output, into view.
        let open: HashSet<usize> = [3].into();
        let mut v = view(&entries, &open);
        v.selected = Some(3);
        v.focused = true;
        let t = text(&build(&v, 50, 12, &colors()));
        let at = t
            .iter()
            .position(|l| l.contains("step 3"))
            .expect("selected in view");
        assert_eq!(t[at + 1].trim(), "a");
        assert!(t[11].contains("Esc back"));
    }

    #[test]
    fn long_lines_are_cut_and_states_follow_the_agent() {
        let entries = vec![cmd(&"x".repeat(200), None, None)];
        let none = HashSet::new();
        let mut v = view(&entries, &none);
        let f = build(&v, 30, 8, &colors());
        assert!(text(&f).iter().all(|l| l.chars().count() <= 30));
        assert!(text(&f)[0].ends_with("WORKING"));
        v.running = false;
        assert!(text(&build(&v, 30, 8, &colors()))[0].ends_with("STOPPED"));
        let empty: Vec<Entry> = Vec::new();
        let t = text(&build(&view(&empty, &none), 60, 8, &colors()));
        assert!(t.join("\n").contains("Nothing yet"));
    }
}
