// src/agent.rs
//
// Agent sessions (`cyberterm +agent`): any coding agent -- Claude Code,
// Codex, Gemini CLI, opencode, Copilot, ... or anything listed under
// [agents.launch] -- started in a git worktree of its own, in a tab of
// its own.
//
// A session is a record in $XDG_STATE_HOME/cyberterm/agents/<id>.json:
// which agent, its command, the task, the worktree, its branch and the
// commit it started from. The tab runs `cyberterm +agent run <id>`, which
// sets CYBERTERM_AGENT_ID and replaces itself with the agent. The window
// recognises agent panes by that variable in the foreground process's
// environment (/proc), so it works the same for daemon panes and after
// reattaching, and nothing here depends on which agent it is.

use crate::config::{AgentLaunch, AgentsConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// How an agent takes the task it starts with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Prompt {
    /// As the last argument: `claude "fix the test"`.
    Positional,
    /// After a flag: `gemini -i "fix the test"`.
    Flag(String),
    /// It doesn't: the task is shown in the pane before the agent starts.
    None,
}

impl Prompt {
    /// From a config value: "positional", "none", or the flag itself.
    fn parse(s: &str) -> Prompt {
        match s.trim() {
            "" | "none" => Prompt::None,
            "positional" => Prompt::Positional,
            flag => Prompt::Flag(flag.to_string()),
        }
    }
}

/// Agents Cyberterm knows how to start: (command, name shown, how it takes
/// a starting task). Checked against each one's `--help`.
const KNOWN: &[(&str, &str, &str)] = &[
    ("claude", "Claude Code", "positional"),
    ("codex", "Codex", "positional"),
    ("copilot", "GitHub Copilot", "-i"),
    ("gemini", "Gemini CLI", "-i"),
    ("cursor-agent", "Cursor Agent", "positional"),
    ("opencode", "opencode", "--prompt"),
    ("crush", "Crush", "none"),
    ("grok", "Grok", "positional"),
    ("pi", "pi", "positional"),
    ("omp", "oh-my-pi", "positional"),
    ("agy", "Antigravity", "-i"),
    ("muse", "Muse Code", "positional"),
    ("hermes", "Hermes Agent", "none"),
    ("ori", "Ori", "none"),
    ("aider", "Aider", "none"),
    ("goose", "Goose", "none"),
    ("amp", "Amp", "none"),
    ("qwen", "Qwen Code", "none"),
    ("kiro-cli", "Kiro", "none"),
    ("droid", "Droid", "none"),
];

/// An agent that can be started.
#[derive(Clone, Debug, PartialEq)]
pub struct Launcher {
    /// What `+agent <name>` takes.
    pub name: String,
    pub label: String,
    pub argv: Vec<String>,
    pub prompt: Prompt,
}

/// Native reporting an agent supports, beyond the shell recorder
/// (src/flight_log.rs). Chosen by the program it runs, so a custom
/// `[agents.launch]` entry for `claude --model opus` gets it too.
pub fn adapter(argv: &[String]) -> Option<&'static str> {
    let program = Path::new(argv.first()?).file_name()?.to_str()?;
    match program {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "gemini" => Some("gemini"),
        _ => None,
    }
}

/// The agents that can be started here: those in [agents.launch], then
/// the known ones found on PATH. A config entry with a known name
/// overrides it.
pub fn launchers(cfg: &AgentsConfig) -> Vec<Launcher> {
    let mut out: Vec<Launcher> = Vec::new();
    for (name, l) in &cfg.launch {
        if let Some(launcher) = from_config(name, l) {
            out.push(launcher);
        }
    }
    for (bin, label, prompt) in KNOWN {
        if out.iter().any(|l| l.name == *bin) || !on_path(bin) {
            continue;
        }
        out.push(Launcher {
            name: bin.to_string(),
            label: label.to_string(),
            argv: vec![bin.to_string()],
            prompt: Prompt::parse(prompt),
        });
    }
    out.sort_by_key(|l| l.label.to_lowercase());
    out
}

