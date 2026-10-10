// src/flight_log.rs
//
// The flight log: what an agent session did -- the prompts it was given,
// the commands it ran (with exit codes, durations and, where the agent
// reports it, output), the files it edited, and when it was waiting for
// you or finished. One JSON object per line in
// $XDG_STATE_HOME/cyberterm/agents/<id>.jsonl, next to the session record
// (src/agent.rs), so it outlives the window and the daemon.
//
// Events come from three sources, whichever the agent supports:
//
// - The shell recorder (any agent): agents run their commands through
//   `bash -c` / `zsh -c`. `+agent run` points BASH_ENV and ZDOTDIR at the
//   small scripts below, which log each command's text, directory, exit
//   code and duration -- and otherwise run your own startup files exactly
//   as before.
// - Native hooks: Claude Code gets hooks for its session (`--settings`),
//   which add prompts, command output, edits and waiting/finished states.
// - The open format: `cyberterm +hook event` takes events from anything --
//   another agent's hook or plugin system, or a wrapper script:
//
//     {"kind": "command", "command": "cargo test", "exit": 101,
//      "duration_ms": 5321, "output": "..."}
//
//   kinds: prompt (text), command_start / command (id, command, cwd,
//   exit, duration_ms, output), edit (path, tool), tool (tool, text),
//   waiting (text), done, session_start, session_end. `t` (ms since the
//   epoch) and `v` are filled in when missing.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Longest command or output text kept per event.
const MAX_TEXT: usize = 4000;
/// Longest hook input read from stdin.
const MAX_INPUT: u64 = 4 << 20;

/// One thing that happened.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    pub v: u32,
    /// When (ms since the epoch).
    pub t: u64,
    pub kind: String,
    /// Who reported it: "shell", "claude", or "hook" (the open format).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Pairs a command's start with its end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The file an edit touched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The agent's tool behind it ("Edit", "WebFetch", ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// A prompt, a tool's summary, or what the agent is waiting for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The command failed (when the agent reports failure without a code).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
}

impl Event {
    fn new(kind: &str, source: &str) -> Self {
        Event {
            v: 1,
            t: crate::shell::tap::now_ms(),
            kind: kind.into(),
            source: source.into(),
            ..Default::default()
        }
    }
}

const KINDS: &[&str] = &[
    "prompt",
    "command_start",
    "command",
    "edit",
    "tool",
    "waiting",
    "done",
    "session_start",
    "session_end",
];

pub fn log_path(id: &str) -> PathBuf {
    crate::agent::state_dir().join(format!("{id}.jsonl"))
}

/// Appends events, one line each, in a single write.
pub fn append(path: &Path, events: &[Event]) -> std::io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let mut buf = String::new();
    for e in events {
        buf.push_str(&serde_json::to_string(e).map_err(std::io::Error::other)?);
        buf.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(buf.as_bytes())
}

/// Every event in a log; lines that don't parse are skipped.
pub fn read(path: &Path) -> Vec<Event> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Keep the end: that's where errors and summaries are.
    let tail: String = s
        .chars()
        .rev()
        .take(max)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

// ----------------------------------------------------------------------
// The open format
// ----------------------------------------------------------------------

/// An event from `cyberterm +hook event`: checked, clipped, stamped.
pub fn from_open(v: &Value) -> Result<Event, String> {
    let mut e: Event =
        serde_json::from_value(v.clone()).map_err(|e| format!("not an event: {e}"))?;
    if !KINDS.contains(&e.kind.as_str()) {
        return Err(format!(
            "unknown kind {:?} (one of {})",
            e.kind,
            KINDS.join(", ")
        ));
    }
    e.v = 1;
    if e.t == 0 {
        e.t = crate::shell::tap::now_ms();
    }
    if e.source.is_empty() {
        e.source = "hook".into();
    }
    for s in [&mut e.command, &mut e.output, &mut e.text]
        .into_iter()
        .flatten()
    {
        *s = clip(s, MAX_TEXT);
    }
    Ok(e)
}

// ----------------------------------------------------------------------
// Agents' own hooks: Claude Code, Codex, Gemini CLI
// ----------------------------------------------------------------------
//
// Claude Code and Codex speak the same hook protocol (PreToolUse,
// PostToolUse, Stop, ...); Gemini CLI has its own names (BeforeTool,
// AfterTool, AfterAgent, ...) and shapes. All three send one JSON object
// per hook on stdin.
//
// How the hooks get there:
// - Claude Code: `--settings <json>` for that session only.
// - Codex: `-c hooks.<Event>=[...]` for that session only. Codex asks you
//   to review hooks it hasn't seen before; you approve Cyberterm's once.
// - Gemini CLI: has no per-session way in (its system settings must be
//   root-owned), so `cyberterm +agent setup gemini` adds the hooks to
//   ~/.gemini/settings.json once -- they do nothing outside Cyberterm's
//   agent sessions.

/// The agents whose hooks Cyberterm understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Gemini,
}

impl Agent {
    pub fn parse(s: &str) -> Option<Agent> {
        match s {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "gemini" => Some(Agent::Gemini),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini",
        }
    }

    /// Its shell tool.
    fn is_shell(self, tool: &str) -> bool {
        match self {
            Agent::Claude | Agent::Codex => tool == "Bash",
            Agent::Gemini => tool == "run_shell_command",
        }
    }

    /// The hook events to register, as (event, tool matcher).
    fn events(self) -> &'static [(&'static str, bool)] {
        match self {
            Agent::Claude => &[
                ("PreToolUse", true),
                ("PostToolUse", true),
                ("PostToolUseFailure", true),
                ("UserPromptSubmit", false),
                ("Notification", false),
                ("PermissionRequest", false),
                ("Stop", false),
                ("SessionStart", false),
                ("SessionEnd", false),
            ],
            Agent::Codex => &[
                ("PreToolUse", true),
                ("PostToolUse", true),
                ("UserPromptSubmit", false),
                ("PermissionRequest", false),
                ("Stop", false),
                ("SessionStart", false),
                ("SessionEnd", false),
            ],
            Agent::Gemini => &[
                ("BeforeTool", true),
                ("AfterTool", true),
                ("BeforeAgent", false),
                ("AfterAgent", false),
                ("Notification", false),
                ("SessionStart", false),
                ("SessionEnd", false),
            ],
        }
    }

    /// Hook timeout as each agent counts it (Gemini: milliseconds).
    fn timeout(self) -> u64 {
        match self {
            Agent::Gemini => 10_000,
            _ => 10,
        }
    }
}

