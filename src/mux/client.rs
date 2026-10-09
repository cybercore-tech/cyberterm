// src/mux/client.rs
//
// A window's connection to the daemon. A background reader keeps one
// replica terminal per pane: it applies the pane's snapshot, then every
// byte the daemon's terminal parsed, so the replica always matches. All
// the window's code (rendering, selection, scrollback, links, prompt
// jumps) works on replicas exactly as on local terminals.

use std::collections::HashMap;
use std::io::{self, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use alacritty_terminal::Term;
use parking_lot::Mutex;

use super::protocol::{ClientMsg, Frame, PaneInfo, ServerMsg, Size, VERSION};
use super::server::socket_path;
use crate::session::{EventProxy, EventSink, GridSize, PaneId, UserEvent};
use crate::shell::ShellState;

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// A pane's local copy of the daemon's terminal.
pub struct Replica {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub shell: Arc<Mutex<ShellState>>,
    pub pid: u32,
    info: PaneInfo,
    parser: Processor<StdSyncHandler>,
}

type Replicas = Arc<Mutex<HashMap<PaneId, Replica>>>;

/// What a window session needs from a replica: its terminal, shell state
/// and the shell's pid.
pub type ReplicaHandle = (
    Arc<FairMutex<Term<EventProxy>>>,
    Arc<Mutex<ShellState>>,
    u32,
);

pub struct DaemonClient {
    writer: Mutex<UnixStream>,
    next_req: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, Sender<ServerMsg>>>>,
    /// Where `SessionReady` / `Error` go while an attach is waiting.
    ready: Arc<Mutex<Option<Sender<ServerMsg>>>>,
    replicas: Replicas,
}

fn grid(size: Size) -> GridSize {
    GridSize {
        cols: size.cols.max(2) as usize,
        rows: size.rows.max(1) as usize,
    }
}

fn shell_state(info: &super::protocol::ShellInfo) -> ShellState {
    ShellState {
        cwd: info.cwd.clone(),
        last_exit: info.last_exit,
        command_running: info.command_running,
        prompts: info.prompts,
    }
}

impl DaemonClient {
    /// Connects to the daemon, starting it first if it isn't running.
    pub fn connect_or_start(sink: EventSink) -> io::Result<Arc<Self>> {
        let path = socket_path();
        let stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(_) => {
                start_daemon()?;
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    match UnixStream::connect(&path) {
                        Ok(s) => break s,
                        Err(e) if Instant::now() >= deadline => return Err(e),
                        Err(_) => std::thread::sleep(Duration::from_millis(50)),
                    }
                }
            }
        };
        Self::start(stream, Some(sink))
    }

    /// Connects without starting anything (for CLI queries).
    pub fn connect_existing() -> io::Result<Arc<Self>> {
        Self::start(UnixStream::connect(socket_path())?, None)
    }

    fn start(stream: UnixStream, sink: Option<EventSink>) -> io::Result<Arc<Self>> {
        let read_half = stream.try_clone()?;
        let client = Arc::new(Self {
            writer: Mutex::new(stream),
            next_req: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            ready: Arc::new(Mutex::new(None)),
            replicas: Arc::new(Mutex::new(HashMap::new())),
        });
        let (pending, ready, replicas) = (
            client.pending.clone(),
            client.ready.clone(),
            client.replicas.clone(),
        );
        std::thread::Builder::new()
            .name("daemon reader".into())
            .spawn(move || read_loop(read_half, sink, pending, ready, replicas))?;
        client.send(&ClientMsg::Hello {
            version: VERSION.into(),
        })?;
        Ok(client)
    }

    pub fn send(&self, msg: &ClientMsg) -> io::Result<()> {
        let mut w = self.writer.lock();
        Frame::json(msg).write_to(&mut *w)?;
        w.flush()
    }

    pub fn send_input(&self, pane: PaneId, bytes: Vec<u8>) {
        let mut w = self.writer.lock();
        let _ = Frame::Input(pane, bytes).write_to(&mut *w);
        let _ = w.flush();
    }

    /// Sends a request carrying `req` and waits for its reply.
    fn request(&self, make: impl FnOnce(u64) -> ClientMsg) -> io::Result<ServerMsg> {
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().insert(req, tx);
        self.send(&make(req))?;
        rx.recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the daemon didn't answer"))
    }

    /// Sends an attach-style message and waits for `SessionReady`.
    fn session(&self, msg: ClientMsg) -> io::Result<(String, serde_json::Value, Vec<PaneInfo>)> {
        let (tx, rx) = mpsc::channel();
        *self.ready.lock() = Some(tx);
        self.send(&msg)?;
        let reply = rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the daemon didn't answer"))?;
        *self.ready.lock() = None;
        match reply {
            ServerMsg::SessionReady {
                name,
                layout,
                panes,
            } => Ok((name, layout, panes)),
            ServerMsg::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn new_session(
        &self,
        name: Option<String>,
    ) -> io::Result<(String, serde_json::Value, Vec<PaneInfo>)> {
        self.session(ClientMsg::NewSession { name })
    }

    /// Attaches to a session. `force` takes it over from another client
    /// (which is told it was detached).
    pub fn attach(
        &self,
        name: Option<String>,
        force: bool,
    ) -> io::Result<(String, serde_json::Value, Vec<PaneInfo>)> {
        self.session(ClientMsg::Attach { name, force })
    }

    /// Starts a shell in the attached session; its replica is ready when
    /// this returns.
    pub fn spawn(&self, build: impl FnOnce(u64) -> ClientMsg) -> io::Result<PaneInfo> {
        match self.request(build)? {
            ServerMsg::Spawned { pane, .. } => Ok(pane),
            ServerMsg::Ack { message, .. } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn list_sessions(&self) -> io::Result<Vec<super::protocol::SessionSummary>> {
        match self.request(|req| ClientMsg::ListSessions { req })? {
            ServerMsg::Sessions { sessions, .. } => Ok(sessions),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn kill_session(&self, name: &str) -> io::Result<()> {
        match self.request(|req| ClientMsg::KillSession {
            req,
            name: name.to_string(),
        })? {
            ServerMsg::Ack { ok: true, .. } => Ok(()),
            ServerMsg::Ack { message, .. } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    /// The replica terminal and shell state of a pane the daemon reported.
    pub fn replica(&self, pane: PaneId) -> Option<ReplicaHandle> {
        self.replicas
            .lock()
            .get(&pane)
            .map(|r| (r.term.clone(), r.shell.clone(), r.pid))
    }

    pub fn forget(&self, pane: PaneId) {
        self.replicas.lock().remove(&pane);
    }

    /// Closes the connection (this client won't use the daemon again).
    pub fn close(&self) {
        let _ = self.writer.lock().shutdown(std::net::Shutdown::Both);
    }
}

/// Starts `cyberterm +daemon` detached from this process: its own process
/// group, no stdio, so neither closing the window nor a terminal hangup
/// takes it down.
fn start_daemon() -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe: PathBuf = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .arg("+daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    // Reap it if it exits early (e.g. lost a race with another daemon).
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn new_replica(info: &PaneInfo, sink: &EventSink) -> Replica {
    let config = super::term_config(&info.settings);
    let listener = EventProxy::new(info.id, sink.clone());
    Replica {
        term: Arc::new(FairMutex::new(Term::new(
            config,
            &grid(info.size),
            listener,
        ))),
        shell: Arc::new(Mutex::new(shell_state(&info.shell))),
        pid: info.pid,
        info: info.clone(),
        parser: Processor::new(),
    }
}

fn read_loop(
    stream: UnixStream,
    sink: Option<EventSink>,
    pending: Arc<Mutex<HashMap<u64, Sender<ServerMsg>>>>,
    ready: Arc<Mutex<Option<Sender<ServerMsg>>>>,
    replicas: Replicas,
) {
    let mut reader = BufReader::new(stream);
    let wake = |pane: PaneId, event: TermEvent| {
        if let Some(sink) = &sink {
            sink(UserEvent::Term(pane, event));
        }
    };
    while let Ok(Some(frame)) = Frame::read_from(&mut reader) {
        match frame {
            Frame::Output(pane, bytes) => {
                if let Some(r) = replicas.lock().get_mut(&pane) {
                    let mut term = r.term.lock();
                    r.parser.advance(&mut *term, &bytes);
                }
                wake(pane, TermEvent::Wakeup);
            }
            Frame::Snapshot(pane, bytes) => {
                if let Some(r) = replicas.lock().get_mut(&pane) {
                    let mut term = r.term.lock();
                    let size = grid(r.info.size);
                    let listener =
                        EventProxy::new(pane, sink.clone().expect("replicas need an event sink"));
                    *term = Term::new(super::term_config(&r.info.settings), &size, listener);
                    r.parser = Processor::new();
                    r.parser.advance(&mut *term, &bytes);
                }
                wake(pane, TermEvent::Wakeup);
            }
            Frame::Json(value) => {
                let Ok(msg) = serde_json::from_value::<ServerMsg>(value) else {
                    continue;
                };
                match &msg {
                    ServerMsg::SessionReady { panes, .. } => {
                        if let Some(sink) = &sink {
                            let mut map = replicas.lock();
                            for info in panes {
                                map.insert(info.id, new_replica(info, sink));
                            }
                        }
                    }
                    ServerMsg::Spawned { pane, .. } => {
                        if let Some(sink) = &sink {
                            replicas.lock().insert(pane.id, new_replica(pane, sink));
                        }
                    }
                    ServerMsg::ShellState { pane, shell } => {
                        if let Some(r) = replicas.lock().get(pane) {
                            *r.shell.lock() = shell_state(shell);
                        }
                        continue;
                    }
                    ServerMsg::Detached { reason } => {
                        if let Some(sink) = &sink {
                            sink(UserEvent::DaemonLost(reason.clone()));
                        }
                        continue;
                    }
                    ServerMsg::PaneExited { pane } => {
                        replicas.lock().remove(pane);
                        wake(*pane, TermEvent::Exit);
                        continue;
                    }
                    _ => {}
                }
                if let Some(req) = msg.reply_to() {
                    if let Some(tx) = pending.lock().remove(&req) {
                        let _ = tx.send(msg);
                    }
                } else if matches!(
                    msg,
                    ServerMsg::SessionReady { .. } | ServerMsg::Error { .. }
                ) {
                    if let Some(tx) = ready.lock().as_ref() {
                        let _ = tx.send(msg);
                    } else if let ServerMsg::Error { message } = msg {
                        eprintln!("cyberterm: daemon: {message}");
                    }
                }
            }
            Frame::Input(..) => {}
        }
    }
    if let Some(sink) = &sink {
        sink(UserEvent::DaemonLost(
            "lost the session daemon; these shells are gone".into(),
        ));
    }
}