fn from_config(name: &str, l: &AgentLaunch) -> Option<Launcher> {
    let known = KNOWN.iter().find(|(bin, _, _)| *bin == name);
    let argv = if l.command.trim().is_empty() {
        vec![known?.0.to_string()]
    } else {
        split_command(&l.command).ok()?
    };
    if argv.is_empty() {
        return None;
    }
    let label = match (l.label.trim(), known) {
        ("", Some((_, label, _))) => label.to_string(),
        ("", None) => name.to_string(),
        (label, _) => label.to_string(),
    };
    let prompt = match (l.prompt.trim(), known) {
        ("", Some((_, _, prompt))) => Prompt::parse(prompt),
        (prompt, _) => Prompt::parse(prompt),
    };
    Some(Launcher {
        name: name.to_string(),
        label,
        argv,
        prompt,
    })
}

/// Finds a launcher by name or shown name, ignoring case.
pub fn find<'a>(launchers: &'a [Launcher], name: &str) -> Option<&'a Launcher> {
    let name = name.to_lowercase();
    launchers
        .iter()
        .find(|l| l.name.to_lowercase() == name || l.label.to_lowercase() == name)
}

fn on_path(bin: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join(bin))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Splits a command line the way a shell would for plain words, quotes
/// and backslashes (no expansions).
pub fn split_command(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err(format!("unclosed ' in {s:?}")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err(format!("unclosed \" in {s:?}")),
                        },
                        Some(c) => word.push(c),
                        None => return Err(format!("unclosed \" in {s:?}")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(c) = chars.next() {
                    word.push(c);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        out.push(word);
    }
    Ok(out)
}

/// Quotes a word for typing at a shell prompt (bash, zsh and fish alike:
/// plain words stay as they are, anything else is double-quoted).
pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:+=@%,".contains(c));
    if plain {
        return s.to_string();
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// ----------------------------------------------------------------------
// Sessions
// ----------------------------------------------------------------------

/// One agent session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    /// The launcher's name and shown name.
    pub agent: String,
    pub label: String,
    pub argv: Vec<String>,
    pub prompt: Prompt,
    pub task: Option<String>,
    /// Where the agent runs (inside the worktree, if there is one).
    pub dir: PathBuf,
    /// The worktree, its branch and the commit it started from; `None`
    /// when the agent runs in place.
    pub worktree: Option<Worktree>,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    /// The repository it was made from.
    pub repo: PathBuf,
}

impl Session {
    /// The agent's command line with `extra` options, and the task added
    /// the way it takes one.
    fn command_with(&self, extra: &[String]) -> Vec<String> {
        let mut argv = self.argv.clone();
        argv.extend(extra.iter().cloned());
        if let Some(task) = self.task.as_ref().filter(|t| !t.trim().is_empty()) {
            match &self.prompt {
                Prompt::Positional => argv.push(task.clone()),
                Prompt::Flag(flag) => {
                    argv.push(flag.clone());
                    argv.push(task.clone());
                }
                Prompt::None => {}
            }
        }
        argv
    }

    /// The tab's title: "Claude Code · fix-flaky-test".
    pub fn title(&self) -> String {
        format!("{} · {}", self.label, self.id)
    }
}

/// Where session records live.
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("cyberterm")
        .join("agents")
}

fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

pub fn load(id: &str) -> Result<Session, String> {
    load_in(&state_dir(), id)
}

fn load_in(dir: &Path, id: &str) -> Result<Session, String> {
    if !valid_id(id) {
        return Err(format!("no agent session {id:?}"));
    }
    let text = std::fs::read_to_string(record_path(dir, id))
        .map_err(|_| format!("no agent session {id:?} (see `cyberterm +agent list`)"))?;
    serde_json::from_str(&text).map_err(|e| format!("agent session {id}: {e}"))
}

/// Every session, newest first.
pub fn sessions() -> Vec<Session> {
    sessions_in(&state_dir())
}

