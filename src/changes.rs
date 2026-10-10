// src/changes.rs
//
// What an agent session changed: its worktree compared with the commit it
// started from -- commits it made and work not committed yet, plus new
// files git doesn't track yet -- as files with their diffs. Also puts a
// file back the way it was when the agent started.
//
// All of it is plain `git` in the worktree, so it's the same whichever
// agent did the work.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Most new (untracked) files diffed; beyond that they're listed only.
const MAX_NEW_FILES: usize = 200;
/// Longest diff kept per file, in lines.
const MAX_LINES: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `@@ -1,3 +1,4 @@`
    Hunk,
    Added,
    Removed,
    Context,
    /// "\ No newline at end of file", "Binary files differ", ...
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
    /// The line number in the new file (for Added / Context lines).
    pub new_line: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Modified,
    Added,
    Deleted,
    Renamed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// Relative to the worktree.
    pub path: String,
    pub old_path: Option<String>,
    pub status: Status,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<Line>,
    pub binary: bool,
}

impl FileDiff {
    /// Where to open it in an editor: the first changed line.
    pub fn first_line(&self) -> u32 {
        self.lines
            .iter()
            .find(|l| l.kind == Kind::Added)
            .or_else(|| self.lines.iter().find(|l| l.new_line.is_some()))
            .and_then(|l| l.new_line)
            .unwrap_or(1)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub files: Vec<FileDiff>,
    /// Commits the agent made: "abc1234 subject", newest first.
    pub commits: Vec<String>,
}

impl Changes {
    pub fn totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }
}

