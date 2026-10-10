// src/ui/changes_view.rs
//
// The Changes view (Ctrl+Shift+M): what an agent session changed, over
// the whole tab. Files on the left with their +/- counts (and the commits
// it made below them), the selected file's diff on the right, keys or a
// question in the footer.

use crate::changes::{Changes, Kind, Status};
use crate::frame::Frame;

pub struct Colors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub dim: [u8; 3],
    pub accent: [u8; 3],
    pub added: [u8; 3],
    pub removed: [u8; 3],
    pub warn: [u8; 3],
    pub selected: [u8; 3],
}

pub struct View<'a> {
    /// "Claude Code · fix-the-flaky-login".
    pub title: &'a str,
    pub branch: Option<&'a str>,
    pub changes: &'a Changes,
    /// The selected file.
    pub file: usize,
    /// First diff line shown.
    pub scroll: usize,
    /// A question waiting for y/n (reverting a file).
    pub confirm: Option<&'a str>,
}

/// Writes runs on a row from `col`, stopping at `end`; returns the column
/// after them.
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

/// `path` cut from the left to `width` chars ("…/deep/file.rs").
fn fit_path(path: &str, width: usize) -> String {
    let n = path.chars().count();
    if n <= width || width < 2 {
        return path.to_string();
    }
    let tail: String = path.chars().skip(n - (width - 1)).collect();
    format!("…{tail}")
}

/// How many diff lines fit for a view this many rows tall.
pub fn diff_rows(rows: usize) -> usize {
    rows.saturating_sub(3)
}

