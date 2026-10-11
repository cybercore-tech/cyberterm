// src/agent_setup.rs
//
// `cyberterm +agent setup [agent] [--remove]`: how much each agent tells
// the flight log, and the opt-in step for agents that can only take hooks
// from their own user settings.
//
// Claude Code, Codex and Copilot get their hooks per session
// (src/agent.rs) and need nothing here. The others install once, and what
// they install does nothing outside Cyberterm's agent sessions:
//
// - Gemini CLI: one hook entry per event in ~/.gemini/settings.json, next
//   to any you already have, running `cyberterm +hook gemini`.
// - Cursor: the same in ~/.cursor/hooks.json (`cyberterm +hook cursor`).
// - Hermes: a small plugin in ~/.hermes/plugins/cyberterm, enabled with
//   Hermes's own `hermes plugins enable` (so Cyberterm never edits its
//   config.yaml), that passes Hermes's hook calls to `cyberterm +hook
//   hermes`.
//
// `--remove` takes exactly those out again. Settings files are backed up
// first and written atomically, and a file that isn't plain JSON is left
// alone with instructions.

use crate::flight_log::{hooks_json, Agent};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// What marks a hook entry as Cyberterm's.
fn is_ours(command: &str, agent: Agent) -> bool {
    command.contains(&format!("+hook {}", agent.name()))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn gemini_settings_path() -> PathBuf {
    std::env::var_os("GEMINI_CLI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(home)
        .join(".gemini")
        .join("settings.json")
}

fn cursor_hooks_path() -> PathBuf {
    home().join(".cursor").join("hooks.json")
}

fn hermes_home() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".hermes"))
}

/// How hooks call this binary: `cyberterm` when the `cyberterm` on PATH
/// is this one, so reinstalling Cyberterm elsewhere doesn't break them;
/// this binary's full path otherwise.
fn hook_program() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("cyberterm"))
            .find(|p| p.is_file())
            .and_then(|p| p.canonicalize().ok())
    });
    match (&exe, &on_path) {
        (Some(e), Some(p)) if e == p => "cyberterm".to_string(),
        (Some(e), _) => e.to_string_lossy().into_owned(),
        _ => "cyberterm".to_string(),
    }
}

/// The hook command to install, for a shell.
fn hook_command(agent: Agent) -> String {
    format!(
        "{} +hook {}",
        crate::agent::shell_quote(&hook_program()),
        agent.name()
    )
}

/// Whether a hook entry is Cyberterm's: a flat `{"command": ...}` entry
/// (Cursor), or a group whose `hooks` hold one (Gemini).
fn entry_is_ours(entry: &Value, agent: Agent) -> bool {
    let command = |h: &Value| {
        h.get("command")
            .or_else(|| h.get("bash"))
            .and_then(Value::as_str)
            .is_some_and(|c| is_ours(c, agent))
    };
    command(entry)
        || entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hs| hs.iter().any(command))
}

/// Removes Cyberterm's hook entries from a settings object's `hooks`;
/// returns how many were removed. Event lists left empty go too.
fn strip_ours(settings: &mut Value, agent: Agent) -> usize {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    for entries in hooks.values_mut() {
        let Some(list) = entries.as_array_mut() else {
            continue;
        };
        let before = list.len();
        list.retain(|entry| !entry_is_ours(entry, agent));
        removed += before - list.len();
    }
    hooks.retain(|_, entries| entries.as_array().is_none_or(|l| !l.is_empty()));
    removed
}

/// Adds Cyberterm's hooks (replacing older copies of them).
fn add_ours(settings: &mut Value, agent: Agent, command: &str) {
    strip_ours(settings, agent);
    if !settings.is_object() {
        *settings = Value::Object(Default::default());
    }
    let obj = settings.as_object_mut().expect("an object");
    if agent == Agent::Cursor {
        obj.entry("version").or_insert(Value::from(1));
    }
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()));
    if !hooks.is_object() {
        *hooks = Value::Object(Default::default());
    }
    let hooks = hooks.as_object_mut().expect("an object");
    let ours = hooks_json(agent, command);
    for (event, entries) in ours.as_object().expect("an object") {
        let list = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let (Some(list), Some(entries)) = (list.as_array_mut(), entries.as_array()) {
            list.extend(entries.iter().cloned());
        }
    }
}

