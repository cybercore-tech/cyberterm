// src/mux/protocol.rs
//
// The daemon wire protocol. Every frame is `[u32 length][u8 kind][body]`
// (length counts kind + body, big-endian). Terminal output and input are
// raw bytes -- they're the hot path and often not UTF-8 -- and everything
// else is a JSON message.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::PaneId;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Largest frame accepted (a 10k-line scrollback snapshot is a few MB).
const MAX_FRAME: usize = 64 * 1024 * 1024;

const KIND_JSON: u8 = 1;
const KIND_OUTPUT: u8 = 2;
const KIND_INPUT: u8 = 3;
const KIND_SNAPSHOT: u8 = 4;

#[derive(Debug, PartialEq)]
pub enum Frame {
    Json(Value),
    /// Bytes the daemon's terminal just parsed, for a pane's replica.
    Output(PaneId, Vec<u8>),
    /// Bytes to write to a pane's PTY.
    Input(PaneId, Vec<u8>),
    /// A full replay of a pane's state (see `snapshot.rs`); the replica
    /// starts over from a blank terminal.
    Snapshot(PaneId, Vec<u8>),
}

impl Frame {
    pub fn json<T: Serialize>(msg: &T) -> Self {
        Frame::Json(serde_json::to_value(msg).unwrap_or(Value::Null))
    }

    pub fn encode(&self) -> Vec<u8> {
        let (kind, pane, body): (u8, Option<PaneId>, Vec<u8>) = match self {
            Frame::Json(v) => (KIND_JSON, None, serde_json::to_vec(v).unwrap_or_default()),
            Frame::Output(p, b) => (KIND_OUTPUT, Some(*p), b.clone()),
            Frame::Input(p, b) => (KIND_INPUT, Some(*p), b.clone()),
            Frame::Snapshot(p, b) => (KIND_SNAPSHOT, Some(*p), b.clone()),
        };
        let len = 1 + pane.map_or(0, |_| 4) + body.len();
        let mut out = Vec::with_capacity(4 + len);
        out.extend_from_slice(&(len as u32).to_be_bytes());
        out.push(kind);
        if let Some(p) = pane {
            out.extend_from_slice(&p.to_be_bytes());
        }
        out.extend_from_slice(&body);
        out
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())
    }

    /// Reads one frame; `Ok(None)` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut len = [0u8; 4];
        match r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame length",
            ));
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        let kind = buf[0];
        let pane_body = |buf: &[u8]| -> io::Result<(PaneId, Vec<u8>)> {
            if buf.len() < 5 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "short frame"));
            }
            let pane = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
            Ok((pane, buf[5..].to_vec()))
        };
        Ok(Some(match kind {
            KIND_JSON => Frame::Json(
                serde_json::from_slice(&buf[1..])
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            ),
            KIND_OUTPUT => {
                let (p, b) = pane_body(&buf)?;
                Frame::Output(p, b)
            }
            KIND_INPUT => {
                let (p, b) = pane_body(&buf)?;
                Frame::Input(p, b)
            }
            KIND_SNAPSHOT => {
                let (p, b) = pane_body(&buf)?;
                Frame::Snapshot(p, b)
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown frame kind {other}"),
                ))
            }
        }))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// Terminal settings a pane is created with; the window's replica uses
