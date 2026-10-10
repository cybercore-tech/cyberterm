// src/session.rs
//
// One shell: its PTY, the alacritty_terminal `Term` it drives, the
// background I/O thread connecting the two, and what shell integration has
// reported. Sessions know nothing about windows or GPUs; the app owns them
// by `PaneId`, which is what makes several of them per window (splits,
// tabs) a layout problem rather than a rewrite.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::event::{Event as TermEvent, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::tty;
use alacritty_terminal::Term;
use parking_lot::Mutex;
use winit::event_loop::EventLoopProxy;

use crate::shell::{ShellState, TappedPty};

pub type PaneId = u32;

/// Events delivered to the winit loop from background threads.
#[derive(Debug)]
pub enum UserEvent {
    Term(PaneId, TermEvent),
    /// A request from the control socket (`control.rs`).
    Control(crate::control::Call),
    /// The session daemon's connection closed, or the daemon detached this
    /// client (the session was attached elsewhere); the reason.
    DaemonLost(String),
    /// An AI answer (`src/ai.rs`) for the request with this number.
    Ai(u64, Result<crate::ai::Answer, String>),
}

/// Where terminal and daemon events go: the window's event loop, or a
/// channel (the terminal attach client).
pub type EventSink = Arc<dyn Fn(UserEvent) + Send + Sync>;

pub fn sink_for(proxy: EventLoopProxy<UserEvent>) -> EventSink {
    Arc::new(move |event| {
        let _ = proxy.send_event(event);
    })
}

/// alacritty_terminal's `EventListener`, tagged with the pane it belongs to.
#[derive(Clone)]
pub struct EventProxy {
    pane: PaneId,
    sink: EventSink,
}

impl EventProxy {
    pub fn new(pane: PaneId, sink: EventSink) -> Self {
        Self { pane, sink }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        (self.sink)(UserEvent::Term(self.pane, event));
    }
}

/// Terminal size in cells, for `Term::new` / `Term::resize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct SpawnOptions {
    pub size: GridSize,
    pub cell_width: f32,
    pub cell_height: f32,
    pub program: Option<String>,
    /// Overrides `TERM` for this shell.
    pub term: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub term_config: TermConfig,
    /// The control socket, exported to the shell as `CYBERTERM_SOCKET`.
    pub control_socket: Option<PathBuf>,
}

pub struct Session {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    /// Working directory and command state reported by shell integration.
    pub shell: Arc<Mutex<ShellState>>,
    backend: Backend,
    size: GridSize,
    /// The shell's process id, for the `/proc` working-directory fallback.
    pid: u32,
    /// Recent output, for rewind.
    pub rewind: Arc<Mutex<crate::rewind::Recorder>>,
}

/// Where the shell actually runs.
enum Backend {
    /// A PTY owned by this window.
    Local(Notifier),
    /// A PTY owned by the session daemon; `term` is a replica it keeps in
    /// sync, and input/resizes are forwarded.
    Remote {
        pane: PaneId,
        client: Arc<crate::mux::client::DaemonClient>,
        cell: (u16, u16),
    },
}

fn window_size(size: GridSize, cell_width: f32, cell_height: f32) -> WindowSize {
    WindowSize {
        num_lines: size.rows as u16,
        num_cols: size.cols as u16,
        cell_width: cell_width as u16,
        cell_height: cell_height as u16,
    }
}

