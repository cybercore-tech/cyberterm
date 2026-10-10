// src/mux/server.rs
//
// The session daemon (`cyberterm +daemon`). It owns every shell: the PTY,
// an authoritative terminal parsing its output, and shell-integration
// state. Windows attach to a session, get a snapshot of each pane, then
// the live byte stream; closing a window just detaches, and the shells
// keep running until their session is killed or they exit.
//
// Threads: one accepting connections, one reader plus one writer per
// client, and one I/O loop per pane. A pane loop parses each chunk and
// broadcasts it while holding that pane's terminal lock; attaching takes
// the same lock to snapshot and subscribe, so a client never misses or
// double-applies output.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, OnResize, WindowSize};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite};
use alacritty_terminal::vte::ansi::{Processor, Rgb, StdSyncHandler};
use alacritty_terminal::Term;
use parking_lot::Mutex;
use polling::{Event as PollEvent, Events, PollMode, Poller};
use serde_json::Value;

use super::protocol::{
    ClientMsg, Frame, PaneInfo, ServerMsg, SessionSummary, ShellInfo, Size, TermSettings, VERSION,
};
use super::snapshot::snapshot;
use crate::frame::{query_color, Palette};
use crate::session::{GridSize, PaneId};
use crate::shell::{ShellState, TappedPty};

/// The daemon's socket.
pub fn socket_path() -> PathBuf {
    crate::control::socket_dir().join("daemon.sock")
}

/// How long the daemon lingers with no sessions and no clients.
const IDLE_EXIT: Duration = Duration::from_secs(3);

type ClientId = u64;

enum PaneCmd {
    Input(Vec<u8>),
    Resize(Size),
    Kill,
}

/// State shared between a pane's loop, its terminal's event listener and
/// the hub.
struct PaneShared {
    cmd_tx: Sender<PaneCmd>,
    poller: Arc<Poller>,
    title: Mutex<String>,
    size: Mutex<Size>,
    palette: Mutex<Palette>,
}

impl PaneShared {
    fn send(&self, cmd: PaneCmd) {
        let _ = self.cmd_tx.send(cmd);
        let _ = self.poller.notify();
    }
}

/// Answers the queries programs make of their terminal (cursor position,
/// device attributes, colors, text area size). Only the daemon answers;
/// window replicas ignore these, so nothing is answered twice.
#[derive(Clone)]
struct PaneListener(Arc<PaneShared>);

impl EventListener for PaneListener {
    fn send_event(&self, event: TermEvent) {
        let s = &self.0;
        match event {
            TermEvent::PtyWrite(text) => s.send(PaneCmd::Input(text.into_bytes())),
            TermEvent::Title(title) => *s.title.lock() = title,
            TermEvent::ResetTitle => s.title.lock().clear(),
            TermEvent::ColorRequest(index, format) => {
                let [r, g, b] = query_color(&s.palette.lock(), index);
                s.send(PaneCmd::Input(format(Rgb { r, g, b }).into_bytes()));
            }
            TermEvent::TextAreaSizeRequest(format) => {
                let size = *s.size.lock();
                s.send(PaneCmd::Input(format(window_size(size)).into_bytes()));
            }
            TermEvent::ClipboardLoad(_, format) => {
                s.send(PaneCmd::Input(format("").into_bytes()));
            }
            _ => {}
        }
    }
}

/// Where finished commands are saved, and what to save.
pub type HistorySink = Option<(crate::history::Recorder, crate::history::Policy)>;

struct ServerPane {
    id: PaneId,
    session: String,
    history: HistorySink,
    pid: u32,
    settings: TermSettings,
    config: TermConfig,
    term: Arc<FairMutex<Term<PaneListener>>>,
    listener: PaneListener,
    shell: Arc<Mutex<ShellState>>,
    shared: Arc<PaneShared>,
    subscribers: Mutex<Vec<(ClientId, Sender<Frame>)>>,
}