fn installed(path: &Path, agent: Agent) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut copy = v.clone();
    Some(strip_ours(&mut copy, agent) > 0)
}

/// Installs (or with `remove`, uninstalls) the hooks in a settings file.
fn update(path: &Path, agent: Agent, command: &str, remove: bool) -> Result<String, String> {
    let shown = crate::agent::short(path);
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if remove {
                return Ok(format!("nothing to remove ({shown} doesn't exist)"));
            }
            "{}".to_string()
        }
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    let mut settings: Value = if text.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&text).map_err(|e| {
            format!(
                "{shown} isn't plain JSON ({e}), so it was left alone.\n   \
                 Add this to its \"hooks\" yourself, or remove the comments and run setup again:\n{}",
                serde_json::to_string_pretty(&hooks_json(agent, command)).unwrap_or_default()
            )
        })?
    };
    let note = if remove {
        match strip_ours(&mut settings, agent) {
            0 => return Ok(format!("no Cyberterm hooks in {shown}")),
            n => format!(
                "removed {n} hook entr{} from {shown}",
                if n == 1 { "y" } else { "ies" }
            ),
        }
    } else {
        add_ours(&mut settings, agent, command);
        format!("added Cyberterm's hooks to {shown}")
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if path.exists() {
        let backup = path.with_extension("json.cyberterm-backup");
        std::fs::copy(path, &backup).map_err(|e| format!("backing up {shown}: {e}"))?;
    }
    let tmp = path.with_extension("json.cyberterm-tmp");
    let body = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&tmp, body).map_err(|e| format!("{shown}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{shown}: {e}"))?;
    Ok(note)
}

// ----------------------------------------------------------------------
// Hermes
// ----------------------------------------------------------------------

/// The plugin's name, folder and `plugins.enabled` entry.
const HERMES_PLUGIN: &str = "cyberterm";