fn sessions_in(dir: &Path) -> Vec<Session> {
    let mut out: Vec<Session> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect();
    out.sort_by_key(|s: &Session| std::cmp::Reverse(s.created_ms));
    out
}

fn save_in(dir: &Path, s: &Session) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = record_path(dir, &s.id);
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A short name from the task's first words ("Fix the flaky login test!"
/// -> "fix-the-flaky-login"), or the agent's name without a task.
pub fn slug(task: Option<&str>, agent: &str) -> String {
    let from = |s: &str, words: usize| {
        s.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .take(words)
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
            .join("-")
    };
    let mut s = task.map(|t| from(t, 4)).unwrap_or_default();
    if s.is_empty() {
        s = from(agent, 3);
    }
    if s.is_empty() {
        s = "agent".into();
    }
    s.truncate(40);
    s.trim_end_matches('-').to_string()
}

/// What `create` needs to know.
pub struct Request<'a> {
    pub launcher: &'a Launcher,
    pub task: Option<String>,
    /// Where the agent was asked for (the current pane's directory).
    pub cwd: PathBuf,
    pub worktree: bool,
}

/// Creates a session: its worktree (when `worktree` is set and `cwd` is in
/// a git repository) and its record.
pub fn create(cfg: &AgentsConfig, req: Request<'_>) -> Result<Session, String> {
    create_in(&state_dir(), cfg, req)
}

fn create_in(state: &Path, cfg: &AgentsConfig, req: Request<'_>) -> Result<Session, String> {
    let task = req.task.filter(|t| !t.trim().is_empty());
    let base_slug = slug(task.as_deref(), &req.launcher.name);
    let repo = if req.worktree {
        git_toplevel(&req.cwd)
    } else {
        None
    };
    let worktree_root = |top: &Path| -> PathBuf {
        let dir = cfg.worktree_dir.trim();
        if dir.is_empty() {
            top.parent().unwrap_or(top).to_path_buf()
        } else {
            expand_home(dir)
        }
    };
    let repo_name = |top: &Path| {
        top.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into())
    };

    // A name nothing else uses yet: record, branch and worktree folder.
    let mut id = base_slug.clone();
    for n in 2.. {
        let taken = record_path(state, &id).exists()
            || repo.as_ref().is_some_and(|top| {
                branch_exists(top, &format!("agent/{id}"))
                    || worktree_root(top)
                        .join(format!("{}-{id}", repo_name(top)))
                        .exists()
            });
        if !taken {
            break;
        }
        id = format!("{base_slug}-{n}");
    }

    let (dir, worktree) = match &repo {
        Some(top) => {
            let path = worktree_root(top).join(format!("{}-{id}", repo_name(top)));
            let branch = format!("agent/{id}");
            let base = git(top, &["rev-parse", "HEAD"])?;
            git(
                top,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    &path.to_string_lossy(),
                    &base,
                ],
            )?;
            // Start where you were: repo/src -> worktree/src.
            let rel = req.cwd.strip_prefix(top).unwrap_or(Path::new(""));
            let dir = Some(path.join(rel))
                .filter(|d| d.is_dir())
                .unwrap_or_else(|| path.clone());
            (
                dir,
                Some(Worktree {
                    path,
                    branch,
                    base,
                    repo: top.clone(),
                }),
            )
        }
        None => (req.cwd.clone(), None),
    };

    let session = Session {
        id,
        agent: req.launcher.name.clone(),
        label: req.launcher.label.clone(),
        argv: req.launcher.argv.clone(),
        prompt: req.launcher.prompt.clone(),
        task,
        dir,
        worktree,
        created_ms: crate::shell::tap::now_ms(),
    };
    save_in(state, &session)?;
    Ok(session)
}

