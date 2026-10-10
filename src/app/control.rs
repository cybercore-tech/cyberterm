// src/app/control.rs
//
// Control-socket methods, run on the GUI thread. Pane and tab ids are the
// stable numbers `list_panes` / `list_tabs` report (not positions), so a
// script can hold on to them while the layout changes around it.

use serde_json::{json, Value};

use super::*;
use crate::control::{Request, RpcError};
use crate::layout::Direction;

type Outcome = Result<Value, RpcError>;

fn opt_u32(params: &Value, key: &str) -> Result<Option<u32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number"))),
    }
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a string"))),
    }
}

fn direction(params: &Value, default: Direction) -> Result<Direction, RpcError> {
    Ok(match opt_str(params, "direction")? {
        None => default,
        Some("right") => Direction::Right,
        Some("down") => Direction::Down,
        Some("left") => Direction::Left,
        Some("up") => Direction::Up,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "direction must be right, down, left or up, not `{other}`"
            )))
        }
    })
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

impl App {
    pub(super) fn on_control(&mut self, request: Request) -> Outcome {
        let p = &request.params;
        match request.method.replace('-', "_").as_str() {
            "ping" => {
                Ok(json!({ "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id() }))
            }
            "list_tabs" => Ok(self.rpc_list_tabs()),
            "list_panes" => Ok(self.rpc_list_panes()),
            "get_text" => self.rpc_get_text(p),
            "send_text" => self.rpc_send_text(p),
            "split" => self.rpc_split(p),
            "blocks" => self.rpc_blocks(p),
            "agent_run" => self.rpc_agent_run(p),
            "history" => {
                let path = crate::history::default_path();
                let store = crate::history::Store::open(&path)
                    .map_err(|e| RpcError::failed(format!("history: {e}")))?;
                let query = crate::history::Query {
                    text: opt_str(p, "query")?.map(str::to_string),
                    failed_only: p.get("failed").and_then(Value::as_bool).unwrap_or(false),
                    cwd: opt_str(p, "cwd")?.map(str::to_string),
                    limit: opt_u32(p, "limit")?.unwrap_or(20).min(500) as usize,
                    ..Default::default()
                };
                let with_output = p.get("output").and_then(Value::as_bool).unwrap_or(false);
                let entries = store
                    .search(&query)
                    .map_err(|e| RpcError::failed(format!("history: {e}")))?;
                Ok(Value::Array(
                    entries
                        .into_iter()
                        .map(|e| {
                            let output = with_output.then(|| store.output(e.id).ok().flatten());
                            json!({
                                "id": e.id,
                                "command": e.command,
                                "cwd": e.cwd,
                                "exit": e.exit,
                                "started_ms": e.started_ms,
                                "finished_ms": e.finished_ms,
                                "host": e.host,
                                "session": e.session,
                                "snippet": e.snippet,
                                "output": output.flatten(),
                            })
                        })
                        .collect(),
                ))
            }
            "load_layout" => {
                let path = match opt_str(p, "path")? {
                    Some(path) => expand_home(path),
                    None => self
                        .focused_pane()
                        .and_then(|pane| pane.session.cwd())
                        .ok_or_else(|| RpcError::invalid_params("`path` is required"))?,
                };
                let plans = crate::layout_file::load(&path).map_err(RpcError::invalid_params)?;
                let count = plans.len();
                self.open_layout(plans)
                    .map_err(|e| RpcError::failed(format!("layout failed: {e}")))?;
                Ok(json!({ "tabs_opened": count }))
            }
            "new_tab" => self.rpc_new_tab(p),
            "focus" => {
                let pane = self.target_pane(p)?;
                self.focus_pane(pane);
                Ok(json!({ "pane": pane }))
            }
            "close" => {
                let pane = self.target_pane(p)?;
                self.close_pane(pane);
                Ok(json!({ "closed": pane }))
            }
            "zoom" => {
                let pane = self.target_pane(p)?;
                self.focus_pane(pane);
                let want = p.get("on").and_then(Value::as_bool);
                let zoomed = self.active().is_some_and(|t| t.zoomed);
                if want.is_none_or(|w| w != zoomed) {
                    self.toggle_zoom();
                }
                Ok(json!({ "zoomed": self.active().is_some_and(|t| t.zoomed) }))
            }
            "set_title" => {
                let tab_id = opt_u32(p, "tab")?;
                let title = opt_str(p, "title")?.map(str::to_string);
                let index = match tab_id {
                    Some(id) => self
                        .tabs
                        .iter()
                        .position(|t| t.id == id)
                        .ok_or_else(|| RpcError::invalid_params(format!("no tab {id}")))?,
                    None => self.active_tab,
                };
                let tab = self
                    .tabs
                    .get_mut(index)
                    .ok_or_else(|| RpcError::failed("no tabs"))?;
                tab.title = title.filter(|t| !t.is_empty());
                let id = tab.id;
                self.layout_dirty = true;
                self.request_redraw();
                Ok(json!({ "tab": id }))
            }
            "resize" => {
                let pane = self.target_pane(p)?;
                self.focus_pane(pane);
                let dir = direction(p, Direction::Right)?;
                let steps = opt_u32(p, "amount")?.unwrap_or(1).min(20);
                for _ in 0..steps {
                    self.resize_focused(dir);
                }
                Ok(json!({ "pane": pane }))
            }
            other => Err(RpcError::not_found(other)),
        }
    }

    /// `params.pane`, defaulting to the focused pane, checked to exist.
    fn target_pane(&self, params: &Value) -> Result<PaneId, RpcError> {
        match opt_u32(params, "pane")? {
            Some(id) if self.pane(id).is_some() => Ok(id),
            Some(id) => Err(RpcError::invalid_params(format!("no pane {id}"))),
            None => self
                .focused_pane()
                .map(|p| p.id)
                .ok_or_else(|| RpcError::failed("no panes")),
        }
    }

    fn rpc_list_tabs(&self) -> Value {
        Value::Array(
            self.tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| {
                    json!({
                        "id": tab.id,
                        "index": index + 1,
                        "title": self.tab_title(tab),
                        "active": index == self.active_tab,
                        "zoomed": tab.zoomed,
                        "broadcast": tab.broadcast,
                        "focused_pane": tab.focused,
                        "panes": tab.root.panes(),
                    })
                })
                .collect(),
        )
    }

    fn rpc_list_panes(&self) -> Value {
        let mut out = Vec::new();
        for tab in &self.tabs {
            for id in tab.root.panes() {
                let Some(pane) = self.pane(id) else { continue };
                let size = pane.session.size();
                let shell = pane.session.shell.lock().clone();
                out.push(json!({
                    "id": id,
                    "tab": tab.id,
                    "title": pane.title,
                    "cwd": pane.session.cwd(),
                    "cols": size.cols,
                    "rows": size.rows,
                    "focused": id == self.focused,
                    "shell_integration": shell.prompts > 0,
                    "command_running": shell.command_running,
                    "last_exit": shell.last_exit,
                }));
            }
        }
        Value::Array(out)
    }

    /// The screen (default), or the last `lines` lines including
    /// scrollback, as plain text.
    fn rpc_get_text(&self, p: &Value) -> Outcome {
        let id = self.target_pane(p)?;
        let pane = self
            .pane(id)
            .ok_or_else(|| RpcError::failed("pane vanished"))?;
        let term = pane.session.term.lock();
        let rows = term.screen_lines() as i32;
        let text = match opt_u32(p, "lines")? {
            Some(n) => {
                // Count back from the last non-empty line on screen.
                let screen = frame::lines_text(&term, 0, rows - 1);
                let used = screen.lines().count().max(1) as i32;
                frame::lines_text(&term, used - n as i32, used - 1)
            }
            None => frame::lines_text(&term, 0, rows - 1),
        };
        Ok(json!({ "pane": id, "text": text }))
    }

    /// Writes text to a pane as if typed. `paste: true` sends it as a
    /// (bracketed) paste instead, so shells don't run it line by line.
    fn rpc_send_text(&mut self, p: &Value) -> Outcome {
        let id = self.target_pane(p)?;
        let text = opt_str(p, "text")?
            .ok_or_else(|| RpcError::invalid_params("`text` is required"))?
            .to_string();
        let as_paste = p.get("paste").and_then(Value::as_bool).unwrap_or(false);
        let pane = self
            .pane(id)
            .ok_or_else(|| RpcError::failed("pane vanished"))?;
        let bytes = if as_paste {
            let bracketed = pane
                .session
                .term
                .lock()
                .mode()
                .contains(TermMode::BRACKETED_PASTE);
            paste::paste_bytes(&text, bracketed)
        } else {
            text.into_bytes()
        };
        pane.session.term.lock().scroll_display(Scroll::Bottom);
        pane.session.write(bytes);
        Ok(json!({ "pane": id }))
    }

    /// The pane's most recent commands (shell integration), oldest first:
    /// command, cwd, exit, timing and, with `output: true`, their output
    /// while it's still in the scrollback.
    fn rpc_blocks(&self, p: &Value) -> Outcome {
        let id = self.target_pane(p)?;
        let limit = opt_u32(p, "limit")?.unwrap_or(10).clamp(1, 100) as usize;
        let with_output = p.get("output").and_then(Value::as_bool).unwrap_or(false);
        let pane = self
            .pane(id)
            .ok_or_else(|| RpcError::failed("pane vanished"))?;
        let metas: Vec<_> = {
            let shell = pane.session.shell.lock();
            let commands: Vec<_> = shell.blocks.iter().filter(|b| b.is_command()).collect();
            commands[commands.len().saturating_sub(limit)..]
                .iter()
                .map(|b| (*b).clone())
                .collect()
        };
        let term = pane.session.term.lock();
        Ok(Value::Array(
            metas
                .into_iter()
                .map(|b| {
                    let output = with_output
                        .then(|| {
                            crate::blocks::span_of(&*term, b.mark)
                                .map(|span| crate::blocks::output_text(&*term, &span))
                        })
                        .flatten();
                    json!({
                        "command": b.command,
                        "cwd": b.cwd,
                        "exit": b.exit,
                        "started_ms": b.started_ms,
                        "finished_ms": b.finished_ms,
                        "duration_ms": b.finished_ms.zip(b.started_ms).map(|(f, s)| f.saturating_sub(s)),
                        "output": output,
                    })
                })
                .collect(),
        ))
    }

    /// Runs a command in a new pane split from the focused one (down by
    /// default) without taking focus, so whoever's typing keeps typing.
    /// `since_ms` marks the start, for waiting on the command's block.
    fn rpc_agent_run(&mut self, p: &Value) -> Outcome {
        let command = opt_str(p, "command")?
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| RpcError::invalid_params("`command` is required"))?
            .to_string();
        let target = self.target_pane(p)?;
        let dir = direction(p, Direction::Down)?;
        let cwd = match opt_str(p, "cwd")? {
            Some(c) => Some(expand_home(c)),
            None => self.pane(target).and_then(|pane| pane.session.cwd()),
        };
        let since_ms = crate::shell::tap::now_ms();
        self.focus_pane(target);
        let id = self
            .split_with(dir, cwd)
            .map_err(|e| RpcError::failed(format!("couldn't open a pane: {e}")))?;
        self.focus_pane(target);
        self.run_in(id, Some(&command));
        Ok(json!({ "pane": id, "since_ms": since_ms }))
    }

    fn rpc_split(&mut self, p: &Value) -> Outcome {
        let target = self.target_pane(p)?;
        let dir = direction(p, Direction::Right)?;
        let cwd = match opt_str(p, "cwd")? {
            Some(c) => Some(expand_home(c)),
            None => self.pane(target).and_then(|pane| pane.session.cwd()),
        };
        self.focus_pane(target);
        let id = self
            .split_with(dir, cwd)
            .map_err(|e| RpcError::failed(format!("split failed: {e}")))?;
        self.run_in(id, opt_str(p, "command")?);
        Ok(json!({ "pane": id }))
    }

    fn rpc_new_tab(&mut self, p: &Value) -> Outcome {
        let cwd = match opt_str(p, "cwd")? {
            Some(c) => Some(expand_home(c)),
            None => self.focused_pane().and_then(|pane| pane.session.cwd()),
        };
        let id = self
            .open_tab(cwd)
            .map_err(|e| RpcError::failed(format!("new tab failed: {e}")))?;
        if let Some(title) = opt_str(p, "title")? {
            if let Some(tab) = self.active_mut() {
                tab.title = Some(title.to_string());
            }
        }
        self.run_in(id, opt_str(p, "command")?);
        let tab = self.active().map(|t| t.id);
        Ok(json!({ "tab": tab, "pane": id }))
    }

    /// Types a command into a freshly spawned shell once it's ready, so
    /// the shell stays (with the command in its history) after the command
    /// exits.
    pub(super) fn run_in(&mut self, pane: PaneId, command: Option<&str>) {
        let Some(command) = command.filter(|c| !c.trim().is_empty()) else {
            return;
        };
        if let Some(pane) = self.pane_mut(pane) {
            pane.pending_command = Some((command.to_string(), Instant::now()));
        }
    }
}