impl ServerPane {
    fn info(&self) -> PaneInfo {
        let shell = self.shell.lock().clone();
        PaneInfo {
            id: self.id,
            pid: self.pid,
            size: *self.shared.size.lock(),
            title: self.shared.title.lock().clone(),
            settings: self.settings.clone(),
            shell: shell_info(&shell),
        }
    }

    /// Sends to every subscribed client, dropping ones that went away.
    fn broadcast(&self, frame: &Frame) {
        let mut subs = self.subscribers.lock();
        subs.retain(|(_, tx)| tx.send(clone_frame(frame)).is_ok());
    }

    /// Snapshot + subscribe atomically with respect to the output stream.
    fn attach(&self, client: ClientId, tx: &Sender<Frame>) {
        let mut term = self.term.lock();
        let title = self.shared.title.lock().clone();
        let snap = snapshot(&mut term, &self.config, self.listener.clone(), Some(&title));
        let _ = tx.send(Frame::Snapshot(self.id, snap));
        let mut subs = self.subscribers.lock();
        subs.retain(|(c, _)| *c != client);
        subs.push((client, tx.clone()));
        let alt = term
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
        drop(subs);
        drop(term);
        if alt {
            // Full-screen programs keep state a snapshot can't carry (scroll
            // regions, ...); a resize makes them redraw from scratch.
            let size = *self.shared.size.lock();
            if size.rows > 2 {
                self.shared.send(PaneCmd::Resize(Size {
                    rows: size.rows - 1,
                    ..size
                }));
                self.shared.send(PaneCmd::Resize(size));
            }
        }
    }

    fn unsubscribe(&self, client: ClientId) {
        self.subscribers.lock().retain(|(c, _)| *c != client);
    }
}

fn clone_frame(frame: &Frame) -> Frame {
    match frame {
        Frame::Json(v) => Frame::Json(v.clone()),
        Frame::Output(p, b) => Frame::Output(*p, b.clone()),
        Frame::Input(p, b) => Frame::Input(*p, b.clone()),
        Frame::Snapshot(p, b) => Frame::Snapshot(*p, b.clone()),
    }
}

fn shell_info(s: &ShellState) -> ShellInfo {
    ShellInfo {
        cwd: s.cwd.clone(),
        remote_cwd: s.remote_cwd.clone(),
        last_exit: s.last_exit,
        command_running: s.command_running,
        prompts: s.prompts,
        blocks: s.blocks.iter().cloned().collect(),
    }
}

fn window_size(size: Size) -> WindowSize {
    WindowSize {
        num_lines: size.rows,
        num_cols: size.cols,
        cell_width: size.cell_width,
        cell_height: size.cell_height,
    }
}

fn grid_size(size: Size) -> GridSize {
    GridSize {
        cols: size.cols.max(2) as usize,
        rows: size.rows.max(1) as usize,
    }
}

struct SessionRec {
    layout: Value,
    panes: Vec<PaneId>,
    attached: Option<ClientId>,
    created: Instant,
    detached_at: Option<Instant>,
    palette: Palette,
}

struct ClientRec {
    tx: Sender<Frame>,
    session: Option<String>,
}

#[derive(Default)]
struct Hub {
    history: HistorySink,
    sessions: BTreeMap<String, SessionRec>,
    panes: HashMap<PaneId, Arc<ServerPane>>,
    clients: HashMap<ClientId, ClientRec>,
    next_pane: PaneId,
    next_client: ClientId,
    next_session: u32,
}

impl Hub {
    fn send(&self, client: ClientId, msg: &ServerMsg) {
        if let Some(c) = self.clients.get(&client) {
            let _ = c.tx.send(Frame::json(msg));
        }
    }

    fn new_session_name(&mut self) -> String {
        loop {
            self.next_session += 1;
            let name = self.next_session.to_string();
            if !self.sessions.contains_key(&name) {
                return name;
            }
        }
    }