/// Runs a session's agent in this process (`+agent run <id>`): changes to
/// its directory, marks the environment, turns on the flight log, and
/// replaces itself with it.
pub fn exec(id: &str, cfg: &AgentsConfig) -> Result<std::convert::Infallible, String> {
    use std::os::unix::process::CommandExt;
    let s = load(id)?;
    if !s.dir.is_dir() {
        return Err(format!(
            "{} is gone (the worktree was removed?)",
            short(&s.dir)
        ));
    }
    if let (Prompt::None, Some(task)) = (&s.prompt, &s.task) {
        println!("\x1b[1mTask:\x1b[0m {task}\n");
    }
    let log = crate::flight_log::log_path(&s.id);
    let mut extra = Vec::new();
    if cfg.hooks {
        match adapter(&s.argv) {
            Some("claude") => {
                let hook = format!("{} +hook claude", self_path());
                extra.push("--settings".to_string());
                extra.push(crate::flight_log::claude_settings(&hook));
            }
            Some("codex") => {
                // Same command every time, so Codex's one-time review of
                // these hooks holds for later sessions.
                let hook = format!("{} +hook codex", self_path());
                for o in crate::flight_log::codex_overrides(&hook) {
                    extra.push("-c".to_string());
                    extra.push(o);
                }
            }
            // Gemini's hooks come from `+agent setup gemini`.
            _ => {}
        }
    }
    let argv = s.command_with(&extra);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&s.dir)
        .env("CYBERTERM_AGENT_ID", &s.id)
        .env("CYBERTERM_AGENT", &s.agent);
    if cfg.record_commands {
        match crate::flight_log::recorder_env(&log) {
            Ok(env) => {
                cmd.envs(env);
            }
            Err(e) => eprintln!("cyberterm: flight log off ({e})"),
        }
    }
    let err = cmd.exec();
    Err(format!("couldn't start {}: {err}", argv[0]))
}

/// The command a tab types to start a session.
pub fn run_command(id: &str) -> String {
    format!("{} +agent run {}", self_command(), shell_quote(id))
}

/// This binary's full path, quoted for a shell (for hooks, which may run
/// with another PATH).
fn self_path() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cyberterm".into());
    shell_quote(&exe)
}

/// How to call this binary from a shell: `cyberterm` when that's the one
/// on PATH, its full path otherwise (a dev build).
fn self_command() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("cyberterm"))
            .find(|p| p.is_file())
            .and_then(|p| p.canonicalize().ok())
    });
    match exe {
        Some(exe) if on_path.as_ref() != Some(&exe) => shell_quote(&exe.to_string_lossy()),
        _ => "cyberterm".into(),
    }
}

// ----------------------------------------------------------------------
// Status and removal
// ----------------------------------------------------------------------

/// A session's state, for `+agent list` and removal.
#[derive(Debug, Default, PartialEq)]
pub struct Status {
    /// Processes running it (still has the session's id in their
    /// environment).
    pub running: bool,
    /// Uncommitted changes in the worktree (files).
    pub changed: usize,
    /// Commits on its branch since it started.
    pub commits: usize,
    pub worktree_missing: bool,
}

pub fn status(s: &Session) -> Status {
    let mut st = Status {
        running: !crate::procs::with_env("CYBERTERM_AGENT_ID", &s.id).is_empty(),
        ..Default::default()
    };
    if let Some(w) = &s.worktree {
        if !w.path.is_dir() {
            st.worktree_missing = true;
            return st;
        }
        st.changed = git(&w.path, &["status", "--porcelain"])
            .map(|out| out.lines().count())
            .unwrap_or(0);
        st.commits = git(
            &w.path,
            &["rev-list", "--count", &format!("{}..HEAD", w.base)],
        )
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    }
    st
}

/// Removes a session: its worktree, its branch when nothing was
/// committed on it, and its record. Refuses while the agent runs, and --
/// unless `force` -- when the worktree has changes or commits.
pub fn remove(id: &str, force: bool) -> Result<String, String> {
    remove_in(&state_dir(), id, force)
}

