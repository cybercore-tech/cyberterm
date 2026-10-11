// src/agent_merge.rs
//
// Bringing an agent session's work back, or throwing it away:
// `cyberterm +agent merge <id>` and `cyberterm +agent discard <id>`.
//
// Merge takes the session's branch (agent/<id>) -- with whatever the agent
// left uncommitted committed onto it first -- into the branch the session
// started from, in your main checkout: as one squashed commit by default
// (`[agents] merge = "squash"`), a merge commit, or a fast-forward. It
// checks everything first and changes nothing when it can't finish: the
// agent still running, your checkout on another branch or with
// uncommitted changes, or conflicts (found with `git merge-tree`, which
// touches no files). Afterwards the worktree, branch and session go,
// unless `--keep`.
//
// Discard removes a session with its worktree and branch, work and all,
// after asking.

use crate::agent::{git, Session};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Strategy {
    /// One commit on top of the target branch.
    Squash,
    /// A merge commit, keeping the agent's commits.
    Merge,
    /// Only when the target hasn't moved since the agent started.
    FastForward,
}

impl Strategy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "squash" => Some(Strategy::Squash),
            "merge" => Some(Strategy::Merge),
            "ff" | "fast-forward" => Some(Strategy::FastForward),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Strategy::Squash => "squash",
            Strategy::Merge => "merge commit",
            Strategy::FastForward => "fast-forward",
        }
    }
}

/// What merging a session would do, found without changing anything.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub branch: String,
    /// The branch it goes into.
    pub onto: String,
    pub repo: PathBuf,
    /// The agent's commits not yet on `onto`: "abc1234 subject", oldest
    /// first.
    pub commits: Vec<String>,
    /// Files it changed without committing (committed first on merge).
    pub uncommitted: usize,
    /// Files that conflict with `onto`.
    pub conflicts: Vec<String>,
    /// Why it can't merge right now, conflicts aside.
    pub blocked: Option<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty() && self.uncommitted == 0
    }

    pub fn ready(&self) -> bool {
        self.blocked.is_none() && self.conflicts.is_empty() && !self.is_empty()
    }
}

/// Runs git with extra environment; (success, stdout, stderr).
fn git_env(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> (bool, String, String) {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    match cmd.output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ),
        Err(e) => (false, String::new(), format!("git: {e}")),
    }
}

fn current_branch(repo: &Path) -> Option<String> {
    git(repo, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty())
}