/// The `hooks` object (event -> matcher groups) running `hook` -- a shell
/// command, this binary's `+hook <agent>` -- for every event.
pub fn hooks_json(agent: Agent, hook: &str) -> Value {
    let mut hooks = serde_json::Map::new();
    for (event, tools) in agent.events() {
        let mut group = serde_json::json!({
            "hooks": [{ "type": "command", "command": hook, "timeout": agent.timeout() }]
        });
        if *tools {
            group["matcher"] = Value::String("*".into());
        }
        hooks.insert(event.to_string(), Value::Array(vec![group]));
    }
    Value::Object(hooks)
}

/// Claude Code's `--settings` JSON.
pub fn claude_settings(hook: &str) -> String {
    serde_json::json!({ "hooks": hooks_json(Agent::Claude, hook) }).to_string()
}

/// Codex's `-c key=value` overrides (TOML values), one per event.
pub fn codex_overrides(hook: &str) -> Vec<String> {
    let quote = |s: &str| {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    };
    Agent::Codex
        .events()
        .iter()
        .map(|(event, tools)| {
            let matcher = if *tools { "matcher=\"*\"," } else { "" };
            format!(
                "hooks.{event}=[{{{matcher}hooks=[{{type=\"command\",command={},timeout={}}}]}}]",
                quote(hook),
                Agent::Codex.timeout()
            )
        })
        .collect()
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Files a tool call changed.
fn edited_paths(agent: Agent, tool: &str, input: &Value) -> Vec<String> {
    let one = |k: &str| s(input, k).map(|p| vec![p.to_string()]).unwrap_or_default();
    match (agent, tool) {
        (_, "Edit" | "MultiEdit" | "Write") => one("file_path"),
        (_, "NotebookEdit") => one("notebook_path"),
        (Agent::Codex, "apply_patch") => {
            // V4A patches: "*** Update File: path", "*** Add File: ...",
            // "*** Delete File: ...", "*** Move to: ...".
            let patch = s(input, "command")
                .or_else(|| s(input, "input"))
                .or_else(|| s(input, "patch"))
                .unwrap_or_default();
            let mut paths: Vec<String> = Vec::new();
            for line in patch.lines() {
                let line = line.trim();
                for prefix in [
                    "*** Update File: ",
                    "*** Add File: ",
                    "*** Delete File: ",
                    "*** Move to: ",
                ] {
                    if let Some(p) = line.strip_prefix(prefix) {
                        if !paths.iter().any(|q| q == p.trim()) {
                            paths.push(p.trim().to_string());
                        }
                    }
                }
            }
            paths
        }
        (Agent::Gemini, "replace" | "write_file") => one("file_path"),
        _ => Vec::new(),
    }
}

/// A one-line summary of what a tool call was about.
fn tool_summary(input: &Value) -> Option<String> {
    [
        "file_path",
        "absolute_path",
        "path",
        "dir_path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ]
    .iter()
    .find_map(|k| s(input, k))
    .map(|t| clip(t.lines().next().unwrap_or(t), 200))
}

/// The text a tool returned. Shapes vary: a string (Codex), stdout and
/// stderr (Claude), or `llmContent` (Gemini, as a string or parts).
fn response_text(r: &Value) -> Option<String> {
    let text = match r {
        Value::String(t) => t.clone(),
        Value::Object(_) => {
            if let Some(content) = r.get("llmContent") {
                match content {
                    Value::String(t) => t.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| s(p, "text").or_else(|| p.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => return None,
                }
            } else {
                let out = s(r, "stdout")
                    .or_else(|| s(r, "output"))
                    .unwrap_or_default();
                let err = s(r, "stderr").unwrap_or_default();
                match (out.is_empty(), err.is_empty()) {
                    (false, false) => format!("{out}\n{err}"),
                    (false, true) => out.to_string(),
                    (true, false) => err.to_string(),
                    (true, true) => return None,
                }
            }
        }
        _ => return None,
    };
    // Gemini fences tool output as untrusted.
    let text = text
        .trim()
        .strip_prefix("<untrusted_context>")
        .and_then(|t| t.strip_suffix("</untrusted_context>"))
        .map(str::trim)
        .unwrap_or(text.trim())
        .to_string();
    Some(text).filter(|t| !t.is_empty())
}

/// What a shell tool's output says about how the command went: exit code
/// and duration where the agent writes them, and the output proper.
///
/// - Codex: "Exit code: 1\nWall time: 0.4 seconds\nOutput:\n..."
/// - Gemini: "Output: ...\nExit Code: 1" (the exit code only when it isn't
///   0, so no "Exit Code" with an "Output:" section means 0)
fn shell_result(agent: Agent, text: &str) -> (Option<i32>, Option<u64>, String) {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string()))
    };
    match agent {
        Agent::Codex => {
            let exit = field("Exit code:").and_then(|v| v.parse().ok());
            let duration = field("Wall time:")
                .and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok())
                .map(|secs| (secs * 1000.0) as u64);
            let output = match text.find("Output:\n") {
                Some(i) => text[i + 8..].to_string(),
                None => text.to_string(),
            };
            (exit, duration, output)
        }
        Agent::Gemini => {
            let exit = field("Exit Code:").and_then(|v| v.parse().ok());
            let has_output = text.lines().any(|l| l.starts_with("Output:"));
            let exit = exit.or((has_output && field("Error:").is_none()).then_some(0));
            // The output section runs until the next "Name: " field.
            let output = match text.find("Output: ") {
                Some(i) => {
                    let rest = &text[i + 8..];
                    let end = [
                        "\nError: ",
                        "\nExit Code: ",
                        "\nSignal: ",
                        "\nBackground PIDs: ",
                        "\nProcess Group PGID: ",
                    ]
                    .iter()
                    .filter_map(|m| rest.find(m))
                    .min()
                    .unwrap_or(rest.len());
                    rest[..end].to_string()
                }
                None => text.to_string(),
            };
            let output = if output.trim() == "(empty)" {
                String::new()
            } else {
                output
            };
            (exit, None, output)
        }
        Agent::Claude => (None, None, text.to_string()),
    }
}

