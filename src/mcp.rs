// src/mcp.rs
//
// `cyberterm +mcp`: an MCP (Model Context Protocol) server on stdio, so any
// MCP-capable agent -- Claude Code, Codex CLI, Gemini CLI, Cursor, Zed, ...
// -- can work with the user's terminal: list panes, read them, look at
// recent commands and history, run a command in a new visible pane, wait
// for it, or type into a pane.
//
// The bridge holds no terminal state and no privileges of its own: every
// tool call becomes a control-socket call tagged with the agent's name, and
// the window decides. Reading a pane needs a per-pane grant and typing or
// running always asks the user ([agents] in the config), with an agent
// badge on the pane and an audit log. That keeps well-behaved agents from
// acting silently; it isn't a sandbox against programs already running as
// the user, who can reach the socket directly.
//
// Transport: newline-delimited JSON-RPC 2.0 (MCP stdio).

use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Protocol versions this server speaks, newest first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Where tool calls go: the control socket (or a fake in tests).
pub trait Backend {
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// The real backend: the control socket of the Cyberterm this agent runs
/// in, else the newest running one.
pub struct SocketBackend;

impl Backend for SocketBackend {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let socket = crate::control::find_socket()
            .ok_or("no running Cyberterm found (is [control] enabled?)")?;
        let timeout = if params.get("agent").is_some() {
            crate::control::AGENT_TIMEOUT
        } else {
            Duration::from_secs(10)
        };
        let response = crate::control::call_with_timeout(&socket, method, params, timeout)
            .map_err(|e| match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                    "the user didn't answer the request in time".to_string()
                }
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => format!(
                    "Cyberterm isn't running (no control socket at {})",
                    socket.display()
                ),
                _ => e.to_string(),
            })?;
        match (response.result, response.error) {
            (_, Some(e)) => Err(e.message),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }
}

pub struct Server<B: Backend> {
    backend: B,
    /// The client's name from `initialize` (`claude-code`, ...), shown to
    /// the user in consent prompts and the audit log.
    agent: String,
}

fn tool(name: &str, title: &str, description: &str, schema: Value, read_only: bool) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": schema,
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": !read_only,
            "openWorldHint": false,
        },
    })
}