/// The worktree's state as a commit -- uncommitted and untracked files
/// included -- made through a scratch index, so neither the worktree's
/// index nor any branch changes. The commit stays unreferenced.
fn snapshot(worktree: &Path) -> Result<String, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = std::env::temp_dir().join(format!(
        "cyberterm-merge-index-{}-{}-{n}",
        std::process::id(),
        crate::shell::tap::now_ms()
    ));
    let env = [("GIT_INDEX_FILE", index.as_path())];
    let result = (|| {
        for args in [&["read-tree", "HEAD"][..], &["add", "-A"][..]] {
            let (ok, _, err) = git_env(worktree, args, &env);
            if !ok {
                return Err(format!("git {}: {err}", args[0]));
            }
        }
        let (ok, tree, err) = git_env(worktree, &["write-tree"], &env);
        if !ok {
            return Err(format!("git write-tree: {err}"));
        }
        let (ok, commit, err) = git_env(
            worktree,
            &[
                "commit-tree",
                &tree,
                "-p",
                "HEAD",
                "-m",
                "cyberterm merge check",
            ],
            &[],
        );
        if !ok {
            return Err(format!("git commit-tree: {err}"));
        }
        Ok(commit)
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// What merging `s` would do. `running`: whether its agent still runs.
pub fn plan(s: &Session, running: bool, strategy: Strategy) -> Result<Plan, String> {
    let Some(w) = &s.worktree else {
        return Err(format!(
            "{} ran in place, without a worktree: its changes are already in {}",
            s.id,
            crate::agent::short(&s.dir)
        ));
    };
    if !w.path.is_dir() {
        return Err(format!(
            "{}'s worktree is gone ({})",
            s.id,
            crate::agent::short(&w.path)
        ));
    }
    let current = current_branch(&w.repo);
    let onto = w
        .onto
        .clone()
        .or_else(|| current.clone())
        .ok_or_else(|| format!("{} has no branch checked out", crate::agent::short(&w.repo)))?;
    let uncommitted = git(&w.path, &["status", "--porcelain"])
        .map(|o| o.lines().count())
        .unwrap_or(0);
    let commits: Vec<String> = git(
        &w.repo,
        &[
            "log",
            "--reverse",
            "--format=%h %s",
            &format!("{onto}..{}", w.branch),
        ],
    )?
    .lines()
    .map(str::to_string)
    .collect();

    let repo_shown = crate::agent::short(&w.repo);
    let blocked = if running {
        Some(format!(
            "{} is still running: let it finish, or quit it first",
            s.id
        ))
    } else if current.as_deref() != Some(onto.as_str()) {
        Some(format!(
            "{repo_shown} is on {}, not {onto}: switch to it first (git switch {onto})",
            current.as_deref().unwrap_or("a detached HEAD")
        ))
    } else if git(&w.repo, &["status", "--porcelain", "--untracked-files=no"])
        .is_ok_and(|o| !o.is_empty())
    {
        Some(format!(
            "{repo_shown} has uncommitted changes: commit or stash them first"
        ))
    } else if strategy == Strategy::FastForward
        && !git_env(
            &w.repo,
            &["merge-base", "--is-ancestor", &onto, &w.branch],
            &[],
        )
        .0
    {
        Some(format!(
            "{onto} has moved on since {} started, so it can't fast-forward: squash or merge instead",
            s.id
        ))
    } else {
        None
    };

    let mut conflicts = Vec::new();
    if commits.len() + uncommitted > 0 {
        let head = if uncommitted > 0 {
            snapshot(&w.path)?
        } else {
            w.branch.clone()
        };
        let (ok, out, err) = git_env(
            &w.repo,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                &onto,
                &head,
            ],
            &[],
        );
        if !ok {
            if out.is_empty() {
                return Err(format!("git merge-tree: {err}"));
            }
            // The tree, then the conflicting files.
            conflicts = out.lines().skip(1).map(str::to_string).collect();
            conflicts.dedup();
        }
    }
    Ok(Plan {
        branch: w.branch.clone(),
        onto,
        repo: w.repo.clone(),
        commits,
        uncommitted,
        conflicts,
        blocked,
    })
}

/// The squashed commit's message: the task as its subject, the agent's
/// own commits in the body.
fn squash_message(s: &Session, commits: &[String]) -> (String, String) {
    let subject = s
        .task
        .as_deref()
        .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(|t| {
            if t.chars().count() > 72 {
                format!("{}…", t.chars().take(71).collect::<String>())
            } else {
                t.to_string()
            }
        })
        .unwrap_or_else(|| format!("Work from {} session {}", s.label, s.id));
    let mut body = format!("From {} (agent session {}).", s.label, s.id);
    if !commits.is_empty() {
        body.push_str("\n\nIts commits:");
        for c in commits.iter().take(30) {
            body.push_str(&format!("\n- {c}"));
        }
        if commits.len() > 30 {
            body.push_str(&format!("\n- and {} more", commits.len() - 30));
        }
    }
    (subject, body)
}

/// Merges session `id` from the sessions in `state`; returns what it did.
pub fn merge_in(state: &Path, id: &str, strategy: Strategy, keep: bool) -> Result<String, String> {
    let s = crate::agent::load_in(state, id)?;
    let running = crate::agent::status(&s).running;
    let p = plan(&s, running, strategy)?;
    if let Some(why) = &p.blocked {
        return Err(why.clone());
    }
    if p.is_empty() {
        return Ok(format!("nothing to merge: {id} changed nothing"));
    }
    let w = s.worktree.as_ref().expect("plan needs a worktree");
    if !p.conflicts.is_empty() {
        return Err(format!(
            "{id} conflicts with {} in: {}\n   Nothing was changed. Ask the agent to merge {} into its branch and resolve them, or do it in {}",
            p.onto,
            p.conflicts.join(", "),
            p.onto,
            crate::agent::short(&w.path)
        ));
    }

    // Whatever the agent left uncommitted goes onto its branch first.
    let (subject, _) = squash_message(&s, &[]);
    if p.uncommitted > 0 {
        git(&w.path, &["add", "-A"])?;
        git(
            &w.path,
            &[
                "commit",
                "-q",
                "-m",
                &format!("{subject} (uncommitted work)"),
            ],
        )?;
    }
    let commits: Vec<String> = git(
        &w.repo,
        &[
            "log",
            "--reverse",
            "--format=%h %s",
            &format!("{}..{}", p.onto, w.branch),
        ],
    )?
    .lines()
    .map(str::to_string)
    .collect();

    match strategy {
        Strategy::Squash => {
            let (subject, body) = squash_message(&s, &commits);
            git(&w.repo, &["merge", "--squash", "-q", &w.branch])?;
            if let Err(e) = git(&w.repo, &["commit", "-q", "-m", &subject, "-m", &body]) {
                let _ = git(&w.repo, &["reset", "-q", "--merge"]);
                return Err(format!(
                    "the squashed commit failed, so the merge was undone: {e}"
                ));
            }
        }
        Strategy::Merge => {
            let message = format!("Merge {}: {subject}", w.branch);
            if let Err(e) = git(
                &w.repo,
                &["merge", "--no-ff", "-q", "-m", &message, &w.branch],
            ) {
                let _ = git(&w.repo, &["merge", "--abort"]);
                return Err(format!("the merge failed and was undone: {e}"));
            }
        }
        Strategy::FastForward => {
            git(&w.repo, &["merge", "--ff-only", "-q", &w.branch])?;
        }
    }
    let head = git(&w.repo, &["rev-parse", "--short", "HEAD"])?;
    let n = commits.len();
    let mut note = format!(
        "merged into {} as {head} ({}, {n} commit{})",
        p.onto,
        strategy.name(),
        if n == 1 { "" } else { "s" }
    );
    if !keep {
        crate::agent::remove_in(state, id, true)?;
        if crate::agent::branch_exists(&w.repo, &w.branch) {
            git(&w.repo, &["branch", "-D", &w.branch])?;
        }
        note.push_str("; removed its worktree, branch and session");
    }
    Ok(note)
}

/// Removes session `id` with its worktree and branch, work and all.
pub fn discard_in(state: &Path, id: &str) -> Result<String, String> {
    let s = crate::agent::load_in(state, id)?;
    let note = crate::agent::remove_in(state, id, true)?;
    if let Some(w) = &s.worktree {
        if crate::agent::branch_exists(&w.repo, &w.branch) {
            git(&w.repo, &["branch", "-D", &w.branch])?;
            return Ok(format!("{note}, deleted branch {}", w.branch));
        }
    }
    Ok(note)
}

// ----------------------------------------------------------------------
// CLI
// ----------------------------------------------------------------------

fn describe(p: &Plan, strategy: Strategy, id: &str) -> String {
    let mut out = format!(
        "{id} → {}  ({}, in {})\n",
        p.onto,
        strategy.name(),
        crate::agent::short(&p.repo)
    );
    let n = p.commits.len();
    out.push_str(&format!("  {n} commit{}", if n == 1 { "" } else { "s" }));
    if p.uncommitted > 0 {
        out.push_str(&format!(
            ", {} uncommitted file{} (committed first)",
            p.uncommitted,
            if p.uncommitted == 1 { "" } else { "s" }
        ));
    }
    out.push('\n');
    for c in p.commits.iter().take(10) {
        out.push_str(&format!("    {c}\n"));
    }
    if p.is_empty() {
        out.push_str("  nothing to merge\n");
    } else if p.conflicts.is_empty() {
        out.push_str("  ✓ no conflicts\n");
    } else {
        out.push_str(&format!("  ✗ conflicts in: {}\n", p.conflicts.join(", ")));
    }
    if let Some(why) = &p.blocked {
        out.push_str(&format!("  ✗ {why}\n"));
    }
    out
}

/// `cyberterm +agent merge <id> [--squash|--merge|--ff] [--keep] [--check]`
pub fn run_merge(args: &[String], cfg: &crate::config::AgentsConfig) -> i32 {
    let mut strategy = Strategy::parse(&cfg.merge).unwrap_or(Strategy::Squash);
    let mut keep = false;
    let mut check = false;
    let mut id = None;
    for a in args {
        match a.as_str() {
            "--squash" => strategy = Strategy::Squash,
            "--merge" => strategy = Strategy::Merge,
            "--ff" | "--fast-forward" => strategy = Strategy::FastForward,
            "--keep" => keep = true,
            "--check" | "--dry-run" | "-n" => check = true,
            s if s.starts_with('-') => {
                eprintln!("cyberterm +agent merge: unknown option {s}");
                return 2;
            }
            s => id = Some(s.to_string()),
        }
    }
    let Some(id) = id else {
        eprintln!("usage: cyberterm +agent merge <id> [--squash|--merge|--ff] [--keep] [--check]");
        return 2;
    };
    let state = crate::agent::state_dir();
    if check {
        let s = match crate::agent::load_in(&state, &id) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("❌ {e}");
                return 1;
            }
        };
        let running = crate::agent::status(&s).running;
        return match plan(&s, running, strategy) {
            Ok(p) => {
                print!("{}", describe(&p, strategy, &id));
                if p.ready() {
                    0
                } else {
                    1
                }
            }
            Err(e) => {
                eprintln!("❌ {e}");
                1
            }
        };
    }
    match merge_in(&state, &id, strategy, keep) {
        Ok(note) => {
            println!("✓ {id}: {note}");
            0
        }
        Err(e) => {
            eprintln!("❌ {e}");
            1
        }
    }
}