/// the same ones so both parse the stream identically.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TermSettings {
    pub scrollback: usize,
    pub kitty_keyboard: bool,
    pub cursor_shape: String,
    pub cursor_blinking: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: PaneId,
    pub pid: u32,
    pub size: Size,
    pub title: String,
    pub settings: TermSettings,
    pub shell: ShellInfo,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShellInfo {
    pub cwd: Option<PathBuf>,
    pub last_exit: Option<i32>,
    pub command_running: bool,
    pub prompts: u64,
    /// Command blocks (see `shell::BlockMeta`), so windows can draw them.
    #[serde(default)]
    pub blocks: Vec<crate::shell::BlockMeta>,
    /// A remote shell's reported (host, directory), for SSH-aware splits.
    #[serde(default)]
    pub remote_cwd: Option<(String, PathBuf)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub name: String,
    pub panes: usize,
    pub attached: bool,
    /// Seconds since the session was created.
    pub age: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        version: String,
    },
    /// Start a new, empty session and attach to it.
    NewSession {
        name: Option<String>,
    },
    /// Attach to a session; `None` picks the most recently detached one.
    Attach {
        name: Option<String>,
        /// Take the session over from a client that has it attached.
        #[serde(default)]
        force: bool,
    },
    Spawn {
        req: u64,
        size: Size,
        settings: TermSettings,
        program: Option<String>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: HashMap<String, String>,
    },
    Resize {
        pane: PaneId,
        size: Size,
    },
    /// Close a pane and hang up its shell.
    Kill {
        pane: PaneId,
    },
    /// The window's tabs and splits, stored with the session so a later
    /// attach restores them.
    SaveLayout {
        layout: Value,
    },
    /// Colors the daemon reports to programs that query them (OSC 4/10/11).
    SetPalette {
        ansi: [u32; 16],
        fg: u32,
        bg: u32,
        cursor: u32,
    },
    ListSessions {
        req: u64,
    },
    KillSession {
        req: u64,
        name: String,
    },
    Detach,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello {
        version: String,
    },
    /// Attached. Snapshot frames for each pane follow, then live output.
    SessionReady {
        name: String,
        layout: Value,
        panes: Vec<PaneInfo>,
    },
    Spawned {
        req: u64,
        pane: PaneInfo,
    },
    PaneExited {
        pane: PaneId,
    },
    ShellState {
        pane: PaneId,
        shell: ShellInfo,
    },
    Sessions {
        req: u64,
        sessions: Vec<SessionSummary>,
    },
    Ack {
        req: u64,
        ok: bool,
        message: String,
    },
    Error {
        message: String,
    },
    /// This client was detached (another one took the session over).
    Detached {
        reason: String,
    },
}

impl ServerMsg {
    /// The request id a reply answers, if it's a reply.
    pub fn reply_to(&self) -> Option<u64> {
        match self {
            ServerMsg::Spawned { req, .. }
            | ServerMsg::Sessions { req, .. }
            | ServerMsg::Ack { req, .. } => Some(*req),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: Frame) {
        let bytes = frame.encode();
        let back = Frame::read_from(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn frames_round_trip() {
        roundtrip(Frame::Output(7, b"\x1b[31mhi\xff\xfe".to_vec()));
        roundtrip(Frame::Input(1, vec![]));
        roundtrip(Frame::Snapshot(u32::MAX, vec![0; 1000]));
        roundtrip(Frame::json(&ClientMsg::Attach {
            name: Some("work".into()),
            force: false,
        }));
    }

    #[test]
    fn messages_are_tagged_json() {
        let v = serde_json::to_value(ClientMsg::Kill { pane: 3 }).unwrap();
        assert_eq!(v["type"], "kill");
        let m: ServerMsg =
            serde_json::from_value(serde_json::json!({"type": "pane_exited", "pane": 4})).unwrap();
        assert_eq!(m, ServerMsg::PaneExited { pane: 4 });
        assert_eq!(
            ServerMsg::Ack {
                req: 9,
                ok: true,
                message: String::new()
            }
            .reply_to(),
            Some(9)
        );
    }

    #[test]
    fn end_of_stream_and_garbage() {
        assert!(Frame::read_from(&mut &b""[..]).unwrap().is_none());
        assert!(Frame::read_from(&mut &[0, 0, 0, 1, 99][..]).is_err());
        assert!(Frame::read_from(&mut &[0, 0, 0, 0][..]).is_err());
        assert!(Frame::read_from(&mut &[0xff, 0xff, 0xff, 0xff][..]).is_err());
    }
}