fn tools() -> Vec<Value> {
    let pane = json!({"type": "integer", "description": "Pane id from list_panes. Omit for the focused pane."});
    vec![
        tool(
            "list_panes",
            "List terminal panes",
            "List the panes open in the user's Cyberterm window: id, tab, title, working directory, size, whether a command is running and the last exit code. Start here to find the pane you need.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
            true,
        ),
        tool(
            "read_pane",
            "Read a pane's text",
            "Return the text of a pane: what's on screen, or the last `lines` lines including scrollback. The user must allow reading each pane; the first read of a pane asks them.",
            json!({"type": "object", "properties": {
                "pane": pane,
                "lines": {"type": "integer", "minimum": 1, "maximum": 5000, "description": "Read this many lines back through the scrollback instead of just the screen."}
            }, "additionalProperties": false}),
            true,
        ),
        tool(
            "recent_commands",
            "Recent commands in a pane",
            "List the most recent commands run in a pane (needs shell integration): command line, directory, exit code, duration, and optionally each command's output. Best way to see what just failed and why.",
            json!({"type": "object", "properties": {
                "pane": pane,
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "How many commands, newest last (default 5)."},
                "output": {"type": "boolean", "description": "Include each command's output (default true)."}
            }, "additionalProperties": false}),
            true,
        ),
        tool(
            "search_history",
            "Search command history",
            "Search the user's saved command history across all terminals by words in the command line or its output, newest first. Returns command, directory, exit code, time and a matching excerpt. Secrets were redacted before saving.",
            json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "Words that must all appear in the command or its output."},
                "failed": {"type": "boolean", "description": "Only commands that exited non-zero."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100}
            }, "additionalProperties": false}),
            true,
        ),
        tool(
            "run_command",
            "Run a command in a new pane",
            "Run a shell command in a NEW pane the user can see (split from the focused pane). The user confirms first. Returns the pane id and a start time to pass to wait_for_command. Use this rather than send_text for anything that runs.",
            json!({"type": "object", "properties": {
                "command": {"type": "string", "description": "The shell command line."},
                "cwd": {"type": "string", "description": "Working directory (default: the focused pane's)."},
                "direction": {"type": "string", "enum": ["right", "down"], "description": "Where the new pane opens (default down)."}
            }, "required": ["command"], "additionalProperties": false}),
            false,
        ),
        tool(
            "wait_for_command",
            "Wait for a command to finish",
            "Wait (up to `timeout_seconds`) for the command started by run_command -- or any command started in the pane after `since_ms` -- to finish, then return its exit code and output. If it's still running at the timeout, returns the current screen instead.",
            json!({"type": "object", "properties": {
                "pane": {"type": "integer"},
                "since_ms": {"type": "integer", "description": "From run_command's result."},
                "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Default 60."}
            }, "required": ["pane"], "additionalProperties": false}),
            true,
        ),
        tool(
            "send_text",
            "Type into a pane",
            "Type text into an existing pane, as if the user typed it (add \"\\r\" to press Enter). The user confirms first. Prefer run_command for running commands; use this to answer a prompt or drive an interactive program the user started.",
            json!({"type": "object", "properties": {
                "pane": {"type": "integer"},
                "text": {"type": "string"}
            }, "required": ["pane", "text"], "additionalProperties": false}),
            false,
        ),
    ]
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// Long text keeps its end (where errors and prompts are).
fn tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars;
    format!(
        "[… {skip} earlier characters omitted …]\n{}",
        text.chars().skip(skip).collect::<String>()
    )
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            agent: "agent".into(),
        }
    }

    /// Handles one JSON-RPC message; returns the response, if it needs one.
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        // Notifications (no id) get no response.
        let id = id?;
        let outcome: Result<Value, (i32, String)> = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => Ok(self.call_tool(&params)),
            "resources/list" => Ok(json!({"resources": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    fn initialize(&mut self, params: &Value) -> Value {
        if let Some(name) = params.pointer("/clientInfo/name").and_then(Value::as_str) {
            self.agent = name.chars().take(40).collect();
        }
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .filter(|v| VERSIONS.contains(v))
            .unwrap_or(VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "cyberterm", "title": "Cyberterm", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Tools for the user's Cyberterm terminal. Call list_panes first. Reading a pane needs the user's permission per pane; run_command and send_text always ask the user to confirm, so tell them what you're about to do. Prefer run_command (a new visible pane) over typing into the user's own panes.",
        })
    }

    fn agent_params(&self, mut params: Value) -> Value {
        if let Value::Object(map) = &mut params {
            map.insert("agent".into(), json!(self.agent));
        }
        params
    }

    fn call_tool(&self, params: &Value) -> Value {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let result = match name {
            "list_panes" => self
                .backend
                .call("list_panes", self.agent_params(json!({})))
                .map(|v| pretty(&v)),
            "read_pane" => self
                .backend
                .call(
                    "get_text",
                    self.agent_params(pick(&args, &["pane", "lines"])),
                )
                .map(|v| tail(v["text"].as_str().unwrap_or_default(), 60_000)),
            "recent_commands" => {
                let mut p = pick(&args, &["pane", "limit", "output"]);
                if p.get("output").is_none() {
                    p["output"] = json!(true);
                }
                self.backend
                    .call("blocks", self.agent_params(p))
                    .map(|v| pretty(&v))
            }
            "search_history" => self
                .backend
                .call(
                    "history",
                    self.agent_params(pick(&args, &["query", "failed", "limit"])),
                )
                .map(|v| pretty(&v)),
            "run_command" => self
                .backend
                .call(
                    "agent_run",
                    self.agent_params(pick(&args, &["command", "cwd", "direction"])),
                )
                .map(|v| pretty(&v)),
            "wait_for_command" => self.wait_for_command(&args),
            "send_text" => self
                .backend
                .call(
                    "send_text",
                    self.agent_params(pick(&args, &["pane", "text"])),
                )
                .map(|_| "Sent.".to_string()),
            other => Err(format!("unknown tool `{other}`")),
        };
        match result {
            Ok(text) => text_result(text, false),
            Err(e) => text_result(e, true),
        }
    }

    /// Polls the pane's blocks until one that started after `since_ms`
    /// finishes, or the timeout passes.
    fn wait_for_command(&self, args: &Value) -> Result<String, String> {
        let pane = args
            .get("pane")
            .and_then(Value::as_u64)
            .ok_or("`pane` is required")?;
        let since = args.get("since_ms").and_then(Value::as_u64).unwrap_or(0);
        let timeout = Duration::from_secs(
            args.get("timeout_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(60)
                .min(600),
        );
        let deadline = Instant::now() + timeout;
        let mut first = true;
        loop {
            let blocks = self.backend.call(
                "blocks",
                self.agent_params(
                    json!({"pane": pane, "limit": 10, "output": true, "poll": !first}),
                ),
            )?;
            first = false;
            let done = blocks.as_array().and_then(|list| {
                list.iter().rev().find(|b| {
                    b["started_ms"].as_u64().is_some_and(|s| s >= since)
                        && !b["finished_ms"].is_null()
                })
            });
            if let Some(block) = done {
                let exit = block["exit"].as_i64().map_or("?".into(), |e| e.to_string());
                let output = tail(block["output"].as_str().unwrap_or_default(), 60_000);
                return Ok(format!(
                    "Command `{}` finished with exit code {exit} in {} ms.\n\n{output}",
                    block["command"].as_str().unwrap_or_default(),
                    block["duration_ms"].as_u64().unwrap_or(0),
                ));
            }
            if Instant::now() >= deadline {
                let screen = self
                    .backend
                    .call("get_text", self.agent_params(json!({"pane": pane})))
                    .ok()
                    .and_then(|v| v["text"].as_str().map(str::to_string))
                    .unwrap_or_default();
                return Ok(format!(
                    "Still running after {}s (or the pane has no shell integration). Current screen:\n\n{}",
                    timeout.as_secs(),
                    tail(&screen, 20_000)
                ));
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    }
}

fn pick(args: &Value, keys: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for k in keys {
        if let Some(v) = args.get(*k).filter(|v| !v.is_null()) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    Value::Object(out)
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Runs the server on stdin/stdout until stdin closes.
pub fn run() -> io::Result<()> {
    let mut server = Server::new(SocketBackend);
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.handle(&msg),
            Err(e) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}}),
            ),
        };
        if let Some(response) = response {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<(String, Value)>>,
        blocks: RefCell<Vec<Value>>,
    }

    impl Backend for &Fake {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            self.calls
                .borrow_mut()
                .push((method.to_string(), params.clone()));
            match method {
                "list_panes" => Ok(json!([{"id": 1, "title": "zsh"}])),
                "get_text" if params["pane"] == 9 => Err("no pane 9".into()),
                "get_text" => Ok(json!({"text": "hello screen"})),
                "blocks" => {
                    let mut b = self.blocks.borrow_mut();
                    Ok(if b.is_empty() {
                        json!([])
                    } else {
                        Value::Array(vec![b.remove(0)])
                    })
                }
                "agent_run" => Ok(json!({"pane": 4, "since_ms": 1000})),
                _ => Ok(json!({})),
            }
        }
    }

    fn rpc(server: &mut Server<&Fake>, id: i64, method: &str, params: Value) -> Value {
        server
            .handle(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .unwrap()
    }

    #[test]
    fn initialize_negotiates_version_and_remembers_the_agent() {
        let fake = Fake::default();
        let mut s = Server::new(&fake);
        let r = rpc(
            &mut s,
            1,
            "initialize",
            json!({"protocolVersion": "2025-03-26", "clientInfo": {"name": "claude-code"}}),
        );
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert_eq!(s.agent, "claude-code");
        let r = rpc(
            &mut s,
            2,
            "initialize",
            json!({"protocolVersion": "2099-01-01"}),
        );
        assert_eq!(r["result"]["protocolVersion"], VERSIONS[0]);
        // Notifications get no reply; unknown methods get -32601.
        assert!(s
            .handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none());
        assert_eq!(rpc(&mut s, 3, "nope", json!({}))["error"]["code"], -32601);
    }

    #[test]
    fn tools_are_listed_with_schemas_and_hints() {
        let fake = Fake::default();
        let mut s = Server::new(&fake);
        let r = rpc(&mut s, 1, "tools/list", json!({}));
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "list_panes",
            "read_pane",
            "recent_commands",
            "search_history",
            "run_command",
            "wait_for_command",
            "send_text",
        ] {
            assert!(names.contains(&expected), "{expected}");
        }
        let run = tools.iter().find(|t| t["name"] == "run_command").unwrap();
        assert_eq!(run["annotations"]["readOnlyHint"], false);
        assert_eq!(run["inputSchema"]["required"][0], "command");
    }

    #[test]
    fn calls_are_tagged_with_the_agent_and_errors_are_tool_errors() {
        let fake = Fake::default();
        let mut s = Server::new(&fake);
        rpc(
            &mut s,
            1,
            "initialize",
            json!({"clientInfo": {"name": "codex"}}),
        );
        let r = rpc(
            &mut s,
            2,
            "tools/call",
            json!({"name": "read_pane", "arguments": {"pane": 1}}),
        );
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["content"][0]["text"], "hello screen");
        let (method, params) = fake.calls.borrow().last().cloned().unwrap();
        assert_eq!(method, "get_text");
        assert_eq!(params["agent"], "codex");

        let r = rpc(
            &mut s,
            3,
            "tools/call",
            json!({"name": "read_pane", "arguments": {"pane": 9}}),
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no pane 9"));
        let r = rpc(
            &mut s,
            4,
            "tools/call",
            json!({"name": "rm_rf", "arguments": {}}),
        );
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn wait_for_command_returns_the_finished_block() {
        let fake = Fake::default();
        fake.blocks
            .borrow_mut()
            .push(json!({"command": "make", "started_ms": 1500, "finished_ms": null}));
        fake.blocks.borrow_mut().push(json!({"command": "make", "started_ms": 1500, "finished_ms": 2500, "exit": 2, "duration_ms": 1000, "output": "error: boom"}));
        let mut s = Server::new(&fake);
        let r = rpc(
            &mut s,
            1,
            "tools/call",
            json!({"name": "wait_for_command", "arguments": {"pane": 4, "since_ms": 1000, "timeout_seconds": 5}}),
        );
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("exit code 2"), "{text}");
        assert!(text.contains("error: boom"));
    }

    #[test]
    fn long_output_keeps_its_end() {
        let t = tail(&"x".repeat(100), 10);
        assert!(t.ends_with("xxxxxxxxxx"));
        assert!(t.contains("90 earlier"));
    }
}