/// `cyberterm +agent discard <id>... [--yes]`
pub fn run_discard(args: &[String]) -> i32 {
    use std::io::{BufRead, IsTerminal, Write};
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let ids: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if ids.is_empty() {
        eprintln!("usage: cyberterm +agent discard <id>... [--yes]");
        return 2;
    }
    let state = crate::agent::state_dir();
    let mut code = 0;
    for id in ids {
        let s = match crate::agent::load_in(&state, id) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("❌ {e}");
                code = 1;
                continue;
            }
        };
        let st = crate::agent::status(&s);
        if !yes {
            if !std::io::stdin().is_terminal() {
                eprintln!("❌ {id}: discarding throws its work away; add --yes to confirm");
                code = 1;
                continue;
            }
            let what = match &s.worktree {
                Some(w) => format!(
                    "{} changed file(s) and {} commit(s) on {} will be lost",
                    st.changed, st.commits, w.branch
                ),
                None => "its record and flight log will be removed".into(),
            };
            print!("Discard {id}? {what}. [y/N] ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().lock().read_line(&mut answer);
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                println!("Kept {id}.");
                continue;
            }
        }
        match discard_in(&state, id) {
            Ok(note) => println!("✓ {id}: {note}"),
            Err(e) => {
                eprintln!("❌ {e}");
                code = 1;
            }
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{create_in, Launcher, Prompt, Request};
    use crate::config::AgentsConfig;

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
        state: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "cyberterm-merge-{name}-{}-{}",
            std::process::id(),
            crate::shell::tap::now_ms()
        ));
        let repo = root.join("api");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"][..],
            &["config", "user.name", "t"][..],
        ] {
            git(&repo, args).unwrap();
        }
        std::fs::write(repo.join("app.py"), "a = 1\nb = 2\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-qm", "init"]).unwrap();
        Fixture {
            state: root.join("state"),
            root,
            repo,
        }
    }

    fn session(f: &Fixture, task: &str) -> Session {
        let l = Launcher {
            name: "demo".into(),
            label: "Demo agent".into(),
            argv: vec!["true".into()],
            prompt: Prompt::Positional,
        };
        create_in(
            &f.state,
            &AgentsConfig::default(),
            Request {
                launcher: &l,
                task: Some(task.into()),
                cwd: f.repo.clone(),
                worktree: true,
            },
        )
        .unwrap()
    }

    fn wt(s: &Session) -> PathBuf {
        s.worktree.as_ref().unwrap().path.clone()
    }

    fn log(repo: &Path) -> Vec<String> {
        git(repo, &["log", "--format=%s"])
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn squash_merges_committed_and_uncommitted_work_then_cleans_up() {
        let f = fixture("squash");
        let s = session(&f, "Give DATABASE_URL a default");
        assert_eq!(s.worktree.as_ref().unwrap().onto.as_deref(), Some("main"));
        // One commit of its own, plus a file it never committed.
        std::fs::write(wt(&s).join("app.py"), "a = 1\nb = 3\n").unwrap();
        git(&wt(&s), &["commit", "-qam", "change b"]).unwrap();
        std::fs::write(wt(&s).join("new.py"), "x = 1\n").unwrap();

        let p = plan(&s, false, Strategy::Squash).unwrap();
        assert_eq!((p.commits.len(), p.uncommitted), (1, 1), "{p:?}");
        assert!(p.commits[0].ends_with(" change b"));
        assert!(p.ready(), "{p:?}");
        // Planning changed nothing in the worktree.
        assert_eq!(
            git(&wt(&s), &["status", "--porcelain"]).unwrap(),
            "?? new.py"
        );

        let note = merge_in(&f.state, &s.id, Strategy::Squash, false).unwrap();
        assert!(note.starts_with("merged into main as "), "{note}");
        assert!(note.contains("squash, 2 commits"), "{note}");
        assert_eq!(log(&f.repo), ["Give DATABASE_URL a default", "init"]);
        let body = git(&f.repo, &["log", "-1", "--format=%b"]).unwrap();
        assert!(
            body.contains(&format!("From Demo agent (agent session {}).", s.id)),
            "{body}"
        );
        assert!(body.contains("(uncommitted work)"), "{body}");
        assert!(body.contains("change b"), "{body}");
        assert_eq!(
            std::fs::read_to_string(f.repo.join("app.py")).unwrap(),
            "a = 1\nb = 3\n"
        );
        assert!(f.repo.join("new.py").is_file());
        // Worktree, branch and session gone.
        assert!(!wt(&s).exists());
        assert!(!crate::agent::branch_exists(
            &f.repo,
            &format!("agent/{}", s.id)
        ));
        assert!(crate::agent::load_in(&f.state, &s.id).is_err());
    }

    #[test]
    fn merge_commit_and_fast_forward_keep_the_agents_commits() {
        let f = fixture("ff");
        let s = session(&f, "one");
        std::fs::write(wt(&s).join("one.txt"), "1\n").unwrap();
        git(&wt(&s), &["add", "."]).unwrap();
        git(&wt(&s), &["commit", "-qm", "add one"]).unwrap();
        merge_in(&f.state, &s.id, Strategy::FastForward, true).unwrap();
        assert_eq!(log(&f.repo), ["add one", "init"]);
        // --keep: everything still there.
        assert!(wt(&s).is_dir());
        assert!(crate::agent::load_in(&f.state, &s.id).is_ok());

        let t = session(&f, "two");
        std::fs::write(wt(&t).join("two.txt"), "2\n").unwrap();
        git(&wt(&t), &["add", "."]).unwrap();
        git(&wt(&t), &["commit", "-qm", "add two"]).unwrap();
        // main moves on: no fast-forward, but a merge commit works.
        std::fs::write(f.repo.join("three.txt"), "3\n").unwrap();
        git(&f.repo, &["add", "."]).unwrap();
        git(&f.repo, &["commit", "-qm", "add three"]).unwrap();
        let p = plan(&t, false, Strategy::FastForward).unwrap();
        assert!(
            p.blocked.as_deref().unwrap().contains("can't fast-forward"),
            "{p:?}"
        );
        merge_in(&f.state, &t.id, Strategy::Merge, false).unwrap();
        let subjects = log(&f.repo);
        assert_eq!(subjects[0], "Merge agent/two: two");
        assert!(subjects.contains(&"add two".to_string()));
    }

    #[test]
    fn conflicts_and_a_busy_checkout_change_nothing() {
        let f = fixture("conflict");
        let s = session(&f, "edit b");
        std::fs::write(wt(&s).join("app.py"), "a = 1\nb = 3\n").unwrap();
        // Uncommitted in the worktree, and main changes the same line.
        std::fs::write(f.repo.join("app.py"), "a = 1\nb = 4\n").unwrap();
        git(&f.repo, &["commit", "-qam", "b is 4"]).unwrap();

        let p = plan(&s, false, Strategy::Squash).unwrap();
        assert_eq!(p.conflicts, ["app.py"], "{p:?}");
        let err = merge_in(&f.state, &s.id, Strategy::Squash, false).unwrap_err();
        assert!(err.contains("conflicts with main in: app.py"), "{err}");
        assert!(err.contains("Nothing was changed"), "{err}");
        // Untouched: the agent's uncommitted edit, main, the session.
        assert_eq!(
            git(&wt(&s), &["status", "--porcelain"]).unwrap(),
            " M app.py"
        );
        assert_eq!(log(&f.repo), ["b is 4", "init"]);
        assert!(wt(&s).is_dir());

        // Your own uncommitted changes, or another branch, block it.
        let t = session(&f, "other");
        std::fs::write(wt(&t).join("other.txt"), "o\n").unwrap();
        std::fs::write(f.repo.join("app.py"), "mine\n").unwrap();
        let p = plan(&t, false, Strategy::Squash).unwrap();
        assert!(
            p.blocked
                .as_deref()
                .unwrap()
                .contains("uncommitted changes"),
            "{p:?}"
        );
        git(&f.repo, &["checkout", "-q", "--", "app.py"]).unwrap();
        git(&f.repo, &["switch", "-q", "-c", "elsewhere"]).unwrap();
        let p = plan(&t, false, Strategy::Squash).unwrap();
        assert!(
            p.blocked
                .as_deref()
                .unwrap()
                .contains("is on elsewhere, not main"),
            "{p:?}"
        );
        assert!(plan(&t, true, Strategy::Squash)
            .unwrap()
            .blocked
            .unwrap()
            .contains("still running"));
    }

    #[test]
    fn discard_removes_work_branch_and_session() {
        let f = fixture("discard");
        let s = session(&f, "throwaway");
        std::fs::write(wt(&s).join("x.txt"), "x\n").unwrap();
        git(&wt(&s), &["add", "."]).unwrap();
        git(&wt(&s), &["commit", "-qm", "x"]).unwrap();
        let note = discard_in(&f.state, &s.id).unwrap();
        assert!(note.contains("deleted branch agent/throwaway"), "{note}");
        assert!(!wt(&s).exists());
        assert!(!crate::agent::branch_exists(&f.repo, "agent/throwaway"));
        assert_eq!(log(&f.repo), ["init"]);
        // Nothing to merge, nothing done.
        let t = session(&f, "idle");
        assert!(plan(&t, false, Strategy::Squash).unwrap().is_empty());
        let note = merge_in(&f.state, &t.id, Strategy::Squash, false).unwrap();
        assert!(note.starts_with("nothing to merge"), "{note}");
        assert!(wt(&t).is_dir());
    }

    #[test]
    fn strategies_parse() {
        assert_eq!(Strategy::parse("squash"), Some(Strategy::Squash));
        assert_eq!(Strategy::parse("ff"), Some(Strategy::FastForward));
        assert_eq!(Strategy::parse("rebase"), None);
    }
}