/// Events from one hook call of `agent`.
pub fn from_hook(agent: Agent, v: &Value) -> Vec<Event> {
    let name = s(v, "hook_event_name").unwrap_or_default();
    let tool = s(v, "tool_name").unwrap_or_default();
    let input = v.get("tool_input").cloned().unwrap_or(Value::Null);
    let id = s(v, "tool_use_id").map(str::to_string);
    let cwd = s(v, "cwd").map(str::to_string);
    let ev = |kind: &str| Event {
        cwd: cwd.clone(),
        ..Event::new(kind, agent.name())
    };
    // Gemini's names, as Claude/Codex ones.
    let name = match name {
        "BeforeTool" => "PreToolUse",
        "AfterTool" => "PostToolUse",
        "BeforeAgent" => "UserPromptSubmit",
        "AfterAgent" => "Stop",
        n => n,
    };
    match name {
        "UserPromptSubmit" => vec![Event {
            text: s(v, "prompt").map(|p| clip(p, MAX_TEXT)),
            ..ev("prompt")
        }],
        "PreToolUse" if agent.is_shell(tool) => vec![Event {
            id,
            command: s(&input, "command").map(|c| clip(c, MAX_TEXT)),
            tool: Some(tool.into()),
            text: s(&input, "description").map(str::to_string),
            ..ev("command_start")
        }],
        "PostToolUse" | "PostToolUseFailure" => {
            let mut failed = name == "PostToolUseFailure";
            if agent.is_shell(tool) {
                let text = v
                    .get("tool_response")
                    .and_then(response_text)
                    .or_else(|| s(v, "error").map(str::to_string))
                    .unwrap_or_default();
                let (exit, duration_ms, output) = shell_result(agent, &text);
                if v.get("tool_response")
                    .and_then(|r| r.get("error"))
                    .is_some_and(|e| !e.is_null())
                {
                    failed = true;
                }
                return vec![Event {
                    id,
                    command: s(&input, "command").map(|c| clip(c, MAX_TEXT)),
                    tool: Some(tool.into()),
                    output: Some(clip(output.trim_end(), MAX_TEXT)).filter(|o| !o.is_empty()),
                    exit,
                    duration_ms,
                    failed,
                    ..ev("command")
                }];
            }
            let paths = edited_paths(agent, tool, &input);
            if !paths.is_empty() {
                return paths
                    .into_iter()
                    .map(|path| Event {
                        id: id.clone(),
                        path: Some(path),
                        tool: Some(tool.into()),
                        failed,
                        ..ev("edit")
                    })
                    .collect();
            }
            vec![Event {
                id,
                tool: Some(tool.into()),
                text: tool_summary(&input),
                failed,
                ..ev("tool")
            }]
        }
        "Notification" | "PermissionRequest" => vec![Event {
            text: s(v, "message")
                .map(str::to_string)
                .or_else(|| s(&input, "description").map(str::to_string))
                .or_else(|| (!tool.is_empty()).then(|| format!("permission to use {tool}"))),
            ..ev("waiting")
        }],
        "Stop" => vec![ev("done")],
        "SessionStart" => vec![Event {
            text: s(v, "source").map(str::to_string),
            ..ev("session_start")
        }],
        "SessionEnd" => vec![Event {
            text: s(v, "reason").map(str::to_string),
            ..ev("session_end")
        }],
        _ => Vec::new(),
    }
}

/// Claude Code's hook calls.
#[cfg(test)]
pub fn from_claude(v: &Value) -> Vec<Event> {
    from_hook(Agent::Claude, v)
}

/// `cyberterm +hook <claude|codex|gemini|event> [--session <id>]`: reads
/// hook input on stdin and appends to the session's flight log. It never
/// fails the caller (always exits 0, prints nothing) -- an agent must not
/// stop because its log couldn't be written -- and outside Cyberterm's
/// agent sessions it does nothing (hooks installed with `+agent setup`
/// run for every session of that agent).
pub fn run_hook(args: &[String]) {
    let mut kind = "event";
    let mut session = std::env::var("CYBERTERM_AGENT_ID").ok();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--session" => session = iter.next().cloned(),
            "claude" => kind = "claude",
            "codex" => kind = "codex",
            "gemini" => kind = "gemini",
            "event" => kind = "event",
            _ => {}
        }
    }
    // Read everything first, so the agent never writes into a closed pipe.
    let mut input = String::new();
    let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut input);
    let Some(session) = session.filter(|s| crate::agent::load(s).is_ok()) else {
        return;
    };
    let events: Vec<Event> = match Agent::parse(kind) {
        Some(agent) => serde_json::from_str(&input)
            .map(|v| from_hook(agent, &v))
            .unwrap_or_default(),
        // One event or several, as a JSON value per line.
        None => input
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| match from_open(&v) {
                Ok(e) => Some(e),
                Err(e) => {
                    eprintln!("cyberterm +hook: {e}");
                    None
                }
            })
            .collect(),
    };
    let _ = append(&log_path(&session), &events);
}

// ----------------------------------------------------------------------
// The shell recorder
// ----------------------------------------------------------------------

const BASH_RECORDER: &str = r#"# Cyberterm flight log: records the commands a coding agent runs through
# `bash -c` (see src/flight_log.rs). Loaded through BASH_ENV in agents that
# `cyberterm +agent` started; a BASH_ENV of your own still runs first.
if [ -n "${CYBERTERM_BASH_ENV:-}" ] && [ -r "$CYBERTERM_BASH_ENV" ]; then
  . "$CYBERTERM_BASH_ENV"
