// src/control.rs
//
// The control socket: a local API for scripts, the `cyberterm +ctl` CLI,
// and (Phase 3) the MCP bridge that lets AI agents work with panes.
//
// Transport: a Unix socket at `$XDG_RUNTIME_DIR/cyberterm/<pid>.sock`, in a
// directory only the user can enter (0700), so the same trust boundary as
// tmux's socket. Shells started by Cyberterm get its path in
// `CYBERTERM_SOCKET`, so `+ctl` inside a pane talks to its own window.
//
// Protocol: one JSON-RPC 2.0 request per line, one response per line.
// Requests are handed to the GUI thread (which owns all terminal state)
// and the connection thread waits for the reply.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use winit::event_loop::EventLoopProxy;

use crate::session::UserEvent;

/// How long a connection waits for the GUI thread to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    pub fn not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("unknown method `{method}`"),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn new(id: Value, outcome: Result<Value, RpcError>) -> Self {
        let (result, error) = match outcome {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            jsonrpc: "2.0".into(),
            id,
            result,
            error,
        }
    }
}

/// A request plus where to send its answer.
pub struct Call {
    pub request: Request,
    pub reply: mpsc::Sender<Response>,
}

impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call")
            .field("request", &self.request)
            .finish()
    }
}

/// `$XDG_RUNTIME_DIR/cyberterm` (falling back to `/tmp/cyberterm-<uid>`).
pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("cyberterm"),
        None => PathBuf::from(format!("/tmp/cyberterm-{}", current_uid())),
    }
}

fn current_uid() -> u32 {
    // The real uid, from /proc rather than a libc call.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|u| u.parse().ok())
        })
        .unwrap_or(0)
}

/// Creates the socket directory, private to the user (0700).
pub fn prepare_socket_dir() -> io::Result<PathBuf> {
    let dir = socket_dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// The listening socket; removes its file when dropped.
pub struct Server {
    path: PathBuf,
}

impl Server {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Binds the socket and starts accepting connections on a background
    /// thread. Calls are delivered to the event loop as
    /// `UserEvent::Control`.
    pub fn start(proxy: EventLoopProxy<UserEvent>) -> io::Result<Self> {
        let dir = prepare_socket_dir()?;
        remove_stale_sockets(&dir);
        let path = dir.join(format!("{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        std::thread::Builder::new()
            .name("control socket".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let proxy = proxy.clone();
                    let _ = std::thread::Builder::new()
                        .name("control connection".into())
                        .spawn(move || serve(stream, proxy));
                }
            })?;
        Ok(Self { path })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Deletes sockets left by Cyberterms that were killed (no destructor ran):
/// a socket named after a pid that no longer exists.
fn remove_stale_sockets(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Session links whose window is gone point at nothing.
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if is_link && !path.exists() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let pid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok());
        let is_sock = path.extension().is_some_and(|x| x == "sock");
        if let (true, Some(pid)) = (is_sock, pid) {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn serve(stream: UnixStream, proxy: EventLoopProxy<UserEvent>) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::new(
                Value::Null,
                Err(RpcError {
                    code: -32700,
                    message: format!("parse error: {e}"),
                }),
            ),
            Ok(request) => {
                let id = request.id.clone();
                let (tx, rx) = mpsc::channel();
                let sent = proxy.send_event(UserEvent::Control(Call { request, reply: tx }));
                match sent.ok().and_then(|_| rx.recv_timeout(REPLY_TIMEOUT).ok()) {
                    Some(response) => response,
                    None => Response::new(id, Err(RpcError::failed("terminal did not answer"))),
                }
            }
        };
        let Ok(mut text) = serde_json::to_string(&response) else {
            return;
        };
        text.push('\n');
        if writer.write_all(text.as_bytes()).is_err() {
            return;
        }
    }
}

/// Client side: picks the socket to talk to. `CYBERTERM_SOCKET` (set in
/// every Cyberterm shell) wins; otherwise the most recently started
/// Cyberterm that still answers.
pub fn find_socket() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CYBERTERM_SOCKET") {
        return Some(PathBuf::from(path));
    }
    let mut sockets: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(socket_dir())
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "sock"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    sockets.sort();
    sockets
        .into_iter()
        .rev()
        .map(|(_, p)| p)
        .find(|p| UnixStream::connect(p).is_ok())
}

/// Sends one request and waits for its response.
pub fn call(socket: &Path, method: &str, params: Value) -> io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT + Duration::from_secs(1)))?;
    let request =
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let mut text = request.to_string();
    text.push('\n');
    stream.write_all(text.as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

/// Turns `key=value` CLI arguments into a params object. Values that parse
/// as JSON (numbers, true/false, null, quoted strings, arrays) are used as
/// such; anything else is a string.
pub fn params_from_args(args: &[String]) -> Result<Value, String> {
    let mut map = serde_json::Map::new();
    for arg in args {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("expected key=value, got `{arg}`"))?;
        let value =
            serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()));
        map.insert(key.replace('-', "_"), value);
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parse_json_values_and_fall_back_to_strings() {
        let args: Vec<String> = [
            "pane=3",
            "text=ls -la",
            "paste=true",
            "dry-run=null",
            "list=[1,2]",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let p = params_from_args(&args).unwrap();
        assert_eq!(p["pane"], 3);
        assert_eq!(p["text"], "ls -la");
        assert_eq!(p["paste"], true);
        assert!(p["dry_run"].is_null());
        assert_eq!(p["list"][1], 2);
        assert!(params_from_args(&["nope".to_string()]).is_err());
    }

    #[test]
    fn responses_carry_result_or_error_not_both() {
        let ok =
            serde_json::to_value(Response::new(7.into(), Ok(serde_json::json!({"a": 1})))).unwrap();
        assert_eq!(ok["id"], 7);
        assert_eq!(ok["result"]["a"], 1);
        assert!(ok.get("error").is_none());
        let err = serde_json::to_value(Response::new(Value::Null, Err(RpcError::not_found("x"))))
            .unwrap();
        assert_eq!(err["error"]["code"], -32601);
        assert!(err.get("result").is_none());
    }

    #[test]
    fn requests_default_their_id_and_params() {
        let r: Request = serde_json::from_str(r#"{"method":"list_panes"}"#).unwrap();
        assert!(r.id.is_null());
        assert!(r.params.is_null());
    }
}
