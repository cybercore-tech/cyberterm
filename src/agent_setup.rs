// src/agent_setup.rs
//
// `cyberterm +agent setup [agent] [--remove]`: how much each agent tells
// the flight log, and the opt-in step for agents that can only take hooks
// from their own user settings.
//
// Claude Code and Codex get their hooks per session (src/agent.rs) and
// need nothing here. Gemini CLI has no per-session way in, so setup adds
// one hook entry per event to ~/.gemini/settings.json -- next to any you
// already have -- running `cyberterm +hook gemini`, which does nothing
// outside Cyberterm's agent sessions. `--remove` takes exactly those
// entries out again. The file is backed up first and written atomically,
// and a file that isn't plain JSON is left alone with instructions.

use crate::flight_log::{hooks_json, Agent};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// What marks a hook entry as Cyberterm's.
fn is_ours(command: &str, agent: Agent) -> bool {
    command.contains(&format!("+hook {}", agent.name()))
}

fn gemini_settings_path() -> PathBuf {
    let home = std::env::var_os("GEMINI_CLI_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".gemini").join("settings.json")
}

/// The hook command to install: `cyberterm +hook <agent>` when the
/// `cyberterm` on PATH is this one, so reinstalling Cyberterm elsewhere
/// doesn't break it; this binary's full path otherwise.
fn hook_command(agent: Agent) -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("cyberterm"))
            .find(|p| p.is_file())
            .and_then(|p| p.canonicalize().ok())
    });
    let program = match (&exe, &on_path) {
        (Some(e), Some(p)) if e == p => "cyberterm".to_string(),
        (Some(e), _) => crate::agent::shell_quote(&e.to_string_lossy()),
        _ => "cyberterm".to_string(),
    };
    format!("{program} +hook {}", agent.name())
}

/// Removes Cyberterm's hook groups from a settings object's `hooks`;
/// returns how many were removed. Event lists left empty go too.
fn strip_ours(settings: &mut Value, agent: Agent) -> usize {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    for groups in hooks.values_mut() {
        let Some(list) = groups.as_array_mut() else {
            continue;
        };
        let before = list.len();
        list.retain(|group| {
            !group
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|hs| {
                    hs.iter().any(|h| {
                        h.get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|c| is_ours(c, agent))
                    })
                })
        });
        removed += before - list.len();
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|l| !l.is_empty()));
    removed
}

/// Adds Cyberterm's hooks (replacing older copies of them).
fn add_ours(settings: &mut Value, agent: Agent, command: &str) {
    strip_ours(settings, agent);
    if !settings.is_object() {
        *settings = Value::Object(Default::default());
    }
    let obj = settings.as_object_mut().expect("an object");
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()));
    if !hooks.is_object() {
        *hooks = Value::Object(Default::default());
    }
    let hooks = hooks.as_object_mut().expect("an object");
    let ours = hooks_json(agent, command);
    for (event, groups) in ours.as_object().expect("an object") {
        let list = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let (Some(list), Some(groups)) = (list.as_array_mut(), groups.as_array()) {
            list.extend(groups.iter().cloned());
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

/// What the flight log gets from an agent, for listings.
pub fn coverage(name: &str) -> String {
    match name {
        "claude" => "full: hooks, per session".into(),
        "codex" => "full: hooks, per session (approve them once in Codex)".into(),
        "gemini" => match installed(&gemini_settings_path(), Agent::Gemini) {
            Some(true) => "full: hooks (set up)".into(),
            _ => "commands only -- `cyberterm +agent setup gemini` for more".into(),
        },
        _ => "commands (shell recorder)".into(),
    }
}

const HELP: &str = "\
cyberterm +agent setup: how much each agent tells the flight log

  cyberterm +agent setup                 each agent and what it reports
  cyberterm +agent setup gemini          add Cyberterm's hooks to ~/.gemini/settings.json
  cyberterm +agent setup gemini --remove take them out again

Claude Code and Codex need nothing: their hooks are added per session.
Hooks set up here only act inside Cyberterm's agent sessions; elsewhere
they return at once. The settings file is backed up before it changes.
";

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
                println!("  {:<14} {}", l.name, coverage(&l.name));
            }
            println!("\nEvery agent's commands are recorded through bash and zsh as well.");
            0
        }
        Some("help" | "--help" | "-h") => {
            print!("{HELP}");
            0
        }
        Some("gemini") => {
            let path = gemini_settings_path();
            match update(&path, Agent::Gemini, &hook_command(Agent::Gemini), remove) {
                Ok(note) => {
                    println!("✓ Gemini CLI: {note}");
                    if !remove {
                        println!("  They only act in Cyberterm agent sessions. Undo: cyberterm +agent setup gemini --remove");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("❌ {e}");
                    1
                }
            }
        }
        Some(name @ ("claude" | "codex")) => {
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
}