fn remove_in(state: &Path, id: &str, force: bool) -> Result<String, String> {
    let s = load_in(state, id)?;
    let st = status(&s);
    if st.running {
        return Err(format!("{id} is still running; quit the agent first"));
    }
    let mut notes = Vec::new();
    if let Some(w) = &s.worktree {
        if !st.worktree_missing {
            if !force && (st.changed > 0 || st.commits > 0) {
                return Err(format!(
                    "{id} has {} uncommitted file(s) and {} commit(s) in {}; \
                     merge or keep what you want, then use --force",
                    st.changed,
                    st.commits,
                    short(&w.path)
                ));
            }
            let path = w.path.to_string_lossy();
            let mut args = vec!["worktree", "remove"];
            if force {
                args.push("--force");
            }
            args.push(&path);
            let shells = crate::procs::with_cwd_under(&w.path).len();
            git(&w.repo, &args)?;
            notes.push(format!("removed {}", short(&w.path)));
            if shells > 0 {
                notes.push(format!(
                    "{shells} process(es) were still in it -- cd out of the deleted folder"
                ));
            }
        } else {
            let _ = git(&w.repo, &["worktree", "prune"]);
        }
        if st.commits == 0 {
            if git(&w.repo, &["branch", "-D", &w.branch]).is_ok() {
                notes.push(format!("deleted branch {}", w.branch));
            }
        } else {
            notes.push(format!(
                "kept branch {} ({} commit(s))",
                w.branch, st.commits
            ));
        }
    }
    let _ = std::fs::remove_file(record_path(state, id));
    let _ = std::fs::remove_file(state.join(format!("{id}.jsonl")));
    notes.push("removed the session and its flight log".into());
    Ok(notes.join(", "))
}

