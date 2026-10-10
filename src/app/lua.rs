// src/app/lua.rs
//
// Lua scripting: ~/.config/cyberterm/init.lua (or [lua] script), run in an
// embedded Lua 5.4 on the GUI thread. Scripts react to events
// (`cyberterm.on`), bind keys (`cyberterm.bind`), define commands
// (`cyberterm.command`, run with `+ctl lua name=...`), set timers
// (`cyberterm.after`), and act through every control-socket method
// (`cyberterm.call` and the wrappers in lua_prelude.lua).
//
// Scripts only touch the window through `__call`, a function that exists
// only while Rust runs a script (`lua_run`): the Lua state is taken out
// of the App for the duration, so the call can borrow the App mutably.
// Events raised meanwhile (a script splitting a pane raises
// pane_created) are queued and delivered afterwards, not re-entered.
// Every run gets a time budget; a script still running after it is
// stopped with an error instead of freezing the window. The file is
// reloaded when it changes (and with the config).

use std::cell::Cell;
use std::rc::Rc;

use mlua::{
    Function, HookTriggers, Lua, LuaSerdeExt, SerializeOptions, Table, Value as LuaValue, VmState,
};
use serde_json::{json, Value};

use super::*;
use crate::control::Request;
use crate::input::bindings::Combo;

/// The longest one script run may take.
const BUDGET: Duration = Duration::from_secs(2);
/// How long a status message stays up.
const STATUS_FOR: Duration = Duration::from_secs(6);

const PRELUDE: &str = include_str!("lua_prelude.lua");

struct Host {
    lua: Lua,
    deadline: Rc<Cell<Instant>>,
    /// Parsed `cyberterm.bind` combos, with their 1-based index.
    bindings: Vec<(Combo, usize)>,
    /// When the next `after` timer is due (ms since the epoch).
    next_timer: Option<u64>,
}

#[derive(Default)]
pub(super) struct LuaState {
    host: Option<Host>,
    /// A script is running (its host is taken out).
    busy: bool,
    pending: Vec<(String, Value)>,
    /// (text, since, is_error) shown along the bottom of the focused pane.
    status: Option<(String, Instant, bool)>,
    started: bool,
    script: Option<(PathBuf, Option<SystemTime>)>,
}

fn log_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("cyberterm")
        .join("lua.log")
}

fn log_line(text: &str) {
    use std::io::Write as _;
    eprintln!("cyberterm lua: {text}");
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "{} {text}",
            crate::history::format_time(crate::shell::tap::now_ms())
        );
    }
}

fn to_lua(lua: &Lua, v: &Value) -> mlua::Result<LuaValue> {
    // JSON null becomes nil, not a sentinel, so `if x then` works.
    lua.to_value_with(
        v,
        SerializeOptions::new()
            .serialize_none_to_null(false)
            .serialize_unit_to_null(false),
    )
}

fn from_lua(lua: &Lua, v: LuaValue) -> mlua::Result<Value> {
    let value: Value = lua.from_value(v)?;
    // An empty Lua table reads as an empty array; methods want objects.
    Ok(match value {
        Value::Array(a) if a.is_empty() => json!({}),
        Value::Null => json!({}),
        other => other,
    })
}