/// Parses `git diff` output (unified, no color).
pub fn parse(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut new_line = 0u32;
    for raw in diff.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            // "a/x b/x": the b side is the path (may be refined below).
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            files.push(FileDiff {
                path,
                old_path: None,
                status: Status::Modified,
                added: 0,
                removed: 0,
                lines: Vec::new(),
                binary: false,
            });
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if f.lines.is_empty() {
            // Header lines, before the first hunk.
            if raw.starts_with("new file mode") {
                f.status = Status::Added;
            } else if raw.starts_with("deleted file mode") {
                f.status = Status::Deleted;
            } else if let Some(from) = raw.strip_prefix("rename from ") {
                f.status = Status::Renamed;
                f.old_path = Some(from.to_string());
            } else if let Some(to) = raw.strip_prefix("rename to ") {
                f.path = to.to_string();
            } else if let Some(p) = raw.strip_prefix("+++ ") {
                if let Some(p) = p.strip_prefix("b/") {
                    f.path = p.to_string();
                }
            } else if raw.starts_with("Binary files ") {
                f.binary = true;
                f.lines.push(Line {
                    kind: Kind::Note,
                    text: "Binary file".into(),
                    new_line: None,
                });
            }
            if !raw.starts_with("@@") {
                continue;
            }
        }
        if f.lines.len() >= MAX_LINES {
            continue;
        }
        let (kind, text) = if raw.starts_with("@@") {
            // "@@ -a,b +c,d @@ context": new lines start at c.
            new_line = raw
                .split_whitespace()
                .find_map(|w| w.strip_prefix('+'))
                .and_then(|w| w.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            (Kind::Hunk, raw.to_string())
        } else if let Some(t) = raw.strip_prefix('+') {
            f.added += 1;
            (Kind::Added, t.to_string())
        } else if let Some(t) = raw.strip_prefix('-') {
            f.removed += 1;
            (Kind::Removed, t.to_string())
        } else if let Some(t) = raw.strip_prefix(' ') {
            (Kind::Context, t.to_string())
        } else if raw.starts_with('\\') {
            (Kind::Note, raw.to_string())
        } else {
            continue;
        };
        let at = matches!(kind, Kind::Added | Kind::Context).then_some(new_line);
        if at.is_some() {
            new_line += 1;
        }
        f.lines.push(Line {
            kind,
            text,
            new_line: at,
        });
    }
    files
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    // `diff --no-index` exits 1 when files differ.
    if out.status.success() || (args.contains(&"--no-index") && out.status.code() == Some(1)) {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Everything changed in `worktree` since `base`.
pub fn collect(worktree: &Path, base: &str) -> Result<Changes, String> {
    let diff = git(
        worktree,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            // Whatever diff.mnemonicPrefix / diff.noprefix say.
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
            "--",
        ],
    )?;
    let mut files = parse(&diff);
    let new = git(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    for (i, path) in new.split('\0').filter(|p| !p.is_empty()).enumerate() {
        if i < MAX_NEW_FILES {
            let d = git(
                worktree,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    "--no-index",
                    "--",
                    "/dev/null",
                    path,
                ],
            )
            .unwrap_or_default();
            if let Some(mut f) = parse(&d).into_iter().next() {
                f.path = path.to_string();
                f.status = Status::Added;
                files.push(f);
                continue;
            }
        }
        files.push(FileDiff {
            path: path.to_string(),
            old_path: None,
            status: Status::Added,
            added: 0,
            removed: 0,
            lines: Vec::new(),
            binary: false,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let commits = git(
        worktree,
        &["log", "--format=%h %s", &format!("{base}..HEAD")],
    )
    .map(|l| l.lines().map(str::to_string).collect())
    .unwrap_or_default();
    Ok(Changes { files, commits })
}

/// Puts one file back the way it was at `base`: restored if it existed
/// then, removed if the agent created it.
pub fn revert(worktree: &Path, base: &str, path: &str) -> Result<(), String> {
    let rel = Path::new(path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!("{path}: not inside the worktree"));
    }
    let existed = git(worktree, &["cat-file", "-e", &format!("{base}:{path}")]).is_ok();
    if existed {
        git(worktree, &["checkout", base, "--", path])?;
    } else {
        let full: PathBuf = worktree.join(rel);
        let _ = git(
            worktree,
            &["rm", "--cached", "--quiet", "--ignore-unmatch", "--", path],
        );
        if full.exists() {
            std::fs::remove_file(&full).map_err(|e| format!("{path}: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/app/config.py b/app/config.py
index 1111111..2222222 100644
--- a/app/config.py
+++ b/app/config.py
@@ -3,4 +3,4 @@ import os
 def load():
     return {
-        \"database_url\": os.environ[\"DATABASE_URL\"],
+        \"database_url\": os.environ.get(\"DATABASE_URL\", \"sqlite:///dev.db\"),
         \"port\": 3000,
diff --git a/notes.md b/notes.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/notes.md
@@ -0,0 +1,2 @@
+# Notes
+done
\\ No newline at end of file
diff --git a/old.txt b/new.txt
similarity index 100%
rename from old.txt
rename to new.txt
diff --git a/logo.png b/logo.png
index 4444444..5555555 100644
Binary files a/logo.png and b/logo.png differ
";

    #[test]
    fn diffs_parse_into_files() {
        let files = parse(DIFF);
        assert_eq!(files.len(), 4);
        let c = &files[0];
        assert_eq!(
            (c.path.as_str(), c.status, c.added, c.removed),
            ("app/config.py", Status::Modified, 1, 1)
        );
        assert_eq!(c.lines[0].kind, Kind::Hunk);
        // Line numbers count in the new file from the hunk header.
        assert_eq!(c.lines[1].new_line, Some(3));
        assert_eq!(c.first_line(), 5);
        let n = &files[1];
        assert_eq!((n.status, n.added), (Status::Added, 2));
        assert_eq!(n.lines.last().unwrap().kind, Kind::Note);
        let r = &files[2];
        assert_eq!(
            (r.status, r.path.as_str(), r.old_path.as_deref()),
            (Status::Renamed, "new.txt", Some("old.txt"))
        );
        assert!(files[3].binary);
    }

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    #[test]
    fn collects_commits_work_in_progress_and_new_files_then_reverts() {
        let dir = std::env::temp_dir().join(format!(
            "cyberterm-changes-{}-{}",
            std::process::id(),
            crate::shell::tap::now_ms()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        run(&dir, &["config", "user.email", "t@example.com"]);
        run(&dir, &["config", "user.name", "t"]);
        // Personal diff settings mustn't change what's parsed.
        run(&dir, &["config", "diff.mnemonicPrefix", "true"]);
        std::fs::write(dir.join("src/a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("b.txt"), "keep\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-qm", "init"]);
        let base = git(&dir, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        // The agent commits one change, leaves another uncommitted, and
        // creates a file.
        std::fs::write(dir.join("src/a.txt"), "one\nTWO\n").unwrap();
        run(&dir, &["commit", "-qam", "Shout two"]);
        std::fs::write(dir.join("b.txt"), "keep\nmore\n").unwrap();
        std::fs::write(dir.join("new.txt"), "hello\n").unwrap();

        let c = collect(&dir, &base).unwrap();
        let paths: Vec<&str> = c.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["b.txt", "new.txt", "src/a.txt"]);
        assert_eq!(c.totals(), (3, 1));
        assert_eq!(c.commits.len(), 1);
        assert!(c.commits[0].ends_with("Shout two"));
        assert_eq!(c.files[1].status, Status::Added);

        revert(&dir, &base, "src/a.txt").unwrap();
        revert(&dir, &base, "new.txt").unwrap();
        assert!(!dir.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("src/a.txt")).unwrap(),
            "one\ntwo\n"
        );
        let c = collect(&dir, &base).unwrap();
        assert_eq!(c.files.len(), 1, "{c:?}");
        assert!(revert(&dir, &base, "../escape").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