    /// Detaches a client from its session (the session keeps running).
    fn detach(&mut self, client: ClientId) {
        let Some(name) = self.clients.get_mut(&client).and_then(|c| c.session.take()) else {
            return;
        };
        if let Some(session) = self.sessions.get_mut(&name) {
            if session.attached == Some(client) {
                session.attached = None;
                session.detached_at = Some(Instant::now());
            }
            for id in &session.panes {
                if let Some(pane) = self.panes.get(id) {
                    pane.unsubscribe(client);
                }
            }
        }
    }

    /// A pane's shell exited (or it was killed): forget it, tell the
    /// window, and drop its session once it has no panes left.
    fn pane_gone(&mut self, id: PaneId) {
        self.panes.remove(&id);
        let Some((name, session)) = self
            .sessions
            .iter_mut()
            .find(|(_, s)| s.panes.contains(&id))
        else {
            return;
        };
        session.panes.retain(|p| *p != id);
        let attached = session.attached;
        let empty = session.panes.is_empty();
        let name = name.clone();
        if let Some(client) = attached {
            self.send(client, &ServerMsg::PaneExited { pane: id });
        }
        if empty {
            self.sessions.remove(&name);
            if let Some(client) = attached.and_then(|c| self.clients.get_mut(&c)) {
                client.session = None;
            }
        }
    }

    fn kill_session(&mut self, name: &str) -> bool {
        let Some(session) = self.sessions.get(name) else {
            return false;
        };
        for id in &session.panes {
            if let Some(pane) = self.panes.get(id) {
                pane.shared.send(PaneCmd::Kill);
            }
        }
        true
    }

    fn idle(&self) -> bool {
        self.sessions.is_empty() && self.clients.is_empty()
    }
}

/// Runs the daemon until it has had no sessions and no clients for a few
/// seconds. Fails if another daemon already answers on the socket.
pub fn run(history: HistorySink) -> io::Result<()> {
    crate::control::prepare_socket_dir()?;
    tty::setup_env();
    run_at(&socket_path(), IDLE_EXIT, history)
}