/// Cyberterm's Hermes plugin. `__HOOK__` becomes the hook command as a
/// Python list, `__EVENTS__` the hooks it registers.
const HERMES_PLUGIN_PY: &str = r#""""Cyberterm's flight log for Hermes: reports what Hermes does in a
Cyberterm agent session -- prompts, tool calls and their results, the
approvals it waits for. Installed by `cyberterm +agent setup hermes`; it
does nothing outside those sessions. Remove it with
`cyberterm +agent setup hermes --remove`."""

# CYBERTERM_INTEGRATION=flight-log

import json
import os
import subprocess

_HOOK = __HOOK__
_EVENTS = __EVENTS__
# Whole conversations aren't needed, only what this turn did.
_SKIP = {"conversation_history", "messages", "middleware_trace"}
_MAX = 16000


def _plain(value):
    if isinstance(value, str):
        return value[-_MAX:]
    if value is None or isinstance(value, (bool, int, float)):
        return value
    if isinstance(value, dict):
        return {str(k): _plain(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_plain(v) for v in value[:200]]
    return _plain(str(value))


def _report(event, kwargs):
    if not os.environ.get("CYBERTERM_AGENT_ID"):
        return None
    payload = {k: _plain(v) for k, v in kwargs.items() if k not in _SKIP}
    payload["hook_event_name"] = event
    try:
        payload["cwd"] = os.getcwd()
    except OSError:
        pass
    try:
        subprocess.run(
            _HOOK,
            input=json.dumps(payload),
            text=True,
            timeout=5,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except Exception:
        pass
    # Never a directive: Cyberterm only watches.
    return None


def _callback(event):
    def callback(**kwargs):
        return _report(event, kwargs)

    return callback


def register(ctx):
    for event in _EVENTS:
        ctx.register_hook(event, _callback(event))
"#;

fn hermes_plugin_files() -> [(&'static str, String); 2] {
    let hook = serde_json::json!([hook_program(), "+hook", "hermes"]).to_string();
    let events: Vec<&str> = crate::flight_log::hook_events(Agent::Hermes);
    let py = HERMES_PLUGIN_PY
        .replace("__HOOK__", &hook)
        .replace("__EVENTS__", &serde_json::json!(events).to_string());
    let mut manifest = format!(
        "name: {HERMES_PLUGIN}\nversion: \"{}\"\ndescription: Cyberterm's flight log -- what Hermes does in a Cyberterm agent session\nprovides_hooks:\n",
        env!("CARGO_PKG_VERSION")
    );
    for event in &events {
        manifest.push_str(&format!("  - {event}\n"));
    }
    [("plugin.yaml", manifest), ("__init__.py", py)]
}

/// Whether `plugins.enabled` in Hermes's config.yaml lists the plugin
/// (read, never written: Hermes's own commands change it).
fn hermes_enabled(config: &str) -> bool {
    let mut in_plugins = false;
    let mut in_enabled = false;
    for line in config.lines() {
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if indent == 0 {
            in_plugins = t == "plugins:";
            in_enabled = false;
            continue;
        }
        if in_plugins && !t.starts_with('-') {
            in_enabled = t == "enabled:";
            if let Some(list) = t.strip_prefix("enabled:") {
                // A flow list: `enabled: [a, b]`.
                let list = list.trim().trim_start_matches('[').trim_end_matches(']');
                if list
                    .split(',')
                    .any(|i| i.trim().trim_matches(['"', '\'']) == HERMES_PLUGIN)
                {
                    return true;
                }
            }
            continue;
        }
        if in_enabled {
            if let Some(item) = t.strip_prefix('-') {
                if item.trim().trim_matches(['"', '\'']) == HERMES_PLUGIN {
                    return true;
                }
            }
        }
    }
    false
}

fn hermes_installed(home: &Path) -> bool {
    let plugin = home.join("plugins").join(HERMES_PLUGIN).join("__init__.py");
    std::fs::read_to_string(plugin).is_ok_and(|t| t.contains("CYBERTERM_INTEGRATION"))
        && std::fs::read_to_string(home.join("config.yaml")).is_ok_and(|c| hermes_enabled(&c))
}

/// Runs `hermes plugins <args>`; its output on failure.
fn hermes_cli(args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new("hermes")
        .arg("plugins")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run hermes: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let text = if text.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            text
        };
        Err(format!("hermes plugins {}: {text}", args.join(" ")))
    }
}

fn hermes_setup(home: &Path, remove: bool) -> Result<String, String> {
    let dir = home.join("plugins").join(HERMES_PLUGIN);
    let shown = crate::agent::short(&dir);
    if remove {
        if !dir.exists() {
            return Ok(format!("nothing to remove ({shown} doesn't exist)"));
        }
        let ours = std::fs::read_to_string(dir.join("__init__.py"))
            .is_ok_and(|t| t.contains("CYBERTERM_INTEGRATION"));
        if !ours {
            return Err(format!(
                "{shown} isn't Cyberterm's plugin, so it was left alone"
            ));
        }
        // Disable first, so config.yaml doesn't keep a stale entry.
        let disabled = hermes_cli(&["disable", HERMES_PLUGIN]);
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{shown}: {e}"))?;
        return Ok(match disabled {
            Ok(()) => format!("disabled the plugin and removed {shown}"),
            Err(e) => format!("removed {shown} ({e})"),
        });
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{shown}: {e}"))?;
    for (name, body) in hermes_plugin_files() {
        let path = dir.join(name);
        std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    hermes_cli(&["enable", HERMES_PLUGIN, "--no-allow-tool-override"]).map_err(|e| {
        format!("added the plugin in {shown}, but enabling it failed:\n   {e}\n   Try: hermes plugins enable {HERMES_PLUGIN}")
    })?;
    Ok(format!("added the plugin in {shown} and enabled it"))
}

// ----------------------------------------------------------------------
// What each agent reports
// ----------------------------------------------------------------------

/// How much the flight log gets from an agent.
#[derive(Clone, Debug, PartialEq)]
pub struct Coverage {
    /// Its own hooks report prompts, output, edits and waits; otherwise
    /// only the commands the shell recorder sees.
    pub full: bool,
    /// How: "hooks · per session", "commands".
    pub note: &'static str,
    /// The command that would give it more, when there is one.
    pub setup: Option<&'static str>,
}

/// Coverage for an agent by its adapter (`crate::agent::adapter`).
pub fn coverage_of(adapter: Option<&str>) -> Coverage {
    let set_up = |done: bool, setup: &'static str| match done {
        true => (true, "hooks · set up", None),
        false => (false, "commands", Some(setup)),
    };
    let (full, note, setup) = match adapter {
        Some("claude") | Some("copilot") => (true, "hooks · per session", None),
        Some("codex") => (true, "hooks · approve once", None),
        Some("gemini") => set_up(
            installed(&gemini_settings_path(), Agent::Gemini) == Some(true),
            "+agent setup gemini",
        ),
        Some("cursor") => set_up(
            installed(&cursor_hooks_path(), Agent::Cursor) == Some(true),
            "+agent setup cursor",
        ),
        Some("hermes") => set_up(hermes_installed(&hermes_home()), "+agent setup hermes"),
        _ => (false, "commands", None),
    };
    Coverage { full, note, setup }
}

/// What the flight log gets from an agent, for listings.
pub fn coverage(adapter: Option<&str>) -> String {
    let c = coverage_of(adapter);
    match (c.full, c.setup) {
        (true, _) if adapter == Some("codex") => {
            "full: hooks, per session (approve them once in Codex)".into()
        }
        (true, _) if c.note == "hooks · set up" => "full: hooks (set up)".into(),
        (true, _) => "full: hooks, per session".into(),
        (false, Some(setup)) => format!("commands only -- `cyberterm {setup}` for more"),
        (false, None) => "commands (shell recorder)".into(),
    }
}

const HELP: &str = "\
cyberterm +agent setup: how much each agent tells the flight log

  cyberterm +agent setup                 each agent and what it reports
  cyberterm +agent setup gemini          add Cyberterm's hooks to ~/.gemini/settings.json
  cyberterm +agent setup cursor          add them to ~/.cursor/hooks.json
  cyberterm +agent setup hermes          add a Hermes plugin and enable it
  cyberterm +agent setup <agent> --remove   take them out again

Claude Code, Codex and Copilot need nothing: their hooks are added per
session. Hooks set up here only act inside Cyberterm's agent sessions;
elsewhere they return at once. Settings files are backed up before they
change.
";

fn report(label: &str, agent: &str, result: Result<String, String>, remove: bool) -> i32 {
    match result {
        Ok(note) => {
            println!("✓ {label}: {note}");
            if !remove {
                println!(
                    "  It only acts in Cyberterm agent sessions. Undo: cyberterm +agent setup {agent} --remove"
                );
            }
            0
        }
        Err(e) => {
            eprintln!("❌ {e}");
            1
        }
    }
}

pub fn run(args: &[String], cfg: &crate::config::AgentsConfig) -> i32 {
    let remove = args.iter().any(|a| a == "--remove");
    let target = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str);
    match target {
        None => {
            println!("What each agent tells the flight log:");
            for l in crate::agent::launchers(cfg) {
                println!(
                    "  {:<14} {}",
                    l.name,
                    coverage(crate::agent::adapter(&l.argv))
                );
            }
            println!("\nEvery agent's commands are recorded through bash and zsh as well.");
            0
        }
        Some("help" | "--help" | "-h") => {
            print!("{HELP}");
            0
        }
        Some("gemini") => report(
            "Gemini CLI",
            "gemini",
            update(
                &gemini_settings_path(),
                Agent::Gemini,
                &hook_command(Agent::Gemini),
                remove,
            ),
            remove,
        ),
        Some("cursor" | "cursor-agent") => report(
            "Cursor",
            "cursor",
            update(
                &cursor_hooks_path(),
                Agent::Cursor,
                &hook_command(Agent::Cursor),
                remove,
            ),
            remove,
        ),
        Some("hermes") => report(
            "Hermes",
            "hermes",
            hermes_setup(&hermes_home(), remove),
            remove,
        ),
        Some(name @ ("claude" | "codex" | "copilot")) => {
            println!("{name} needs no setup: Cyberterm adds its hooks to each session it starts.");
            0
        }
        Some(other) => {
            eprintln!(
                "❌ No hook setup for {other:?} yet; its commands are recorded through bash and zsh."
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyberterm-setup-{name}-{}-{}",
            std::process::id(),
            crate::shell::tap::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn setup_adds_beside_existing_hooks_and_removes_only_its_own() {
        let dir = scratch("beside");
        let path = dir.join("settings.json");
        // Someone else's hook (herdr's, here) and other settings.
        let before = json!({
            "theme": "Dracula",
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "bash herdr-agent-state.sh session"}]}]
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&before).unwrap()).unwrap();

        update(&path, Agent::Gemini, "cyberterm +hook gemini", false).unwrap();
        // Twice: no duplicates.
        update(&path, Agent::Gemini, "cyberterm +hook gemini", false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "Dracula");
        let starts = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(starts.len(), 2, "{v:#}");
        assert_eq!(
            starts[0]["hooks"][0]["command"],
            "bash herdr-agent-state.sh session"
        );
        assert_eq!(v["hooks"]["BeforeTool"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["BeforeTool"][0]["matcher"], "*");
        assert_eq!(v["hooks"]["AfterTool"][0]["hooks"][0]["timeout"], 10_000);
        assert!(dir.join("settings.json.cyberterm-backup").exists());
        assert_eq!(installed(&path, Agent::Gemini), Some(true));

        update(&path, Agent::Gemini, "cyberterm +hook gemini", true).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v, before);
        assert_eq!(installed(&path, Agent::Gemini), Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_hooks_sit_beside_herdrs_in_cursors_flat_format() {
        let dir = scratch("cursor");
        let path = dir.join("hooks.json");
        // What herdr installs on this machine.
        let before = json!({
            "hooks": {
                "sessionStart": [{"command": "bash '/home/u/.cursor/herdr-agent-state.sh' session"}]
            },
            "version": 1
        });
        std::fs::write(&path, serde_json::to_string_pretty(&before).unwrap()).unwrap();
        update(&path, Agent::Cursor, "cyberterm +hook cursor", false).unwrap();
        update(&path, Agent::Cursor, "cyberterm +hook cursor", false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        let starts = v["hooks"]["sessionStart"].as_array().unwrap();
        assert_eq!(starts.len(), 2, "{v:#}");
        assert_eq!(starts[1]["command"], "cyberterm +hook cursor");
        assert_eq!(
            v["hooks"]["afterFileEdit"][0]["command"],
            "cyberterm +hook cursor"
        );
        assert!(v["hooks"]["afterFileEdit"][0].get("hooks").is_none());
        assert_eq!(installed(&path, Agent::Cursor), Some(true));
        // Gemini's setup doesn't see Cursor's entries as its own.
        assert_eq!(installed(&path, Agent::Gemini), Some(false));

        update(&path, Agent::Cursor, "cyberterm +hook cursor", true).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v, before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_creates_a_missing_file_and_refuses_one_it_cant_parse() {
        let dir = scratch("refuse");
        let path = dir.join("new").join("settings.json");
        update(&path, Agent::Gemini, "cyberterm +hook gemini", false).unwrap();
        assert_eq!(installed(&path, Agent::Gemini), Some(true));

        let commented = dir.join("commented.json");
        let text = "{\n  // my settings\n  \"theme\": \"x\"\n}\n";
        std::fs::write(&commented, text).unwrap();
        let err = update(&commented, Agent::Gemini, "cyberterm +hook gemini", false).unwrap_err();
        assert!(err.contains("isn't plain JSON"), "{err}");
        assert!(err.contains("+hook gemini"), "{err}");
        assert_eq!(std::fs::read_to_string(&commented).unwrap(), text);
        assert!(update(&dir.join("absent.json"), Agent::Gemini, "x", true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hermes_enabled_reads_both_list_styles() {
        let block = "model: x\nplugins:\n  enabled:\n    - herdr-agent-state\n    - cyberterm\n  disabled: []\nother: 1\n";
        assert!(hermes_enabled(block));
        assert!(hermes_enabled(
            "plugins:\n  enabled: [herdr, \"cyberterm\"]\n"
        ));
        assert!(!hermes_enabled(
            "plugins:\n  enabled:\n    - herdr-agent-state\n"
        ));
        // Only under plugins.enabled.
        assert!(!hermes_enabled("plugins:\n  disabled:\n    - cyberterm\n"));
        assert!(!hermes_enabled("skills:\n  enabled:\n    - cyberterm\n"));
    }

    #[test]
    fn the_hermes_plugin_is_inert_outside_sessions_and_never_directs() {
        let [(manifest_name, manifest), (py_name, py)] = hermes_plugin_files();
        assert_eq!(manifest_name, "plugin.yaml");
        assert!(manifest.starts_with("name: cyberterm\n"), "{manifest}");
        assert_eq!(py_name, "__init__.py");
        assert!(!py.contains("__HOOK__") && !py.contains("__EVENTS__"));
        assert!(py.contains("\"+hook\",\"hermes\"]"), "{py}");
        assert!(py.contains("\"pre_tool_call\""), "{py}");
        assert!(py.contains("if not os.environ.get(\"CYBERTERM_AGENT_ID\"):"));
        assert!(py.contains("CYBERTERM_INTEGRATION"));
    }
}