// ----------------------------------------------------------------------
// git
// ----------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// A path for display, with the home directory as `~`.
pub fn short(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => format!("~{}", &s[home.len()..]),
        _ => s,
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

// ----------------------------------------------------------------------
// CLI
// ----------------------------------------------------------------------

const HELP: &str = "\
cyberterm +agent: run any coding agent in a worktree of its own

  cyberterm +agent                       the agent home: your agents and sessions
  cyberterm +agent <name> [task...]      start one in a new tab, in a new worktree
      --no-worktree                      run it in this directory instead
      --here                             run it in this terminal, not a new tab
  cyberterm +agent list                  sessions: running, changed files, commits
  cyberterm +agent log [id] [-o] [-f]    what it did: prompts, commands, edits
                                         (-o with output, -f keep following)
  cyberterm +agent setup [agent]         what each agent reports; opt-in hooks (Gemini)
  cyberterm +agent rm <id> [--force]     remove a session and its worktree
                                         (--force: even with changes or commits)

The worktree is ../<repo>-<id> on a new branch agent/<id>. Agents found on
PATH are offered automatically; add others under [agents.launch]:

  [agents.launch.mine]
  command = \"my-agent --flag\"
  label = \"My agent\"
  prompt = \"positional\"     # or a flag such as \"-i\", or \"none\"
";

pub fn run_cli(args: &[String], cfg: &AgentsConfig) -> i32 {
    let first = args.first().map(String::as_str);
    match first {
        None | Some("list" | "ls") => {
            list(cfg, first.is_none());
            0
        }
        Some("help" | "--help" | "-h") => {
            print!("{HELP}");
            0
        }
        Some("run") => {
            let Some(id) = args.get(1) else {
                eprintln!("usage: cyberterm +agent run <id>");
                return 2;
            };
            match exec(id, cfg) {
                Ok(never) => match never {},
                Err(e) => {
                    eprintln!("cyberterm +agent: {e}");
                    1
                }
            }
        }
        Some("setup") => crate::agent_setup::run(&args[1..], cfg),
        Some("log") => {
            let follow = args.iter().any(|a| a == "-f" || a == "--follow");
            let output = args.iter().any(|a| a == "-o" || a == "--output");
            let id = args[1..].iter().find(|a| !a.starts_with('-')).cloned();
            let id = match id.or_else(|| sessions().first().map(|s| s.id.clone())) {
                Some(id) => id,
                None => {
                    eprintln!("No agent sessions yet.");
                    return 1;
                }
            };
            if let Err(e) = load(&id) {
                eprintln!("❌ {e}");
                return 1;
            }
            show_log(&id, output, follow);
            0
        }
        Some("rm" | "remove") => {
            let force = args.iter().any(|a| a == "--force" || a == "-f");
            let ids: Vec<&String> = args[1..].iter().filter(|a| !a.starts_with('-')).collect();
            if ids.is_empty() {
                eprintln!("usage: cyberterm +agent rm <id> [--force]");
                return 2;
            }
            let mut code = 0;
            for id in ids {
                match remove(id, force) {
                    Ok(what) => println!("{id}: {what}"),
                    Err(e) => {
                        eprintln!("❌ {e}");
                        code = 1;
                    }
                }
            }
            code
        }
        Some(name) => start(name, &args[1..], cfg),
    }
}

fn start(name: &str, rest: &[String], cfg: &AgentsConfig) -> i32 {
    let launchers = launchers(cfg);
    let Some(launcher) = find(&launchers, name) else {
        eprintln!("❌ No agent called {name:?} here.");
        if !launchers.is_empty() {
            let names: Vec<&str> = launchers.iter().map(|l| l.name.as_str()).collect();
            eprintln!("   Found: {}", names.join(", "));
        }
        eprintln!("   Others can be added under [agents.launch] (cyberterm +agent help).");
        return 1;
    };
    let mut worktree = cfg.worktrees;
    let mut here = false;
    let mut words = Vec::new();
    for arg in rest {
        match arg.as_str() {
            "--no-worktree" => worktree = false,
            "--here" => here = true,
            _ => words.push(arg.clone()),
        }
    }
    let task = Some(words.join(" ")).filter(|t| !t.trim().is_empty());
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let session = match create(
        cfg,
        Request {
            launcher,
            task,
            cwd,
            worktree,
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ {e}");
            return 1;
        }
    };
    if let Some(w) = &session.worktree {
        println!(
            "◆ {} · worktree {} on {}",
            session.label,
            short(&w.path),
            w.branch
        );
    }
    let socket = (!here).then(crate::control::find_socket).flatten();
    if let Some(socket) = socket {
        let params = serde_json::json!({
            "cwd": session.dir,
            "title": session.title(),
            "command": run_command(&session.id),
            "flight_log": true,
        });
        match crate::control::call(&socket, "new_tab", params) {
            Ok(resp) if resp.error.is_none() => {
                println!("◆ Opened in a new tab ({})", session.id);
                return 0;
            }
            Ok(resp) => eprintln!(
                "couldn't open a tab ({}); running it here",
                resp.error.map(|e| e.message).unwrap_or_default()
            ),
            Err(e) => eprintln!("couldn't reach Cyberterm ({e}); running it here"),
        }
    }
    match exec(&session.id, cfg) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("❌ {e}");
            1
        }
    }
}