fi
if [ -n "${CYBERTERM_FLIGHT_LOG:-}" ] && [ -z "${__CYBERTERM_FLIGHT:-}" ] && [ -n "${BASH_EXECUTION_STRING:-}" ]; then
  export __CYBERTERM_FLIGHT=1
  __ct_esc() {
    local s=${1:0:4000}
    s=${s//\\/\\\\}; s=${s//\"/\\\"}; s=${s//$'\n'/\\n}; s=${s//$'\t'/\\t}
    s=${s//[$'\001'-$'\037']/}
    __ct_r=$s
  }
  __ct_ms() { local t=${EPOCHREALTIME/[.,]/}; __ct_r=${t:0:13}; }
  __ct_ms; __ct_t0=$__ct_r; __ct_id="sh-$$-$__ct_t0"
  __ct_esc "$BASH_EXECUTION_STRING"; __ct_cmd=$__ct_r
  __ct_esc "$PWD"; __ct_cwd=$__ct_r
  printf '{"v":1,"t":%s,"kind":"command_start","source":"shell","id":"%s","command":"%s","cwd":"%s"}\n' \
    "$__ct_t0" "$__ct_id" "$__ct_cmd" "$__ct_cwd" >>"$CYBERTERM_FLIGHT_LOG" 2>/dev/null
  __ct_end() {
    local rc=$1; __ct_ms
    printf '{"v":1,"t":%s,"kind":"command","source":"shell","id":"%s","command":"%s","cwd":"%s","exit":%s,"duration_ms":%s}\n' \
      "$__ct_r" "$__ct_id" "$__ct_cmd" "$__ct_cwd" "$rc" "$((__ct_r - __ct_t0))" >>"$CYBERTERM_FLIGHT_LOG" 2>/dev/null
  }
  trap '__ct_end $?' EXIT
fi
"#;

/// Sets up recording in a `zsh -c` shell. The EXIT trap also keeps zsh
/// from replacing itself with a lone command (which would skip it), and is
/// set again after each startup file because /etc/zsh/zprofile's `emulate
/// sh` clears it -- always at top level: in zsh, an EXIT trap set inside a
/// function fires when the function returns. zshexit is the fallback
/// should a command replace the trap.
const ZSH_RECORDER: &str = r#"# Cyberterm flight log (zsh) -- see src/flight_log.rs.
if [[ -n ${CYBERTERM_FLIGHT_LOG-} && -z ${__CYBERTERM_FLIGHT-} && -n ${ZSH_EXECUTION_STRING-} && ! -o interactive ]]; then
  export __CYBERTERM_FLIGHT=1
  zmodload zsh/datetime 2>/dev/null
  __ct_esc() {
    local s=${1[1,4000]}
    s=${s//\\/\\\\}; s=${s//\"/\\\"}; s=${s//$'\n'/\\n}; s=${s//$'\t'/\\t}
    s=${s//[$'\001'-$'\037']/}
    __ct_r=$s
  }
  __ct_ms() { __ct_r=${EPOCHREALTIME/[.,]/}; __ct_r=${__ct_r[1,13]} }
  __ct_ms; __ct_t0=$__ct_r; __ct_id="sh-$$-$__ct_t0"
  __ct_esc "$ZSH_EXECUTION_STRING"; __ct_cmd=$__ct_r
  __ct_esc "$PWD"; __ct_cwd=$__ct_r
  print -r -- "{\"v\":1,\"t\":$__ct_t0,\"kind\":\"command_start\",\"source\":\"shell\",\"id\":\"$__ct_id\",\"command\":\"$__ct_cmd\",\"cwd\":\"$__ct_cwd\"}" >>$CYBERTERM_FLIGHT_LOG 2>/dev/null
  __ct_done=
  __ct_end() {
    [[ -n $__ct_done ]] && return
    __ct_done=1
    local rc=$1; __ct_ms
    print -r -- "{\"v\":1,\"t\":$__ct_r,\"kind\":\"command\",\"source\":\"shell\",\"id\":\"$__ct_id\",\"command\":\"$__ct_cmd\",\"cwd\":\"$__ct_cwd\",\"exit\":$rc,\"duration_ms\":$(( __ct_r - __ct_t0 ))}" >>$CYBERTERM_FLIGHT_LOG 2>/dev/null
  }
  __ct_atexit() { __ct_end $? }
  zshexit_functions+=(__ct_atexit)
  trap '__ct_end $?' EXIT
fi
"#;

/// The startup files in Cyberterm's ZDOTDIR. Each runs the matching file
/// of yours, at top level and with your ZDOTDIR set, so your config
/// behaves exactly as usual; the last one zsh reads hands ZDOTDIR back.
/// `last` is the condition under which this is the last file.
fn zsh_startup(file: &str, last: &str, first: bool) -> String {
    let mut s = String::from("# Cyberterm flight log -- see src/flight_log.rs.\n");
    if first {
        s.push_str(
            "__ct_dir=$ZDOTDIR\n\
             __ct_user=${CYBERTERM_ZDOTDIR:-$HOME}\n\
             source $__ct_dir/flight.zsh\n",
        );
    }
    s.push_str(&format!(
        "ZDOTDIR=$__ct_user\n\
         [[ -r $ZDOTDIR/{file} ]] && source $ZDOTDIR/{file}\n\
         __ct_user=$ZDOTDIR\n\
         (( ${{+functions[__ct_end]}} )) && trap '__ct_end $?' EXIT\n\
         if {last}; then\n  \
           [[ -z ${{CYBERTERM_ZDOTDIR-}} && $ZDOTDIR == $HOME ]] && unset ZDOTDIR\n\
         else\n  \
           ZDOTDIR=$__ct_dir\n\
         fi\n"
    ));
    s
}

/// Writes the recorder scripts (when they changed) and returns the
/// environment that turns them on for an agent.
pub fn recorder_env(log: &Path) -> std::io::Result<Vec<(String, String)>> {
    recorder_env_in(&crate::agent::state_dir().join("shell"), log)
}

fn recorder_env_in(dir: &Path, log: &Path) -> std::io::Result<Vec<(String, String)>> {
    let zdir = dir.join("zsh");
    std::fs::create_dir_all(&zdir)?;
    let write = |path: PathBuf, text: &str| -> std::io::Result<()> {
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text) {
            std::fs::write(&path, text)?;
        }
        Ok(())
    };
    write(dir.join("flight.bash"), BASH_RECORDER)?;
    write(zdir.join("flight.zsh"), ZSH_RECORDER)?;
    write(
        zdir.join(".zshenv"),
        &zsh_startup(".zshenv", "[[ ! -o login && ! -o interactive ]]", true),
    )?;
    write(
        zdir.join(".zprofile"),
        &zsh_startup(".zprofile", "false", false),
    )?;
    write(
        zdir.join(".zshrc"),
        &zsh_startup(".zshrc", "[[ ! -o login ]]", false),
    )?;
    write(zdir.join(".zlogin"), &zsh_startup(".zlogin", "true", false))?;

    let var = |k: &str| std::env::var(k).unwrap_or_default();
    Ok(vec![
        ("CYBERTERM_FLIGHT_LOG".into(), log.to_string_lossy().into()),
        ("CYBERTERM_BASH_ENV".into(), var("BASH_ENV")),
        (
            "BASH_ENV".into(),
            dir.join("flight.bash").to_string_lossy().into(),
        ),
        ("CYBERTERM_ZDOTDIR".into(), var("ZDOTDIR")),
        ("ZDOTDIR".into(), zdir.to_string_lossy().into()),
    ])
}

// ----------------------------------------------------------------------
// The timeline
// ----------------------------------------------------------------------

/// One line of the timeline: events folded together (a command's start,
/// its end, and the agent's own report of it become one entry).
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Prompt {
        t: u64,
        text: String,
    },
    Command {
        t: u64,
        command: String,
        cwd: Option<String>,
        /// `None` while it runs (or if its end was never seen).
        exit: Option<i32>,
        failed: bool,
        duration_ms: Option<u64>,
        output: Option<String>,
        running: bool,
    },
    Edit {
        t: u64,
        path: String,
        tool: Option<String>,
    },
    Tool {
        t: u64,
        tool: String,
        text: Option<String>,
    },
    Waiting {
        t: u64,
        text: Option<String>,
    },
    Done {
        t: u64,
    },
}

/// The command a shell actually ran, without wrappers agents put around
/// it: Claude Code's `source <snapshot> && ... && eval '<command>'
/// [< /dev/null] && pwd -P >| <file>`.
pub fn unwrap_command(raw: &str) -> String {
    let raw = raw.trim();
    let (Some(start), Some(tail)) = (raw.find(" eval '"), raw.rfind(" && pwd -P")) else {
        return raw.to_string();
    };
    let body = raw[..tail].trim_end();
    let body = body.strip_suffix("< /dev/null").unwrap_or(body).trim_end();
    match body.strip_suffix('\'') {
        Some(body) if body.len() >= start + 7 => body[start + 7..]
            .replace("'\"'\"'", "'")
            .replace("'\\''", "'"),
        _ => raw.to_string(),
    }
}

/// A shell running one of Cyberterm's own hooks (agents run hook commands
/// through a shell, so the recorder sees them).
fn is_hook_call(command: &str) -> bool {
    let words = crate::agent::split_command(command).unwrap_or_default();
    words.first().is_some_and(|p| p.ends_with("cyberterm"))
        && words.get(1).map(String::as_str) == Some("+hook")
}

fn same_command(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    norm(a) == norm(b)
}

/// Folds events into timeline entries, oldest first.
pub fn timeline(events: &[Event]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    // Command entries by id, and agent-reported commands still waiting
    // for the shell's exit code (index into `out`).
    let mut by_id: std::collections::HashMap<String, usize> = Default::default();
    let mut agent_cmds: Vec<usize> = Vec::new();
    // With an agent's hooks on, its own reports are the list of what it
    // ran; shell commands that match none of them are its internals (the
    // shell snapshot Claude Code takes at start), so they're left out.
    let agent_reports_commands = events.iter().any(|e| Agent::parse(&e.source).is_some());

    for e in events {
        match e.kind.as_str() {
            "prompt" => out.push(Entry::Prompt {
                t: e.t,
                text: e.text.clone().unwrap_or_default(),
            }),
            "command_start" | "command" => {
                let finished = e.kind == "command";
                let mut command = e.command.clone().unwrap_or_default();
                if e.source == "shell" {
                    command = unwrap_command(&command);
                }
                // The same command already on the timeline: by id, or by
                // its text -- the shell recorder's report of an agent's
                // command, or the end of one whose agent gives no ids
                // (Gemini).
                let existing =
                    e.id.as_ref()
                        .and_then(|id| by_id.get(id).copied())
                        .or_else(|| {
                            let from_shell = e.source == "shell";
                            let unpaired_end = !from_shell && finished && e.id.is_none();
                            agent_cmds.iter().rev().copied().find(|&i| match &out[i] {
                                Entry::Command {
                                    command: c,
                                    exit,
                                    running,
                                    ..
                                } => {
                                    same_command(c, &command)
                                        && ((from_shell && exit.is_none())
                                            || (unpaired_end && *running))
                                }
                                _ => false,
                            })
                        });
                if let Some(i) = existing {
                    if let Entry::Command {
                        exit,
                        failed,
                        duration_ms,
                        output,
                        running,
                        cwd,
                        ..
                    } = &mut out[i]
                    {
                        if finished {
                            *exit = e.exit.or(*exit);
                            *failed |= e.failed;
                            *duration_ms = e.duration_ms.or(*duration_ms);
                            if e.output.is_some() {
                                *output = e.output.clone();
                            }
                            *running = false;
                        }
                        if cwd.is_none() {
                            *cwd = e.cwd.clone();
                        }
                    }
                    if let Some(id) = &e.id {
                        by_id.insert(id.clone(), i);
                    }
                    continue;
                }
                if command.trim().is_empty()
                    || (e.source == "shell" && (agent_reports_commands || is_hook_call(&command)))
                {
                    continue;
                }
                out.push(Entry::Command {
                    t: e.t,
                    command,
                    cwd: e.cwd.clone(),
                    exit: e.exit,
                    failed: e.failed,
                    duration_ms: e.duration_ms,
                    output: e.output.clone(),
                    running: !finished,
                });
                let i = out.len() - 1;
                if let Some(id) = &e.id {
                    by_id.insert(id.clone(), i);
                }
                if e.source != "shell" {
                    agent_cmds.push(i);
                }
            }
            "edit" => {
                if let Some(path) = &e.path {
                    out.push(Entry::Edit {
                        t: e.t,
                        path: path.clone(),
                        tool: e.tool.clone(),
                    });
                }
            }
            "tool" => out.push(Entry::Tool {
                t: e.t,
                tool: e.tool.clone().unwrap_or_else(|| "tool".into()),
                text: e.text.clone(),
            }),
            "waiting" => out.push(Entry::Waiting {
                t: e.t,
                text: e.text.clone(),
            }),
            "done" => out.push(Entry::Done { t: e.t }),
            _ => {}
        }
    }
    out
}

/// Where a session stands, from its timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing recorded yet.
    Idle,
    Working,
    /// Asked for you (a permission, a question) and hasn't moved since.
    Waiting,
    /// Finished its turn.
    Done,
}

/// A session at a glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub state: State,
    pub commands: usize,
    pub failed: usize,
    pub edits: usize,
    /// When the latest entry happened.
    pub last_t: u64,
}