impl Session {
    pub fn spawn(
        pane: PaneId,
        proxy: EventLoopProxy<UserEvent>,
        opts: SpawnOptions,
    ) -> std::io::Result<Self> {
        let listener = EventProxy::new(pane, sink_for(proxy));
        let term = Arc::new(FairMutex::new(Term::new(
            opts.term_config,
            &opts.size,
            listener.clone(),
        )));

        let program = opts
            .program
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/bash".to_string());
        let mut env = HashMap::new();
        env.insert("TERM_PROGRAM".to_string(), "cyberterm".to_string());
        env.insert(
            "TERM_PROGRAM_VERSION".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        );
        env.insert("CYBERTERM_PANE".to_string(), pane.to_string());
        if let Some(socket) = &opts.control_socket {
            env.insert(
                "CYBERTERM_SOCKET".to_string(),
                socket.to_string_lossy().into_owned(),
            );
        }
        if let Some(term) = opts.term.filter(|t| !t.trim().is_empty()) {
            env.insert("TERM".to_string(), term);
        }
        let pty_options = tty::Options {
            shell: Some(tty::Shell::new(program, opts.args)),
            working_directory: opts.cwd,
            drain_on_exit: true,
            env,
        };
        let pty = tty::new(
            &pty_options,
            window_size(opts.size, opts.cell_width, opts.cell_height),
            pane as u64,
        )?;

        let pid = pty.child().id();
        let shell = Arc::new(Mutex::new(ShellState::default()));
        let rewind = Arc::new(Mutex::new(crate::rewind::Recorder::new(opts.size)));
        let pty = TappedPty::new(pty, shell.clone())?.with_recorder(rewind.clone());
        let event_loop = PtyEventLoop::new(term.clone(), listener, pty, true, false)?;
        let notifier = Notifier(event_loop.channel());
        event_loop.spawn();

        Ok(Self {
            term,
            shell,
            backend: Backend::Local(notifier),
            size: opts.size,
            pid,
            rewind,
        })
    }

    /// A pane living in the session daemon, from the replica its client
    /// keeps for it.
    pub fn remote(
        pane: PaneId,
        client: Arc<crate::mux::client::DaemonClient>,
        size: GridSize,
    ) -> Option<Self> {
        let (term, shell, pid, rewind) = client.replica(pane)?;
        Some(Self {
            term,
            shell,
            backend: Backend::Remote {
                pane,
                client,
                cell: (0, 0),
            },
            size,
            pid,
            rewind,
        })
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.backend, Backend::Remote { .. })
    }

    /// Ends the shell. Local sessions also end when dropped; remote ones
    /// only end when asked, since dropping one just means the window let
    /// go of it (detach).
    pub fn kill(&self) {
        match &self.backend {
            Backend::Local(notifier) => {
                let _ = notifier.0.send(Msg::Shutdown);
            }
            Backend::Remote { pane, client, .. } => {
                let _ = client.send(&crate::mux::protocol::ClientMsg::Kill { pane: *pane });
                client.forget(*pane);
            }
        }
    }

    pub fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        match &self.backend {
            Backend::Local(notifier) => notifier.notify(bytes),
            Backend::Remote { pane, client, .. } => {
                let bytes = bytes.into();
                if !bytes.is_empty() {
                    client.send_input(*pane, bytes.into_owned());
                }
            }
        }
    }

    /// The shell's process id (in the daemon's PID namespace -- the same
    /// machine -- for daemon panes).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The shell's current directory. The kernel's view (Linux `/proc`) is
    /// always current; shell integration's OSC 7 report lags behind a
    /// command that `cd`s and keeps running, so it's the fallback.
    pub fn cwd(&self) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/cwd", self.pid))
            .ok()
            .or_else(|| self.shell.lock().cwd.clone())
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    pub fn resize(&mut self, size: GridSize, cell_width: f32, cell_height: f32) {
        self.images.lock().cell = (cell_width.max(1.0), cell_height.max(1.0));
        let ws = window_size(size, cell_width, cell_height);
        match &mut self.backend {
            Backend::Local(notifier) => {
                if size == self.size {
                    return;
                }
                self.size = size;
                self.term.lock().resize(size);
                self.rewind.lock().resize(size);
                notifier.on_resize(ws);
            }
            Backend::Remote { pane, client, cell } => {
                // The pixel cell size matters to the daemon too (programs
                // can ask for it), so resend when only that changed.
                let new_cell = (ws.cell_width, ws.cell_height);
                if size == self.size && *cell == new_cell {
                    return;
                }
                self.size = size;
                *cell = new_cell;
                self.term.lock().resize(size);
                self.rewind.lock().resize(size);
                let _ = client.send(&crate::mux::protocol::ClientMsg::Resize {
                    pane: *pane,
                    size: crate::mux::protocol::Size {
                        cols: ws.num_cols,
                        rows: ws.num_lines,
                        cell_width: ws.cell_width,
                        cell_height: ws.cell_height,
                    },
                });
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A local session stops its PTY thread, which hangs up the shell.
        // A remote one is left running in the daemon (detach).
        if let Backend::Local(notifier) = &self.backend {
            let _ = notifier.0.send(Msg::Shutdown);
        }
    }
}