impl App {
    fn lua_script_path(&self) -> PathBuf {
        let s = self.config.lua.script.trim();
        if s.is_empty() {
            return self.config_root.join("init.lua");
        }
        match s.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(rest))
                .unwrap_or_else(|| PathBuf::from(s)),
            None => PathBuf::from(s),
        }
    }

    /// (Re)loads the script: a fresh Lua state, the prelude, then the file.
    pub(super) fn lua_load(&mut self) {
        self.lua.host = None;
        self.lua.pending.clear();
        let path = self.lua_script_path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        self.lua.script = Some((path.clone(), mtime));
        if !self.config.lua.enabled || mtime.is_none() {
            return;
        }
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => return self.lua_report(&format!("{}: {e}", path.display())),
        };
        let host = match new_host() {
            Ok(h) => h,
            Err(e) => return self.lua_report(&format!("couldn't start Lua: {e}")),
        };
        self.lua.host = Some(host);
        let name = path.display().to_string();
        self.lua_run(|lua| lua.load(&source).set_name(name).exec());
        self.lua_collect_bindings();
        self.lua_flush();
    }

    /// Reloads the script when its file changed (called once a second).
    pub(super) fn lua_poll(&mut self) {
        let path = self.lua_script_path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let known = self.lua.script.as_ref();
        if known.is_none_or(|(p, m)| *p != path || *m != mtime) {
            let had = self.lua.host.is_some() || known.is_some_and(|(_, m)| m.is_some());
            let before = Instant::now();
            self.lua_load();
            // Keep an error from the reload on screen instead.
            let failed = self
                .lua
                .status
                .as_ref()
                .is_some_and(|(_, since, error)| *error && *since >= before);
            if (had || self.lua.host.is_some()) && !failed {
                self.lua_set_status("Lua script reloaded", false);
            }
        }
    }

    /// Runs `f` with `__call` available. None if there's no script, a
    /// script is already running, or it failed (the error is reported).
    fn lua_run<R>(&mut self, f: impl FnOnce(&Lua) -> mlua::Result<R>) -> Option<R> {
        if self.lua.busy {
            return None;
        }
        let host = self.lua.host.take()?;
        self.lua.busy = true;
        host.deadline.set(Instant::now() + BUDGET);
        let result = host.lua.scope(|scope| {
            let call = scope.create_function_mut(|lua, (method, params): (String, LuaValue)| {
                let params = from_lua(lua, params)?;
                match self.lua_dispatch(&method, params) {
                    Ok(v) => to_lua(lua, &v),
                    Err(e) => Err(mlua::Error::runtime(e)),
                }
            })?;
            host.lua.globals().set("__call", call)?;
            let r = f(&host.lua);
            host.lua.globals().set("__call", LuaValue::Nil)?;
            r
        });
        let mut host = host;
        host.next_timer = host
            .lua
            .globals()
            .get::<Table>("cyberterm")
            .and_then(|ct| ct.get::<Function>("_next_timer"))
            .and_then(|f| f.call::<Option<u64>>(()))
            .ok()
            .flatten();
        self.lua.host = Some(host);
        self.lua.busy = false;
        match result {
            Ok(r) => Some(r),
            Err(e) => {
                self.lua_report(&e.to_string());
                None
            }
        }
    }

    /// What `__call` does: a few Lua-only methods, else any control method.
    fn lua_dispatch(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "lua_copy" => {
                let text = params["text"].as_str().unwrap_or_default().to_string();
                self.copy_text(&text);
                Ok(Value::Null)
            }
            "lua_status" => {
                let text = params["text"].as_str().unwrap_or_default().to_string();
                self.lua_set_status(&text, false);
                Ok(Value::Null)
            }
            "lua" => Err("Lua commands can't be run from Lua".into()),
            _ => self
                .on_control(Request {
                    id: Value::Null,
                    method: method.to_string(),
                    params,
                })
                .map_err(|e| e.message),
        }
    }

    /// Sends an event to the script (queued if one is running).
    pub(super) fn lua_emit(&mut self, event: &str, payload: Value) {
        if self.lua.busy {
            if self.lua.pending.len() < 256 {
                self.lua.pending.push((event.to_string(), payload));
            }
            return;
        }
        if self.lua.host.is_none() {
            return;
        }
        self.lua_emit_now(event, payload);
        self.lua_flush();
    }

    fn lua_emit_now(&mut self, event: &str, payload: Value) {
        let event = event.to_string();
        self.lua_run(|lua| {
            let ct: Table = lua.globals().get("cyberterm")?;
            let emit: Function = ct.get("_emit")?;
            emit.call::<()>((event, to_lua(lua, &payload)?))
        });
    }

    /// Delivers events queued while a script ran (a few rounds at most, so
    /// a script reacting to its own events can't loop forever).
    fn lua_flush(&mut self) {
        for _ in 0..4 {
            let pending = std::mem::take(&mut self.lua.pending);
            if pending.is_empty() {
                return;
            }
            for (event, payload) in pending {
                self.lua_emit_now(&event, payload);
            }
        }
        self.lua.pending.clear();
    }

    fn lua_collect_bindings(&mut self) {
        let Some(host) = &self.lua.host else { return };
        let combos: Vec<String> = host
            .lua
            .globals()
            .get::<Table>("cyberterm")
            .and_then(|ct| ct.get::<Table>("_bindings"))
            .map(|t| {
                t.sequence_values::<Table>()
                    .filter_map(|b| b.ok()?.get::<String>("combo").ok())
                    .collect()
            })
            .unwrap_or_default();
        let mut bindings = Vec::new();
        let mut errors = Vec::new();
        for (i, combo) in combos.iter().enumerate() {
            match Combo::parse(combo) {
                Ok(c) => bindings.push((c, i + 1)),
                Err(e) => errors.push(format!("bind(\"{combo}\"): {e}")),
            }
        }
        if let Some(host) = &mut self.lua.host {
            host.bindings = bindings;
        }
        for e in errors {
            self.lua_report(&e);
        }
    }

    /// The script's binding for a key, if it has one.
    pub(super) fn lua_binding(
        &self,
        logical: &Key,
        base: &Key,
        mods: ModifiersState,
    ) -> Option<usize> {
        let host = self.lua.host.as_ref()?;
        host.bindings
            .iter()
            .find(|(c, _)| c.matches(logical, base, mods))
            .map(|(_, i)| *i)
    }

    pub(super) fn lua_run_binding(&mut self, index: usize) {
        self.lua_run(|lua| {
            let ct: Table = lua.globals().get("cyberterm")?;
            ct.get::<Function>("_run_binding")?.call::<()>(index)
        });
        self.lua_flush();
        self.request_redraw();
    }

    /// Names of the script's `cyberterm.command`s, sorted.
    pub(super) fn lua_command_names(&self) -> Vec<String> {
        let Some(host) = &self.lua.host else {
            return Vec::new();
        };
        let mut names: Vec<String> = host
            .lua
            .globals()
            .get::<Table>("cyberterm")
            .and_then(|ct| ct.get::<Table>("_commands"))
            .map(|t| {
                t.pairs::<String, LuaValue>()
                    .filter_map(|kv| kv.ok().map(|(k, _)| k))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// `+ctl lua name=... args=...`.
    pub(super) fn lua_command(&mut self, name: &str, args: Value) -> Result<Value, String> {
        if self.lua.host.is_none() {
            return Err("no Lua script loaded (put one in ~/.config/cyberterm/init.lua)".into());
        }
        let name = name.to_string();
        let result = self.lua_run(|lua| {
            let ct: Table = lua.globals().get("cyberterm")?;
            let out: LuaValue = ct
                .get::<Function>("_run_command")?
                .call((name, to_lua(lua, &args)?))?;
            let v: Value = lua.from_value(out)?;
            Ok(v)
        });
        self.lua_flush();
        self.request_redraw();
        result.ok_or_else(|| {
            self.lua
                .status
                .as_ref()
                .map(|(t, ..)| t.clone())
                .unwrap_or_else(|| "the command failed".into())
        })
    }

    /// Timers and the startup event (from `about_to_wait`); returns when
    /// the next timer is due.
    pub(super) fn lua_tick(&mut self) -> Option<Instant> {
        if !self.lua.started && !self.panes.is_empty() {
            self.lua.started = true;
            self.lua_emit("startup", json!({}));
        }
        let due = self.lua.host.as_ref()?.next_timer?;
        let now = crate::shell::tap::now_ms();
        if due <= now {
            self.lua_run(|lua| {
                let ct: Table = lua.globals().get("cyberterm")?;
                ct.get::<Function>("_fire_timers")?.call::<()>(now)
            });
            self.lua_flush();
            self.request_redraw();
        }
        let next = self.lua.host.as_ref()?.next_timer?;
        let wait = next.saturating_sub(crate::shell::tap::now_ms());
        Some(Instant::now() + Duration::from_millis(wait))
    }

    fn lua_report(&mut self, error: &str) {
        log_line(error);
        let first = error.lines().next().unwrap_or(error).to_string();
        self.lua_set_status(&format!("Lua: {first}"), true);
    }

    pub(super) fn lua_set_status(&mut self, text: &str, error: bool) {
        self.lua.status = Some((text.to_string(), Instant::now(), error));
        self.request_redraw();
    }

    /// The status / error bar on the focused pane's bottom row.
    pub(super) fn draw_lua_status(&self, frame: &mut Frame) {
        let Some((text, since, error)) = &self.lua.status else {
            return;
        };
        if since.elapsed() > STATUS_FOR || frame.rows == 0 {
            return;
        }
        let bg = frame::hex_to_rgb(self.palette.bg);
        let color = frame::hex_to_rgb(if *error {
            self.palette.ansi[1]
        } else {
            self.palette.ansi[4]
        });
        let row = frame.rows - 1;
        frame.fill(row, 0, bg, color);
        let shown: String = text.chars().take(frame.cols.saturating_sub(2)).collect();
        frame.put(row, 1, &shown, bg, color);
    }

    pub(super) fn lua_status_shown(&self) -> bool {
        self.lua.status.is_some()
    }

    pub(super) fn lua_clear_status(&mut self) {
        self.lua.status = None;
        self.request_redraw();
    }

    /// Whether the status bar is still up (to schedule its removal).
    pub(super) fn lua_status_until(&self) -> Option<Instant> {
        self.lua
            .status
            .as_ref()
            .map(|(_, since, _)| *since + STATUS_FOR)
            .filter(|t| *t > Instant::now())
    }
}

fn new_host() -> mlua::Result<Host> {
    let lua = Lua::new();
    let deadline = Rc::new(Cell::new(Instant::now() + BUDGET));
    let d = deadline.clone();
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(10_000),
        move |_lua, _debug| {
            if Instant::now() > d.get() {
                Err(mlua::Error::runtime(format!(
                    "script ran longer than {}s and was stopped",
                    BUDGET.as_secs()
                )))
            } else {
                Ok(VmState::Continue)
            }
        },
    );
    let g = lua.globals();
    g.set(
        "__now",
        lua.create_function(|_, ()| Ok(crate::shell::tap::now_ms()))?,
    )?;
    g.set(
        "__log",
        lua.create_function(|_, text: String| {
            log_line(&text);
            Ok(())
        })?,
    )?;
    g.set(
        "__notify",
        lua.create_function(|_, (title, body): (String, String)| {
            let _ = std::process::Command::new("notify-send")
                .args(["--app-name=Cyberterm", &title, &body])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            Ok(())
        })?,
    )?;
    g.set("__call", LuaValue::Nil)?;
    lua.load(PRELUDE).set_name("cyberterm prelude").exec()?;
    Ok(Host {
        lua,
        deadline,
        bindings: Vec::new(),
        next_timer: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host whose `__call` records calls and answers `blocks`.
    fn host_with_fake_call() -> (Host, Rc<std::cell::RefCell<Vec<String>>>) {
        let host = new_host().unwrap();
        let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
        let c = calls.clone();
        let call = host
            .lua
            .create_function(move |lua, (method, params): (String, LuaValue)| {
                let params = from_lua(lua, params)?;
                c.borrow_mut().push(format!("{method} {params}"));
                let reply = match method.as_str() {
                    "blocks" => json!([{ "output": "boom\n" }]),
                    "split" => json!({ "pane": 7 }),
                    _ => Value::Null,
                };
                to_lua(lua, &reply)
            })
            .unwrap();
        host.lua.globals().set("__call", call).unwrap();
        (host, calls)
    }

    fn run(host: &Host, code: &str) -> mlua::Result<()> {
        host.lua.load(code).exec()
    }

    #[test]
    fn events_reach_handlers_and_output_is_fetched_on_demand() {
        let (host, calls) = host_with_fake_call();
        run(
            &host,
            r#"
            seen = {}
            cyberterm.on("command_finished", function(ev)
              if ev.exit ~= 0 then
                seen.cmd = ev.command
                seen.out = ev.output()
                seen.pane = cyberterm.split{ direction = "down", command = "less log" }
              end
            end)
            "#,
        )
        .unwrap();
        let ct: Table = host.lua.globals().get("cyberterm").unwrap();
        let ev = to_lua(
            &host.lua,
            &json!({ "pane": 3, "command": "make", "exit": 2, "cwd": null }),
        )
        .unwrap();
        ct.get::<Function>("_emit")
            .unwrap()
            .call::<()>(("command_finished", ev))
            .unwrap();
        let seen: Table = host.lua.globals().get("seen").unwrap();
        assert_eq!(seen.get::<String>("cmd").unwrap(), "make");
        assert_eq!(seen.get::<String>("out").unwrap(), "boom\n");
        assert_eq!(seen.get::<i64>("pane").unwrap(), 7);
        let calls = calls.borrow();
        assert!(calls[0].starts_with("blocks ") && calls[0].contains("\"pane\":3"));
        assert!(calls[1].contains("\"direction\":\"down\""));
    }

    #[test]
    fn bindings_commands_and_timers_are_recorded() {
        let (host, _) = host_with_fake_call();
        run(
            &host,
            r#"
            n = 0
            cyberterm.bind("ctrl+alt+g", function() n = n + 1 end)
            cyberterm.command("greet", function(args) return "hi " .. args[1] end)
            cyberterm.after(0, function() n = n + 10 end)
            "#,
        )
        .unwrap();
        let ct: Table = host.lua.globals().get("cyberterm").unwrap();
        ct.get::<Function>("_run_binding")
            .unwrap()
            .call::<()>(1)
            .unwrap();
        let out: String = ct
            .get::<Function>("_run_command")
            .unwrap()
            .call(("greet", to_lua(&host.lua, &json!(["raven"])).unwrap()))
            .unwrap();
        assert_eq!(out, "hi raven");
        let due: Option<u64> = ct.get::<Function>("_next_timer").unwrap().call(()).unwrap();
        ct.get::<Function>("_fire_timers")
            .unwrap()
            .call::<()>(due.unwrap())
            .unwrap();
        assert_eq!(host.lua.globals().get::<i64>("n").unwrap(), 11);
        assert!(ct
            .get::<Function>("_next_timer")
            .unwrap()
            .call::<Option<u64>>(())
            .unwrap()
            .is_none());
        // Unknown events are caught at registration.
        assert!(run(&host, r#"cyberterm.on("nope", function() end)"#).is_err());
    }

    #[test]
    fn a_runaway_script_is_stopped() {
        let host = new_host().unwrap();
        host.deadline
            .set(Instant::now() + Duration::from_millis(200));
        let started = Instant::now();
        let err = run(&host, "while true do end").unwrap_err();
        assert!(err.to_string().contains("was stopped"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn json_and_lua_values_convert_both_ways() {
        let lua = Lua::new();
        let v = to_lua(&lua, &json!({ "a": null, "b": [1, 2] })).unwrap();
        let t = v.as_table().unwrap();
        assert!(t.get::<LuaValue>("a").unwrap().is_nil());
        assert_eq!(
            from_lua(&lua, LuaValue::Table(lua.create_table().unwrap())).unwrap(),
            json!({})
        );
        assert_eq!(from_lua(&lua, v).unwrap()["b"], json!([1, 2]));
    }
}