pub fn summary(entries: &[Entry]) -> Summary {
    let failed = |e: &Entry| matches!(e, Entry::Command { exit, failed, .. } if *failed || exit.is_some_and(|c| c != 0));
    let state = match entries.last() {
        None => State::Idle,
        Some(Entry::Waiting { .. }) => State::Waiting,
        Some(Entry::Done { .. }) => State::Done,
        Some(_) => State::Working,
    };
    Summary {
        state,
        commands: entries
            .iter()
            .filter(|e| matches!(e, Entry::Command { .. }))
            .count(),
        failed: entries.iter().filter(|e| failed(e)).count(),
        edits: entries
            .iter()
            .filter(|e| matches!(e, Entry::Edit { .. }))
            .count(),
        last_t: entries.last().map(Entry::t).unwrap_or(0),
    }
}

impl Entry {
    pub fn t(&self) -> u64 {
        match self {
            Entry::Prompt { t, .. }
            | Entry::Command { t, .. }
            | Entry::Edit { t, .. }
            | Entry::Tool { t, .. }
            | Entry::Waiting { t, .. }
            | Entry::Done { t } => *t,
        }
    }
}

// ----------------------------------------------------------------------
// `cyberterm +agent log`
// ----------------------------------------------------------------------

pub fn clock(t: u64) -> String {
    // Local time of day, from the C library (no time zone crate here).
    let secs = (t / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// The timeline as text, for `+agent log`.
pub fn render(entries: &[Entry], with_output: bool, color: bool) -> String {
    let c = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let mut out = String::new();
    for e in entries {
        let line = match e {
            Entry::Prompt { t, text } => format!(
                "{} {} {}",
                c("2", &clock(*t)),
                c("1;35", "»"),
                text.lines().next().unwrap_or_default()
            ),
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
                let mark = match (exit, failed, running) {
                    (Some(0), false, _) => c("32", "✓"),
                    (Some(code), _, _) => c("31", &format!("✗ {code}")),
                    (None, true, _) => c("31", "✗"),
                    (None, false, true) => c("33", "…"),
                    (None, false, false) => c("2", "·"),
                };
                let took = duration_ms
                    .map(|d| format!(" {}", c("2", &crate::blocks::format_duration(d))))
                    .unwrap_or_default();
                let mut s = format!(
                    "{} {} {}{}",
                    c("2", &clock(*t)),
                    mark,
                    command.lines().next().unwrap_or_default(),
                    took
                );
                if with_output {
                    if let Some(o) = output {
                        for l in o
                            .lines()
                            .rev()
                            .take(8)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                        {
                            s.push_str(&format!("\n           {}", c("2", l)));
                        }
                    }
                }
                s
            }
            Entry::Edit { t, path, tool } => format!(
                "{} {} {}{}",
                c("2", &clock(*t)),
                c("36", "✎"),
                path,
                tool.as_ref()
                    .map(|t| c("2", &format!("  ({t})")))
                    .unwrap_or_default()
            ),
            Entry::Tool { t, tool, text } => format!(
                "{} {} {}{}",
                c("2", &clock(*t)),
                c("2", "○"),
                tool,
                text.as_ref()
                    .map(|x| c("2", &format!("  {x}")))
                    .unwrap_or_default()
            ),
            Entry::Waiting { t, text } => format!(
                "{} {} {}",
                c("2", &clock(*t)),
                c("1;33", "◆ needs you"),
                text.clone().unwrap_or_default()
            ),
            Entry::Done { t } => format!("{} {}", c("2", &clock(*t)), c("32", "■ done")),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn open_format_events_are_checked_and_stamped() {
        let e = from_open(&json!({"kind": "command", "command": "make", "exit": 2})).unwrap();
        assert_eq!((e.v, e.source.as_str(), e.exit), (1, "hook", Some(2)));
        assert!(e.t > 0);
        assert!(from_open(&json!({"kind": "explode"})).is_err());
        assert!(from_open(&json!("nope")).is_err());
        let long = "x".repeat(MAX_TEXT + 50);
        let e = from_open(&json!({"kind": "prompt", "text": long})).unwrap();
        assert!(e.text.unwrap().chars().count() <= MAX_TEXT + 1);
    }

    #[test]
    fn claude_hooks_become_events() {
        let pre = json!({
            "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "t1",
            "cwd": "/w", "tool_input": {"command": "cargo test", "description": "Run tests"}
        });
        let e = &from_claude(&pre)[0];
        assert_eq!(e.kind, "command_start");
        assert_eq!(e.command.as_deref(), Some("cargo test"));
        assert_eq!(e.id.as_deref(), Some("t1"));

        let post = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "t1",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"stdout": "running 3 tests\nok", "stderr": "", "interrupted": false}
        });
        let e = &from_claude(&post)[0];
        assert_eq!(e.kind, "command");
        assert_eq!(e.output.as_deref(), Some("running 3 tests\nok"));

        let edit = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Edit",
            "tool_input": {"file_path": "/w/src/a.rs", "old_string": "a", "new_string": "b"}
        });
        assert_eq!(from_claude(&edit)[0].path.as_deref(), Some("/w/src/a.rs"));

        let read = json!({"hook_event_name": "PostToolUse", "tool_name": "Read",
                          "tool_input": {"file_path": "/w/README.md"}});
        let e = &from_claude(&read)[0];
        assert_eq!(
            (e.kind.as_str(), e.text.as_deref()),
            ("tool", Some("/w/README.md"))
        );

        let fail = json!({"hook_event_name": "PostToolUseFailure", "tool_name": "Bash",
                          "tool_input": {"command": "false"}, "error": "Exit code 1"});
        let e = &from_claude(&fail)[0];
        assert!(e.failed);
        assert_eq!(e.output.as_deref(), Some("Exit code 1"));

        let wait = json!({"hook_event_name": "Notification", "message": "Claude needs your permission to use Bash"});
        assert_eq!(from_claude(&wait)[0].kind, "waiting");
        assert_eq!(
            from_claude(&json!({"hook_event_name": "Stop"}))[0].kind,
            "done"
        );
        let prompt = json!({"hook_event_name": "UserPromptSubmit", "prompt": "fix it"});
        assert_eq!(from_claude(&prompt)[0].text.as_deref(), Some("fix it"));
        assert!(from_claude(&json!({"hook_event_name": "Mystery"})).is_empty());
    }

    #[test]
    fn codex_hooks_become_events() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: src/new.rs\n+fn n() {}\n*** End Patch";
        let edit = json!({
            "hook_event_name": "PostToolUse", "tool_name": "apply_patch", "tool_use_id": "c1",
            "tool_input": {"command": patch}, "tool_response": "Success"
        });
        let paths: Vec<String> = from_hook(Agent::Codex, &edit)
            .into_iter()
            .filter_map(|e| e.path)
            .collect();
        assert_eq!(paths, vec!["src/a.rs", "src/new.rs"]);

        let run = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "c2",
            "tool_input": {"command": "cargo test"},
            "tool_response": "Exit code: 101\nWall time: 5.3 seconds\nOutput:\ntest result: FAILED"
        });
        let e = &from_hook(Agent::Codex, &run)[0];
        assert_eq!(
            (e.source.as_str(), e.exit, e.duration_ms),
            ("codex", Some(101), Some(5300))
        );
        assert_eq!(e.output.as_deref(), Some("test result: FAILED"));

        let ask = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                         "tool_input": {"command": "rm -rf build", "description": "clean the build"}});
        let e = &from_hook(Agent::Codex, &ask)[0];
        assert_eq!(
            (e.kind.as_str(), e.text.as_deref()),
            ("waiting", Some("clean the build"))
        );
        assert_eq!(
            from_hook(
                Agent::Codex,
                &json!({"hook_event_name": "Stop", "last_assistant_message": "done"})
            )[0]
            .kind,
            "done"
        );
    }

    #[test]
    fn codex_overrides_are_valid_toml_hooks() {
        let hook = "/opt/my \"apps\"/cyberterm +hook codex";
        for o in codex_overrides(hook) {
            let (key, value) = o.split_once('=').unwrap();
            let doc: toml::Value =
                toml::from_str(&format!("v = {value}")).unwrap_or_else(|e| panic!("{o}: {e}"));
            let group = &doc["v"][0];
            assert_eq!(group["hooks"][0]["command"].as_str(), Some(hook), "{o}");
            assert_eq!(group["hooks"][0]["type"].as_str(), Some("command"));
            let tools = key.ends_with("ToolUse");
            assert_eq!(group.get("matcher").is_some(), tools, "{o}");
        }
    }

    #[test]
    fn gemini_hooks_become_events() {
        let before = json!({"hook_event_name": "BeforeTool", "tool_name": "run_shell_command",
                            "tool_input": {"command": "npm test", "description": "Run tests"}});
        let e = &from_hook(Agent::Gemini, &before)[0];
        assert_eq!(
            (e.kind.as_str(), e.command.as_deref(), e.id.as_deref()),
            ("command_start", Some("npm test"), None)
        );

        // A command that passed: no "Exit Code" line means 0.
        let ok = json!({"hook_event_name": "AfterTool", "tool_name": "run_shell_command",
                        "tool_input": {"command": "npm test"},
                        "tool_response": {"llmContent": "<untrusted_context> Output: 12 passing\nProcess Group PGID: 4242 </untrusted_context>", "returnDisplay": "12 passing"}});
        let e = &from_hook(Agent::Gemini, &ok)[0];
        assert_eq!((e.exit, e.output.as_deref()), (Some(0), Some("12 passing")));

        let failed = json!({"hook_event_name": "AfterTool", "tool_name": "run_shell_command",
                            "tool_input": {"command": "npm test"},
                            "tool_response": {"llmContent": "Output: 1 failing\nExit Code: 1", "error": null}});
        assert_eq!(from_hook(Agent::Gemini, &failed)[0].exit, Some(1));

        let edit = json!({"hook_event_name": "AfterTool", "tool_name": "replace",
                          "tool_input": {"file_path": "/w/app.py", "old_string": "a", "new_string": "b"}});
        assert_eq!(
            from_hook(Agent::Gemini, &edit)[0].path.as_deref(),
            Some("/w/app.py")
        );
        let read = json!({"hook_event_name": "AfterTool", "tool_name": "read_file", "tool_input": {"absolute_path": "/w/README.md"}});
        assert_eq!(
            from_hook(Agent::Gemini, &read)[0].text.as_deref(),
            Some("/w/README.md")
        );

        let prompt = json!({"hook_event_name": "BeforeAgent", "prompt": "fix it"});
        assert_eq!(from_hook(Agent::Gemini, &prompt)[0].kind, "prompt");
        let done = json!({"hook_event_name": "AfterAgent", "prompt": "fix it", "prompt_response": "Fixed."});
        assert_eq!(from_hook(Agent::Gemini, &done)[0].kind, "done");
        let note = json!({"hook_event_name": "Notification", "notification_type": "ToolPermission", "message": "Allow npm test?"});
        assert_eq!(
            from_hook(Agent::Gemini, &note)[0].text.as_deref(),
            Some("Allow npm test?")
        );
    }

    #[test]
    fn hook_calls_are_not_commands() {
        assert!(is_hook_call("/usr/bin/cyberterm +hook gemini"));
        assert!(is_hook_call("'/a b/cyberterm' +hook codex"));
        assert!(!is_hook_call("cyberterm +agent list"));
        assert!(!is_hook_call("git commit -m '+hook'"));
        let shell = ev("command", "shell", |e| {
            e.command = Some("cyberterm +hook event".into());
            e.exit = Some(0);
        });
        assert!(timeline(&[shell]).is_empty());
    }

    #[test]
    fn gemini_commands_pair_without_ids() {
        let start = from_hook(
            Agent::Gemini,
            &json!({"hook_event_name": "BeforeTool", "tool_name": "run_shell_command", "tool_input": {"command": "make"}}),
        );
        let end = from_hook(
            Agent::Gemini,
            &json!({"hook_event_name": "AfterTool", "tool_name": "run_shell_command", "tool_input": {"command": "make"}, "tool_response": {"llmContent": "Output: done"}}),
        );
        let t = timeline(&[start, end].concat());
        assert_eq!(t.len(), 1, "{t:#?}");
        assert!(matches!(
            &t[0],
            Entry::Command {
                exit: Some(0),
                running: false,
                ..
            }
        ));
    }

    #[test]
    fn claude_settings_register_every_hook() {
        let v: Value = serde_json::from_str(&claude_settings("/bin/ct +hook claude")).unwrap();
        for k in [
            "PreToolUse",
            "PostToolUse",
            "Stop",
            "UserPromptSubmit",
            "Notification",
        ] {
            assert_eq!(
                v["hooks"][k][0]["hooks"][0]["command"], "/bin/ct +hook claude",
                "{k}"
            );
        }
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "*");
    }

    #[test]
    fn claude_shell_wrappers_as_seen_in_a_real_session() {
        // Claude Code 2.1.292, from a real session's flight log.
        let raw = "source /home/raven/.claude/shell-snapshots/snapshot-zsh-1791664476397-plxaym.sh 2>/dev/null || true && setopt NO_EXTENDED_GLOB NO_BARE_GLOB_QUAL 2>/dev/null || true && { \\builtin unalias -- 'unsetenv'; \\builtin unset -f -- 'unsetenv'; } >/dev/null 2>&1 || true && eval 'ls -la; cat PKGBUILD 2>/dev/null | head -60' < /dev/null && pwd -P >| /home/raven/.cache/omarchy/tmp/claude-ac3a-cwd";
        assert_eq!(
            unwrap_command(raw),
            "ls -la; cat PKGBUILD 2>/dev/null | head -60"
        );

        let shell = |kind: &str, id: &str, cmd: &str| {
            ev(kind, "shell", |e| {
                e.id = Some(id.into());
                e.command = Some(cmd.into());
                if kind == "command" {
                    e.exit = Some(0);
                    e.duration_ms = Some(1500);
                }
            })
        };
        let wrapped = raw.to_string();
        let events = vec![
            // Claude's start-up shell snapshot: internals, not its work.
            shell("command_start", "sh-1", "env"),
            shell("command", "sh-1", "env"),
            ev("command_start", "claude", |e| {
                e.id = Some("tu1".into());
                e.command = Some("ls -la; cat PKGBUILD 2>/dev/null | head -60".into());
            }),
            shell("command_start", "sh-2", &wrapped),
            shell("command", "sh-2", &wrapped),
            ev("command", "claude", |e| {
                e.id = Some("tu1".into());
                e.command = Some("ls -la; cat PKGBUILD 2>/dev/null | head -60".into());
                e.output = Some("package() {".into());
            }),
        ];
        let t = timeline(&events);
        assert_eq!(t.len(), 1, "{t:#?}");
        assert!(matches!(
            &t[0],
            Entry::Command {
                exit: Some(0),
                duration_ms: Some(1500),
                output: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn agents_without_hooks_keep_their_shell_commands() {
        let events = vec![
            ev("prompt", "hook", |e| e.text = Some("go".into())),
            ev("command", "shell", |e| {
                e.id = Some("sh-1".into());
                e.command = Some("npm run dev".into());
                e.exit = Some(0);
            }),
        ];
        assert_eq!(timeline(&events).len(), 2);
    }

    #[test]
    fn claude_shell_wrappers_are_unwrapped() {
        let raw = "source /h/.claude/shell-snapshots/s.sh 2>/dev/null || true && setopt NO_EXTENDED_GLOB 2>/dev/null || true && eval 'grep -n '\"'\"'fn main'\"'\"' src/main.rs' && pwd -P >| /tmp/claude-1-cwd";
        assert_eq!(unwrap_command(raw), "grep -n 'fn main' src/main.rs");
        assert_eq!(unwrap_command("  cargo test "), "cargo test");
    }

    fn ev(kind: &str, source: &str, f: impl FnOnce(&mut Event)) -> Event {
        let mut e = Event::new(kind, source);
        f(&mut e);
        e
    }

    #[test]
    fn the_timeline_folds_starts_ends_and_both_reports_of_a_command() {
        let events = vec![
            ev("prompt", "claude", |e| e.text = Some("fix the test".into())),
            // Claude reports the command; the shell recorder sees it run.
            ev("command_start", "claude", |e| {
                e.id = Some("t1".into());
                e.command = Some("cargo test".into());
            }),
            ev("command_start", "shell", |e| {
                e.id = Some("sh-1".into());
                e.command = Some("source s.sh && eval 'cargo  test' && pwd -P >| /tmp/x".into());
            }),
            ev("command", "shell", |e| {
                e.id = Some("sh-1".into());
                e.command = Some("source s.sh && eval 'cargo  test' && pwd -P >| /tmp/x".into());
                e.exit = Some(101);
                e.duration_ms = Some(5300);
            }),
            ev("command", "claude", |e| {
                e.id = Some("t1".into());
                e.command = Some("cargo test".into());
                e.output = Some("test result: FAILED".into());
            }),
            ev("edit", "claude", |e| e.path = Some("src/a.rs".into())),
            // A command only the shell saw: with Claude's hooks on, that's
            // Claude's internals, so it's left out.
            ev("command_start", "shell", |e| {
                e.id = Some("sh-2".into());
                e.command = Some("npm run dev".into());
            }),
            ev("waiting", "claude", |e| e.text = Some("permission".into())),
            ev("done", "claude", |_| {}),
        ];
        let t = timeline(&events);
        assert_eq!(t.len(), 5, "{t:#?}");
        match &t[1] {
            Entry::Command {
                command,
                exit,
                duration_ms,
                output,
                running,
                ..
            } => {
                assert_eq!(command, "cargo test");
                assert_eq!(*exit, Some(101));
                assert_eq!(*duration_ms, Some(5300));
                assert_eq!(output.as_deref(), Some("test result: FAILED"));
                assert!(!running);
            }
            other => panic!("{other:?}"),
        }
        assert!(!t
            .iter()
            .any(|e| matches!(e, Entry::Command { command, .. } if command == "npm run dev")));
        let text = render(&t, true, false);
        assert!(text.contains("✗ 101 cargo test 5.3s"), "{text}");
        assert!(text.contains("◆ needs you permission"), "{text}");
    }

    /// Runs `shell -c|-lc script` with the recorder on, in an empty HOME.
    fn recorded(shell: &str, flag: &str, script: &str) -> Option<Vec<Event>> {
        let ok = std::process::Command::new(shell)
            .arg("-c")
            .arg("true")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("{shell} not installed; skipped");
            return None;
        }
        let root = std::env::temp_dir().join(format!(
            "cyberterm-rec-{shell}-{}-{}",
            std::process::id(),
            crate::shell::tap::now_ms()
        ));
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let log = root.join("log.jsonl");
        let env = recorder_env_in(&root.join("shell"), &log).unwrap();
        std::process::Command::new(shell)
            .arg(flag)
            .arg(script)
            .env("HOME", &home)
            .env_remove("ZDOTDIR")
            .env_remove("BASH_ENV")
            .envs(env)
            .output()
            .unwrap();
        let events = read(&log);
        let _ = std::fs::remove_dir_all(&root);
        Some(events)
    }

    fn ended(events: &[Event]) -> Vec<(String, Option<i32>)> {
        events
            .iter()
            .filter(|e| e.kind == "command")
            .map(|e| (e.command.clone().unwrap_or_default(), e.exit))
            .collect()
    }

    #[test]
    fn the_shell_recorder_logs_commands_and_exit_codes() {
        for (shell, flag) in [
            ("bash", "-c"),
            ("bash", "-lc"),
            ("zsh", "-c"),
            ("zsh", "-lc"),
        ] {
            // A lone external command (which shells like to exec into),
            // a failing one, and quoting that has to survive JSON.
            for (script, exit) in [
                ("ls /nonexistent-cyberterm", 2),
                ("false", 1),
                ("echo \"a\\b\tc\" >/dev/null; exit 4", 4),
            ] {
                let Some(events) = recorded(shell, flag, script) else {
                    break;
                };
                assert_eq!(
                    ended(&events),
                    vec![(script.to_string(), Some(exit))],
                    "{shell} {flag} {script}: {events:#?}"
                );
                assert_eq!(events[0].kind, "command_start");
            }
        }
        // Nested shells: only the outer command.
        if let Some(events) = recorded("bash", "-c", "bash -c true") {
            assert_eq!(ended(&events).len(), 1);
        }
    }

    #[test]
    fn summaries_count_and_tell_the_state() {
        let cmd = |exit: i32| Entry::Command {
            t: 1,
            command: "x".into(),
            cwd: None,
            exit: Some(exit),
            failed: false,
            duration_ms: None,
            output: None,
            running: false,
        };
        assert_eq!(summary(&[]).state, State::Idle);
        let s = summary(&[
            cmd(0),
            cmd(2),
            Entry::Edit {
                t: 5,
                path: "a".into(),
                tool: None,
            },
        ]);
        assert_eq!(
            (s.state, s.commands, s.failed, s.edits, s.last_t),
            (State::Working, 2, 1, 1, 5)
        );
        assert_eq!(
            summary(&[cmd(0), Entry::Waiting { t: 2, text: None }]).state,
            State::Waiting
        );
        assert_eq!(summary(&[cmd(0), Entry::Done { t: 2 }]).state, State::Done);
    }

    #[test]
    fn events_round_trip_through_the_log() {
        let dir = std::env::temp_dir().join(format!(
            "cyberterm-flight-{}-{}",
            std::process::id(),
            crate::shell::tap::now_ms()
        ));
        let path = dir.join("s.jsonl");
        let a = ev("prompt", "hook", |e| e.text = Some("hi \"there\"\n".into()));
        let b = ev("done", "hook", |_| {});
        append(&path, std::slice::from_ref(&a)).unwrap();
        append(&path, std::slice::from_ref(&b)).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        assert_eq!(read(&path), vec![a, b]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