pub fn build(v: &View<'_>, cols: usize, rows: usize, c: &Colors) -> Frame {
    let mut f = Frame::blank(cols, rows, c.fg, c.bg);
    if cols < 30 || rows < 6 {
        return f;
    }
    let ch = v.changes;
    let (plus, minus) = ch.totals();

    // Header.
    let files = format!(
        "  {} file{}  ",
        ch.files.len(),
        if ch.files.len() == 1 { "" } else { "s" }
    );
    let plus_s = format!("+{plus} ");
    let minus_s = format!("−{minus}");
    let commits = match ch.commits.len() {
        0 => String::new(),
        1 => "  · 1 commit".to_string(),
        n => format!("  · {n} commits"),
    };
    let branch = v.branch.map(|b| format!("  ⎇ {b}")).unwrap_or_default();
    let title = format!("  {}", v.title);
    put_runs(
        &mut f,
        0,
        0,
        cols,
        &[
            (" ◆ CHANGES", c.accent),
            (&title, c.fg),
            (&branch, c.dim),
            (&files, c.dim),
            (&plus_s, c.added),
            (&minus_s, c.removed),
            (&commits, c.dim),
        ],
        c.bg,
    );
    let rule = "─".repeat(cols);
    f.put(1, 0, &rule, c.dim, c.bg);

    // Footer.
    match v.confirm {
        Some(q) => {
            let text = format!(" {q}  y / n");
            put_runs(&mut f, rows - 1, 0, cols, &[(&text, c.warn)], c.bg);
        }
        None => {
            let keys = " ↑↓ file · PgUp PgDn scroll · e edit · r revert · y copy path · Esc close";
            put_runs(&mut f, rows - 1, 0, cols, &[(keys, c.dim)], c.bg);
        }
    }

    let body = 2..rows - 1;
    if ch.files.is_empty() {
        let text = " No changes yet: the worktree matches where the agent started.";
        put_runs(&mut f, body.start + 1, 0, cols, &[(text, c.dim)], c.bg);
        return f;
    }

    // Files (and commits) on the left.
    let left = (cols * 30 / 100).clamp(24, 48).min(cols / 2);
    let height = body.len();
    let first = v.file.saturating_sub(height.saturating_sub(1));
    let mut row = body.start;
    for (i, file) in ch.files.iter().enumerate().skip(first).take(height) {
        let bg = if i == v.file { c.selected } else { c.bg };
        if bg != c.bg {
            for col in 0..left {
                f.put(row, col, " ", c.fg, bg);
            }
        }
        let (mark, color) = match file.status {
            Status::Modified => ("M", c.warn),
            Status::Added => ("A", c.added),
            Status::Deleted => ("D", c.removed),
            Status::Renamed => ("R", c.accent),
        };
        let counts = if file.binary {
            "bin".to_string()
        } else {
            format!("+{} −{}", file.added, file.removed)
        };
        let room = left.saturating_sub(counts.chars().count() + 5);
        let path = fit_path(&file.path, room);
        put_runs(
            &mut f,
            row,
            0,
            left,
            &[(" ", c.fg), (mark, color), (" ", c.fg), (&path, c.fg)],
            bg,
        );
        let at = left.saturating_sub(counts.chars().count() + 1);
        f.put(row, at, &counts, c.dim, bg);
        row += 1;
    }
    if row + 2 < body.end && !ch.commits.is_empty() {
        row += 1;
        put_runs(&mut f, row, 0, left, &[(" commits", c.dim)], c.bg);
        row += 1;
        for commit in &ch.commits {
            if row >= body.end {
                break;
            }
            let text = format!(" {commit}");
            put_runs(&mut f, row, 0, left, &[(&text, c.dim)], c.bg);
            row += 1;
        }
    }
    for r in body.clone() {
        f.put(r, left, "│", c.dim, c.bg);
    }

    // The diff on the right.
    let Some(file) = ch.files.get(v.file) else {
        return f;
    };
    let x = left + 2;
    let mut row = body.start;
    if let Some(old) = &file.old_path {
        let text = format!("renamed from {old}");
        put_runs(&mut f, row, x, cols, &[(&text, c.accent)], c.bg);
        row += 1;
    }
    if file.lines.is_empty() {
        put_runs(
            &mut f,
            row,
            x,
            cols,
            &[("(empty, or too many new files to show)", c.dim)],
            c.bg,
        );
    }
    for line in file.lines.iter().skip(v.scroll) {
        if row >= body.end {
            break;
        }
        let text = line.text.replace('\t', "    ");
        let gutter = line
            .new_line
            .map(|n| format!("{n:>5} "))
            .unwrap_or_else(|| "      ".into());
        let (sign, color) = match line.kind {
            Kind::Hunk => ("", c.accent),
            Kind::Added => ("+", c.added),
            Kind::Removed => ("-", c.removed),
            Kind::Context => (" ", c.fg),
            Kind::Note => ("", c.dim),
        };
        put_runs(
            &mut f,
            row,
            x,
            cols,
            &[(&gutter, c.dim), (sign, color), (&text, color)],
            c.bg,
        );
        row += 1;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::parse;

    fn colors() -> Colors {
        Colors {
            fg: [200; 3],
            bg: [0; 3],
            dim: [100; 3],
            accent: [0, 200, 200],
            added: [0, 200, 0],
            removed: [200, 0, 0],
            warn: [200, 200, 0],
            selected: [40; 3],
        }
    }

    fn text(f: &Frame) -> Vec<String> {
        (0..f.rows)
            .map(|r| f.row_text(r).0.trim_end().to_string())
            .collect()
    }

    fn sample() -> Changes {
        Changes {
            files: parse(
                "diff --git a/app/config.py b/app/config.py\n--- a/app/config.py\n+++ b/app/config.py\n@@ -3,2 +3,2 @@\n def load():\n-    old\n+    new\n\
                 diff --git a/a/very/deep/folder/structure/file_name.rs b/a/very/deep/folder/structure/file_name.rs\nnew file mode 100644\n--- /dev/null\n+++ b/a/very/deep/folder/structure/file_name.rs\n@@ -0,0 +1 @@\n+fn x() {}\n",
            ),
            commits: vec!["abc1234 Default DATABASE_URL".into()],
        }
    }

    #[test]
    fn files_counts_and_the_selected_diff_show() {
        let ch = sample();
        let v = View {
            title: "Demo agent · fix",
            branch: Some("agent/fix"),
            changes: &ch,
            file: 0,
            scroll: 0,
            confirm: None,
        };
        let t = text(&build(&v, 100, 14, &colors()));
        assert!(
            t[0].contains("◆ CHANGES  Demo agent · fix  ⎇ agent/fix  2 files  +2 −1  · 1 commit"),
            "{t:#?}"
        );
        assert!(
            t[2].contains(" M app/config.py") && t[2].contains("+1 −1"),
            "{t:#?}"
        );
        // Long paths keep their end.
        assert!(
            t[3].contains(" A …") && t[3].contains("file_name.rs"),
            "{t:#?}"
        );
        assert!(t.iter().any(|l| l.contains("abc1234 Default DATABASE_URL")));
        let all = t.join("\n");
        assert!(all.contains("@@ -3,2 +3,2 @@"), "{all}");
        assert!(all.contains("    3  def load():"), "{all}");
        assert!(all.contains("-    old"), "{all}");
        assert!(all.contains("    4 +    new"), "{all}");
        assert!(t[13].contains("r revert"));
    }

    #[test]
    fn questions_replace_the_keys_and_empty_says_so() {
        let ch = sample();
        let v = View {
            title: "x",
            branch: None,
            changes: &ch,
            file: 1,
            scroll: 0,
            confirm: Some("Revert app/config.py?"),
        };
        let t = text(&build(&v, 80, 10, &colors()));
        assert!(t[9].contains("Revert app/config.py?  y / n"));
        assert!(t.join("\n").contains("+fn x() {}"));
        let empty = Changes::default();
        let v = View {
            changes: &empty,
            confirm: None,
            ..v
        };
        assert!(text(&build(&v, 80, 10, &colors()))
            .join("\n")
            .contains("No changes yet"));
    }
}