/// The daemon on a given socket; `idle_exit` is how long it lingers once
/// it has no sessions and no clients.
pub fn run_at(path: &std::path::Path, idle_exit: Duration, history: HistorySink) -> io::Result<()> {
    let path = path.to_path_buf();
    if UnixStream::connect(&path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "a Cyberterm daemon is already running",
        ));
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let hub = Arc::new(Mutex::new(Hub {
        history,
        ..Hub::default()
    }));
    let mut idle_since = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let hub = hub.clone();
                std::thread::Builder::new()
                    .name("daemon client".into())
                    .spawn(move || serve_client(stream, hub))?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
        if hub.lock().idle() {
            if idle_since.elapsed() >= idle_exit {
                break;
            }
        } else {
            idle_since = Instant::now();
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn serve_client(stream: UnixStream, hub: Arc<Mutex<Hub>>) {
    let (tx, rx) = mpsc::channel::<Frame>();
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    std::thread::spawn(move || write_frames(write_half, rx));

    let client = {
        let mut h = hub.lock();
        h.next_client += 1;
        let id = h.next_client;
        h.clients.insert(
            id,
            ClientRec {
                tx: tx.clone(),
                session: None,
            },
        );
        id
    };

    let mut reader = io::BufReader::new(stream);
    while let Ok(Some(frame)) = Frame::read_from(&mut reader) {
        match frame {
            Frame::Input(pane, bytes) => {
                if let Some(p) = hub.lock().panes.get(&pane) {
                    p.shared.send(PaneCmd::Input(bytes));
                }
            }
            Frame::Json(value) => match serde_json::from_value::<ClientMsg>(value) {
                Ok(msg) => handle(msg, client, &tx, &hub),
                Err(e) => {
                    let _ = tx.send(Frame::json(&ServerMsg::Error {
                        message: format!("bad message: {e}"),
                    }));
                }
            },
            _ => {}
        }
    }

    let mut h = hub.lock();
    h.detach(client);
    h.clients.remove(&client);
}

fn write_frames(stream: UnixStream, rx: Receiver<Frame>) {
    let mut w = BufWriter::new(stream);
    while let Ok(frame) = rx.recv() {
        if frame.write_to(&mut w).is_err() {
            return;
        }
        // Batch whatever else is already queued, then flush.
        while let Ok(more) = rx.try_recv() {
            if more.write_to(&mut w).is_err() {
                return;
            }
        }
        if w.flush().is_err() {
            return;
        }
    }
}

fn handle(msg: ClientMsg, client: ClientId, tx: &Sender<Frame>, hub: &Arc<Mutex<Hub>>) {
    let reply = |m: ServerMsg| {
        let _ = tx.send(Frame::json(&m));
    };
    match msg {
        ClientMsg::Hello { .. } => reply(ServerMsg::Hello {
            version: VERSION.into(),
        }),
        ClientMsg::NewSession { name } => {
            let mut h = hub.lock();
            h.detach(client);
            let name = match name.filter(|n| !n.trim().is_empty()) {
                Some(n) if h.sessions.contains_key(&n) => {
                    reply(ServerMsg::Error {
                        message: format!("session `{n}` already exists"),
                    });
                    return;
                }
                Some(n) => n,
                None => h.new_session_name(),
            };
            h.sessions.insert(
                name.clone(),
                SessionRec {
                    layout: Value::Null,
                    panes: Vec::new(),
                    attached: Some(client),
                    created: Instant::now(),
                    detached_at: None,
                    palette: Palette::default(),
                },
            );
            if let Some(c) = h.clients.get_mut(&client) {
                c.session = Some(name.clone());
            }
            reply(ServerMsg::SessionReady {
                name,
                layout: Value::Null,
                panes: Vec::new(),
            });
        }
        ClientMsg::Attach { name, force } => {
            let mut h = hub.lock();
            h.detach(client);
            let picked = match name {
                Some(n) => Some(n),
                None => h
                    .sessions
                    .iter()
                    .filter(|(_, s)| s.attached.is_none())
                    .max_by_key(|(_, s)| s.detached_at.unwrap_or(s.created))
                    .map(|(n, _)| n.clone()),
            };
            let Some(name) = picked else {
                reply(ServerMsg::Error {
                    message: "no detached session to attach to".into(),
                });
                return;
            };
            let Some(session) = h.sessions.get_mut(&name) else {
                reply(ServerMsg::Error {
                    message: format!("no session `{name}`"),
                });
                return;
            };
            if let Some(other) = session.attached.filter(|c| *c != client) {
                if !force {
                    reply(ServerMsg::Error {
                        message: format!(
                            "session `{name}` is attached elsewhere (use --force to take it over)"
                        ),
                    });
                    return;
                }
                h.detach(other);
                h.send(
                    other,
                    &ServerMsg::Detached {
                        reason: format!("session `{name}` was attached from another terminal"),
                    },
                );
            }
            let Some(session) = h.sessions.get_mut(&name) else {
                return;
            };
            session.attached = Some(client);
            session.detached_at = None;
            let layout = session.layout.clone();
            let ids = session.panes.clone();
            if let Some(c) = h.clients.get_mut(&client) {
                c.session = Some(name.clone());
            }
            let panes: Vec<Arc<ServerPane>> = ids
                .iter()
                .filter_map(|id| h.panes.get(id).cloned())
                .collect();
            drop(h);
            reply(ServerMsg::SessionReady {
                name,
                layout,
                panes: panes.iter().map(|p| p.info()).collect(),
            });
            for pane in panes {
                pane.attach(client, tx);
            }
        }
        ClientMsg::Spawn {
            req,
            size,
            settings,
            program,
            args,
            cwd,
            env,
        } => {
            let session = hub
                .lock()
                .clients
                .get(&client)
                .and_then(|c| c.session.clone());
            let Some(session) = session else {
                reply(ServerMsg::Error {
                    message: "spawn before attaching to a session".into(),
                });
                return;
            };
            let spec = SpawnSpec {
                size,
                settings,
                program,
                args,
                cwd,
                env,
            };
            match spawn_pane(hub, &session, spec, client, tx, req) {
                Ok(()) => {}
                Err(e) => reply(ServerMsg::Ack {
                    req,
                    ok: false,
                    message: format!("couldn't start a shell: {e}"),
                }),
            }
        }
        ClientMsg::Resize { pane, size } => {
            if let Some(p) = hub.lock().panes.get(&pane) {
                p.shared.send(PaneCmd::Resize(size));
            }
        }
        ClientMsg::Kill { pane } => {
            if let Some(p) = hub.lock().panes.get(&pane) {
                p.shared.send(PaneCmd::Kill);
            }
        }
        ClientMsg::SaveLayout { layout } => {
            let mut h = hub.lock();
            let name = h.clients.get(&client).and_then(|c| c.session.clone());
            if let Some(session) = name.and_then(|n| h.sessions.get_mut(&n)) {
                session.layout = layout;
            }
        }
        ClientMsg::SetPalette {
            ansi,
            fg,
            bg,
            cursor,
        } => {
            let palette = Palette {
                ansi,
                fg,
                bg,
                cursor,
            };
            let mut h = hub.lock();
            let name = h.clients.get(&client).and_then(|c| c.session.clone());
            if let Some(session) = name.and_then(|n| h.sessions.get_mut(&n)) {
                session.palette = palette;
                let ids = session.panes.clone();
                for id in ids {
                    if let Some(p) = h.panes.get(&id) {
                        *p.shared.palette.lock() = palette;
                    }
                }
            }
        }
        ClientMsg::ListSessions { req } => {
            let h = hub.lock();
            let sessions = h
                .sessions
                .iter()
                .map(|(name, s)| SessionSummary {
                    name: name.clone(),
                    panes: s.panes.len(),
                    attached: s.attached.is_some(),
                    age: s.created.elapsed().as_secs(),
                })
                .collect();
            reply(ServerMsg::Sessions { req, sessions });
        }
        ClientMsg::KillSession { req, name } => {
            let ok = hub.lock().kill_session(&name);
            reply(ServerMsg::Ack {
                req,
                ok,
                message: if ok {
                    String::new()
                } else {
                    format!("no session `{name}`")
                },
            });
        }
        ClientMsg::Detach => hub.lock().detach(client),
    }
}

struct SpawnSpec {
    size: Size,
    settings: TermSettings,
    program: Option<String>,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: HashMap<String, String>,
}

/// Starts a shell in `session`. The requesting client is subscribed and
/// sent `Spawned` *before* the pane starts reading, so the first output it
/// sees can't arrive ahead of the pane's announcement or be missed.
fn spawn_pane(
    hub: &Arc<Mutex<Hub>>,
    session: &str,
    spec: SpawnSpec,
    client: ClientId,
    tx: &Sender<Frame>,
    req: u64,
) -> io::Result<()> {
    let SpawnSpec {
        size,
        settings,
        program,
        args,
        cwd,
        env,
    } = spec;
    let id = {
        let mut h = hub.lock();
        h.next_pane += 1;
        h.next_pane
    };
    let (palette, history) = {
        let h = hub.lock();
        (
            h.sessions
                .get(session)
                .map(|s| s.palette)
                .unwrap_or_default(),
            h.history.clone(),
        )
    };

    let program = program
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/bash".to_string());
    let mut env = env;
    env.insert("CYBERTERM_PANE".to_string(), id.to_string());
    let options = tty::Options {
        shell: Some(tty::Shell::new(program, args)),
        working_directory: cwd.filter(|c| c.is_dir()),
        drain_on_exit: true,
        env,
    };
    let pty = tty::new(&options, window_size(size), id as u64)?;
    let pid = pty.child().id();
    let shell = Arc::new(Mutex::new(ShellState::default()));
    let pty = TappedPty::new(pty, shell.clone())?.with_da1_answer()?;

    let (cmd_tx, cmd_rx) = mpsc::channel();
    let shared = Arc::new(PaneShared {
        cmd_tx,
        poller: Arc::new(Poller::new()?),
        title: Mutex::new(String::new()),
        size: Mutex::new(size),
        palette: Mutex::new(palette),
    });
    let listener = PaneListener(shared.clone());
    let config = super::term_config(&settings);
    let term = Arc::new(FairMutex::new(Term::new(
        config.clone(),
        &grid_size(size),
        listener.clone(),
    )));
    let pane = Arc::new(ServerPane {
        id,
        session: session.to_string(),
        history,
        pid,
        settings,
        config,
        term,
        listener,
        shell,
        shared,
        subscribers: Mutex::new(Vec::new()),
    });
    {
        let mut h = hub.lock();
        h.panes.insert(id, pane.clone());
        if let Some(s) = h.sessions.get_mut(session) {
            s.panes.push(id);
        }
    }
    pane.subscribers.lock().push((client, tx.clone()));
    let _ = tx.send(Frame::json(&ServerMsg::Spawned {
        req,
        pane: pane.info(),
    }));
    let (pane_for_loop, hub_for_loop) = (pane.clone(), hub.clone());
    std::thread::Builder::new()
        .name(format!("pane {id}"))
        .spawn(move || {
            pane_loop(&pane_for_loop, pty, cmd_rx);
            hub_for_loop.lock().pane_gone(pane_for_loop.id);
        })?;
    Ok(())
}

/// Poll keys alacritty_terminal registers the PTY under (`tty/unix.rs`).
const PTY_READ_WRITE: usize = 0;
const PTY_CHILD_EVENT: usize = 1;

/// One pane's I/O: PTY output -> terminal -> subscribers, commands -> PTY.
/// Returns when the shell exits or the pane is killed.
fn pane_loop(pane: &ServerPane, mut pty: TappedPty, cmds: Receiver<PaneCmd>) {
    let poller = pane.shared.poller.clone();
    let mut interest = PollEvent::readable(PTY_READ_WRITE);
    // SAFETY: deregistered below before the PTY is dropped.
    if unsafe { pty.register(&poller, interest, PollMode::Level) }.is_err() {
        return;
    }
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    let mut events = Events::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut writes: VecDeque<Vec<u8>> = VecDeque::new();
    let mut last_shell = pane.shell.lock().clone();

    'outer: loop {
        let deadline = parser.sync_timeout().sync_timeout();
        let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        events.clear();
        if let Err(e) = poller.wait(&mut events, timeout) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        // A synchronized update that never finished: apply it, and resync
        // windows (their parsers are still waiting for its end).
        if deadline.is_some_and(|d| Instant::now() >= d) {
            let mut term = pane.term.lock();
            parser.stop_sync(&mut *term);
            let title = pane.shared.title.lock().clone();
            let snap = snapshot(&mut term, &pane.config, pane.listener.clone(), Some(&title));
            pane.broadcast(&Frame::Snapshot(pane.id, snap));
        }

        while let Ok(cmd) = cmds.try_recv() {
            match cmd {
                PaneCmd::Input(bytes) => writes.push_back(bytes),
                PaneCmd::Resize(size) => {
                    pty.on_resize(window_size(size));
                    pane.term.lock().resize(grid_size(size));
                    *pane.shared.size.lock() = size;
                }
                PaneCmd::Kill => break 'outer,
            }
        }

        for event in events.iter() {
            match event.key {
                PTY_CHILD_EVENT => {
                    if let Some(ChildEvent::Exited(_)) = pty.next_child_event() {
                        read_available(pane, &mut pty, &mut parser, &mut buf);
                        break 'outer;
                    }
                }
                PTY_READ_WRITE
                    if event.readable && !read_available(pane, &mut pty, &mut parser, &mut buf) =>
                {
                    break 'outer;
                }
                _ => {}
            }
        }

        // Flush pending input (non-blocking).
        while let Some(front) = writes.front_mut() {
            match pty.writer().write(front) {
                Ok(n) if n == front.len() => {
                    writes.pop_front();
                }
                Ok(n) => {
                    front.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break 'outer,
            }
        }
        let want_write = !writes.is_empty();
        if want_write != interest.writable {
            interest.writable = want_write;
            if pty.reregister(&poller, interest, PollMode::Level).is_err() {
                break;
            }
        }

        if let Some((recorder, policy)) = &pane.history {
            let records = {
                let term = pane.term.lock();
                let mut shell = pane.shell.lock();
                crate::history::collect(&*term, &mut shell, &pane.session, policy)
            };
            recorder.record(records);
        }
        let shell = pane.shell.lock().clone();
        if shell != last_shell {
            pane.broadcast(&Frame::json(&ServerMsg::ShellState {
                pane: pane.id,
                shell: shell_info(&shell),
            }));
            last_shell = shell;
        }
    }
    let _ = pty.deregister(&poller);
}

/// Reads what the PTY has (bounded, to stay responsive to commands).
/// Returns false on a fatal read error.
fn read_available(
    pane: &ServerPane,
    pty: &mut TappedPty,
    parser: &mut Processor<StdSyncHandler>,
    buf: &mut [u8],
) -> bool {
    for _ in 0..16 {
        match pty.reader().read(buf) {
            Ok(0) => return true,
            Ok(n) => {
                let mut term = pane.term.lock();
                parser.advance(&mut *term, &buf[..n]);
                pane.broadcast(&Frame::Output(pane.id, buf[..n].to_vec()));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return true;
            }
            // EIO: the shell hung up; the child-exit event follows.
            Err(e) if e.raw_os_error() == Some(5) => return true,
            Err(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    //! End-to-end: a real daemon on a temporary socket, a real `/bin/sh`,
    //! and the wire protocol spoken directly, with a local replica kept the
    //! way a window keeps one.

    use super::*;
    use crate::mux::protocol::ShellInfo;
    use alacritty_terminal::grid::Dimensions;

    #[derive(Clone, Copy)]
    struct Quiet;
    impl EventListener for Quiet {}

    struct Conn {
        reader: io::BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl Conn {
        fn open(path: &std::path::Path) -> Self {
            let deadline = Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match UnixStream::connect(path) {
                    Ok(s) => break s,
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(e) => panic!("daemon never came up: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            Self {
                reader: io::BufReader::new(stream.try_clone().unwrap()),
                writer: stream,
            }
        }

        fn send(&mut self, msg: &ClientMsg) {
            Frame::json(msg).write_to(&mut self.writer).unwrap();
        }

        fn input(&mut self, pane: PaneId, text: &str) {
            Frame::Input(pane, text.as_bytes().to_vec())
                .write_to(&mut self.writer)
                .unwrap();
        }

        fn next(&mut self) -> Frame {
            Frame::read_from(&mut self.reader)
                .unwrap()
                .expect("daemon hung up")
        }

        fn next_msg(&mut self) -> ServerMsg {
            loop {
                if let Frame::Json(v) = self.next() {
                    return serde_json::from_value(v).unwrap();
                }
            }
        }
    }

    fn settings() -> TermSettings {
        TermSettings {
            scrollback: 1000,
            kitty_keyboard: true,
            cursor_shape: "block".into(),
            cursor_blinking: false,
        }
    }

    fn text(term: &Term<Quiet>) -> String {
        crate::frame::lines_text(term, -100, term.screen_lines() as i32 - 1)
    }

    /// Feeds output frames into the replica until `needle` shows up.
    fn pump_until(
        conn: &mut Conn,
        term: &mut Term<Quiet>,
        parser: &mut Processor<StdSyncHandler>,
        needle: &str,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !text(term).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "never saw {needle:?}; screen:\n{}",
                text(term)
            );
            match conn.next() {
                Frame::Output(_, bytes) | Frame::Snapshot(_, bytes) => parser.advance(term, &bytes),
                _ => {}
            }
        }
    }

    #[test]
    fn shells_survive_detach_and_reattach_restores_them() {
        let dir =
            std::env::temp_dir().join(format!("cyberterm-daemon-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        let server_path = path.clone();
        std::thread::spawn(move || run_at(&server_path, Duration::from_millis(300), None));

        let size = Size {
            cols: 40,
            rows: 10,
            cell_width: 8,
            cell_height: 16,
        };
        let config = crate::mux::term_config(&settings());

        // First window: new session, one shell, run a command.
        let mut a = Conn::open(&path);
        a.send(&ClientMsg::NewSession {
            name: Some("work".into()),
        });
        assert!(matches!(a.next_msg(), ServerMsg::SessionReady { ref name, .. } if name == "work"));
        let mut env = HashMap::new();
        env.insert("PS1".to_string(), "$ ".to_string());
        a.send(&ClientMsg::Spawn {
            req: 1,
            size,
            settings: settings(),
            program: Some("/bin/sh".into()),
            args: vec![],
            cwd: Some(dir.clone()),
            env,
        });
        let pane = match a.next_msg() {
            ServerMsg::Spawned { req: 1, pane } => pane.id,
            other => panic!("expected Spawned, got {other:?}"),
        };
        let mut replica = Term::new(config.clone(), &grid_size(size), Quiet);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        a.input(pane, "echo hello-$((40+2))\n");
        pump_until(&mut a, &mut replica, &mut parser, "hello-42");
        a.send(&ClientMsg::SaveLayout {
            layout: serde_json::json!({"tabs": "kept"}),
        });
        // Window closes: just drop the connection.
        drop(a);

        // Second window: reattach and get the same screen back.
        let mut b = Conn::open(&path);
        b.send(&ClientMsg::Attach {
            name: None,
            force: false,
        });
        let (layout, panes) = match b.next_msg() {
            ServerMsg::SessionReady {
                name,
                layout,
                panes,
            } => {
                assert_eq!(name, "work");
                (layout, panes)
            }
            other => panic!("expected SessionReady, got {other:?}"),
        };
        assert_eq!(layout["tabs"], "kept");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].id, pane);
        let mut replica = Term::new(config.clone(), &grid_size(size), Quiet);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        match b.next() {
            Frame::Snapshot(id, bytes) => {
                assert_eq!(id, pane);
                parser.advance(&mut replica, &bytes);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
        assert!(text(&replica).contains("hello-42"), "{}", text(&replica));

        // The shell is still alive and keeps working.
        b.input(pane, "echo still-$((6*7))\n");
        pump_until(&mut b, &mut replica, &mut parser, "still-42");

        // A second window can't grab an attached session.
        let mut c = Conn::open(&path);
        c.send(&ClientMsg::Attach {
            name: Some("work".into()),
            force: false,
        });
        assert!(matches!(c.next_msg(), ServerMsg::Error { .. }));

        // Session listing, then the shell exits and the session ends.
        c.send(&ClientMsg::ListSessions { req: 5 });
        match c.next_msg() {
            ServerMsg::Sessions { req: 5, sessions } => {
                assert_eq!(sessions.len(), 1);
                assert!(sessions[0].attached);
                assert_eq!(sessions[0].panes, 1);
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
        // --force takes the session over; the old client is told.
        c.send(&ClientMsg::Attach {
            name: Some("work".into()),
            force: true,
        });
        assert!(matches!(c.next_msg(), ServerMsg::SessionReady { .. }));
        assert!(matches!(c.next(), Frame::Snapshot(id, _) if id == pane));
        loop {
            if let ServerMsg::Detached { .. } = b.next_msg() {
                break;
            }
        }

        c.input(pane, "exit\n");
        loop {
            if let ServerMsg::PaneExited { pane: gone } = c.next_msg() {
                assert_eq!(gone, pane);
                break;
            }
        }
        c.send(&ClientMsg::ListSessions { req: 6 });
        assert!(
            matches!(c.next_msg(), ServerMsg::Sessions { req: 6, sessions } if sessions.is_empty())
        );
        let _ = ShellInfo::default();
        drop((b, c));
        let _ = std::fs::remove_dir_all(dir);
    }
}
