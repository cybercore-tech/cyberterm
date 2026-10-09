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
}

/// alacritty_terminal's `EventListener`, tagged with the pane it belongs to.
#[derive(Clone)]
pub struct EventProxy {
    pane: PaneId,
    proxy: EventLoopProxy<UserEvent>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.proxy.send_event(UserEvent::Term(self.pane, event));
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
}

pub struct Session {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    /// Working directory and command state reported by shell integration.
    pub shell: Arc<Mutex<ShellState>>,
    notifier: Notifier,
    size: GridSize,
    /// The shell's process id, for the `/proc` working-directory fallback.
    pid: u32,
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
        let listener = EventProxy { pane, proxy };
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
        let pty = TappedPty::new(pty, shell.clone())?;
        let event_loop = PtyEventLoop::new(term.clone(), listener, pty, true, false)?;
        let notifier = Notifier(event_loop.channel());
        event_loop.spawn();

        Ok(Self {
            term,
            shell,
            notifier,
            size: opts.size,
            pid,
        })
    }

    pub fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    /// The shell's current directory: what shell integration last
    /// reported (OSC 7), else what the kernel says (Linux `/proc`).
    pub fn cwd(&self) -> Option<PathBuf> {
        if let Some(cwd) = self.shell.lock().cwd.clone() {
            return Some(cwd);
        }
        std::fs::read_link(format!("/proc/{}/cwd", self.pid)).ok()
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    pub fn resize(&mut self, size: GridSize, cell_width: f32, cell_height: f32) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.term.lock().resize(size);
        self.notifier
            .on_resize(window_size(size, cell_width, cell_height));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Stops the PTY thread, which drops the PTY and hangs up the shell.
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}