/// Prints a session's flight log; with `follow`, keeps printing new
/// entries until interrupted.
fn show_log(id: &str, output: bool, follow: bool) {
    use std::io::IsTerminal;
    let path = crate::flight_log::log_path(id);
    let color = std::io::stdout().is_terminal();
    let mut shown = 0;
    loop {
        let entries = crate::flight_log::timeline(&crate::flight_log::read(&path));
        // Entries already shown can still change (a command finishing),
        // so in follow mode the last few are reprinted when they do.
        if entries.len() > shown || !follow {
            let new = &entries[shown.min(entries.len())..];
            print!("{}", crate::flight_log::render(new, output, color));
            shown = entries.len();
        }
        if !follow {
            if entries.is_empty() {
                println!("Nothing recorded for {id} yet.");
            }
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn list(cfg: &AgentsConfig, with_agents: bool) {
    if with_agents {
        let launchers = launchers(cfg);
        println!("Agents found here ({}):", launchers.len());
        for l in &launchers {
            let how = match &l.prompt {
                Prompt::Positional => "takes a task".to_string(),
                Prompt::Flag(f) => format!("takes a task ({f})"),
                Prompt::None => "start, then type the task".to_string(),
            };
            println!(
                "  {:<14} {:<16} {how:<28} {}",
                l.name,
                l.label,
                crate::agent_setup::coverage(&l.name)
            );
        }
        println!();
    }
    let sessions = sessions();
    if sessions.is_empty() {
        println!("No agent sessions. Start one: cyberterm +agent <name> [task]");
        return;
    }
    println!("Sessions:");
    for s in &sessions {
        let st = status(s);
        let state = if st.running { "running" } else { "stopped" };
        let changes = match &s.worktree {
            Some(_) if st.worktree_missing => "worktree gone".to_string(),
            Some(_) => format!("{} changed · {} commits", st.changed, st.commits),
            None => "no worktree".to_string(),
        };
        println!(
            "  {:<28} {:<14} {:<8} {:<24} {}",
            s.id,
            s.agent,
            state,
            changes,
            short(&s.dir)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launcher(name: &str, prompt: Prompt) -> Launcher {
        Launcher {
            name: name.into(),
            label: name.into(),
            argv: vec![name.into()],
            prompt,
        }
    }

    #[test]
    fn commands_split_like_a_shell() {
        assert_eq!(
            split_command(r#"aider --model "gpt 5" 'a b' c\ d"#).unwrap(),
            vec!["aider", "--model", "gpt 5", "a b", "c d"]
        );
        assert!(split_command("x 'open").is_err());
        assert_eq!(split_command("  ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn words_are_quoted_only_when_needed() {
        assert_eq!(shell_quote("fix-test"), "fix-test");
        assert_eq!(shell_quote("/a b/c"), "\"/a b/c\"");
        assert_eq!(shell_quote("$x\"y"), "\"\\$x\\\"y\"");
    }

    #[test]
    fn slugs_come_from_the_task() {
        assert_eq!(
            slug(Some("Fix the flaky login test, please!"), "claude"),
            "fix-the-flaky-login"
        );
        assert_eq!(slug(None, "cursor-agent"), "cursor-agent");
        assert_eq!(slug(Some("!!!"), "claude"), "claude");
        assert!(valid_id(&slug(Some("Ünïcode ✓ task"), "x")));
    }

    #[test]
    fn the_task_goes_where_each_agent_takes_it() {
        let mut s = Session {
            id: "t".into(),
            agent: "a".into(),
            label: "A".into(),
            argv: vec!["a".into(), "--yolo".into()],
            prompt: Prompt::Positional,
            task: Some("do it".into()),
            dir: PathBuf::from("/"),
            worktree: None,
            created_ms: 0,
        };
        assert_eq!(s.command_with(&[]), vec!["a", "--yolo", "do it"]);
        s.prompt = Prompt::Flag("-i".into());
        assert_eq!(s.command_with(&[]), vec!["a", "--yolo", "-i", "do it"]);
        s.prompt = Prompt::None;
        assert_eq!(s.command_with(&[]), vec!["a", "--yolo"]);
        s.task = None;
        s.prompt = Prompt::Positional;
        assert_eq!(s.command_with(&[]), vec!["a", "--yolo"]);
    }

    #[test]
    fn config_entries_override_and_extend_the_known_agents() {
        let mut cfg = AgentsConfig::default();
        cfg.launch.insert(
            "claude".into(),
            AgentLaunch {
                command: "claude --model opus".into(),
                ..Default::default()
            },
        );
        cfg.launch.insert(
            "mine".into(),
            AgentLaunch {
                command: "my-agent run".into(),
                label: "My agent".into(),
                prompt: "--task".into(),
            },
        );
        let all = launchers(&cfg);
        let claude = find(&all, "claude").unwrap();
        assert_eq!(claude.label, "Claude Code");
        assert_eq!(claude.argv, vec!["claude", "--model", "opus"]);
        assert_eq!(claude.prompt, Prompt::Positional);
        let mine = find(&all, "My Agent").unwrap();
        assert_eq!(mine.prompt, Prompt::Flag("--task".into()));
        assert_eq!(all.iter().filter(|l| l.name == "claude").count(), 1);
    }

    fn git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        std::fs::create_dir_all(dir.join("src")).unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("src/a.txt"), "a\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyberterm-agent-{name}-{}-{}",
            std::process::id(),
            crate::shell::tap::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sessions_get_their_own_worktree_and_unique_names() {
        let root = scratch("wt");
        let repo = root.join("api");
        git_repo(&repo);
        let state = root.join("state");
        let cfg = AgentsConfig::default();
        let l = launcher("claude", Prompt::Positional);
        let req = |task: &str| Request {
            launcher: &l,
            task: Some(task.into()),
            cwd: repo.join("src"),
            worktree: true,
        };

        let a = create_in(&state, &cfg, req("fix the test")).unwrap();
        std::fs::write(state.join("fix-the-test.jsonl"), "{}\n").unwrap();
        assert_eq!(a.id, "fix-the-test");
        let w = a.worktree.as_ref().unwrap();
        assert_eq!(w.path, root.join("api-fix-the-test"));
        assert_eq!(w.branch, "agent/fix-the-test");
        // Started from repo/src, so it runs in worktree/src.
        assert_eq!(a.dir, w.path.join("src"));
        assert!(a.dir.join("a.txt").is_file());
        assert_eq!(load_in(&state, "fix-the-test").unwrap(), a);

        let b = create_in(&state, &cfg, req("Fix the test")).unwrap();
        assert_eq!(b.id, "fix-the-test-2");
        assert_eq!(sessions_in(&state).len(), 2);

        // Outside a repository there's no worktree.
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let c = create_in(
            &state,
            &cfg,
            Request {
                launcher: &l,
                task: None,
                cwd: plain.clone(),
                worktree: true,
            },
        )
        .unwrap();
        assert_eq!(
            (c.id.as_str(), c.dir.as_path()),
            ("claude", plain.as_path())
        );
        assert!(c.worktree.is_none());

        let st = status(&a);
        assert_eq!((st.changed, st.commits, st.running), (0, 0, false));
        std::fs::write(a.dir.join("b.txt"), "b\n").unwrap();
        assert_eq!(status(&a).changed, 1);

        // Changes keep it unless forced; the branch goes when it has no
        // commits of its own.
        assert!(remove_in(&state, "fix-the-test", false).is_err());
        assert!(w.path.is_dir());
        remove_in(&state, "fix-the-test", true).unwrap();
        assert!(!w.path.exists());
        assert!(!branch_exists(&repo, "agent/fix-the-test"));
        assert!(load_in(&state, "fix-the-test").is_err());
        assert!(!state.join("fix-the-test.jsonl").exists());

        // A commit on its branch: the branch is kept.
        let bw = b.worktree.as_ref().unwrap();
        std::fs::write(bw.path.join("c.txt"), "c\n").unwrap();
        git(&bw.path, &["add", "."]).unwrap();
        git(&bw.path, &["commit", "-qm", "c"]).unwrap();
        assert_eq!(status(&b).commits, 1);
        assert!(remove_in(&state, &b.id, false).is_err());
        let note = remove_in(&state, &b.id, true).unwrap();
        assert!(note.contains("kept branch agent/fix-the-test-2"), "{note}");
        assert!(branch_exists(&repo, "agent/fix-the-test-2"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_ids_are_rejected_before_touching_the_disk() {
        assert!(load_in(Path::new("/nonexistent"), "../etc/passwd").is_err());
        assert!(!valid_id("A"));
        assert!(valid_id("fix-1"));
    }
}
