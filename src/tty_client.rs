// src/tty_client.rs
//
// `cyberterm +attach` inside another terminal -- over SSH, on a console,
// or with `--tty`. It attaches to a daemon session like a window does
// (replica terminals fed by snapshot + live output) and draws the active
// tab's panes as text, with dividers and a status line.
//
// Keys go to the focused pane as the local terminal sends them. To make
// the local terminal encode them the way the program in the pane expects,
// the pane's input modes (application cursor keys, bracketed paste, mouse
// reporting, focus events, Kitty keyboard flags) are mirrored onto it.
//
// Commands are a prefix key (Ctrl+\ by default) and a letter, tmux style;
// Ctrl+\ ? lists them.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use alacritty_terminal::Term;
use parking_lot::Mutex;

use crate::app::AttachTarget;
use crate::config::CyberConfig;
use crate::layout::{neighbor, Axis, Direction, Removed, Tab, TabId};
use crate::mux::client::DaemonClient;
use crate::mux::layout_doc;
use crate::mux::protocol::{ClientMsg, Size, TermSettings};
use crate::mux::snapshot::Style;
use crate::renderer::Rect;
use crate::session::{EventProxy, EventSink, GridSize, PaneId, UserEvent};
use crate::shell::ShellState;

/// Ctrl+\ -- rarely used by programs, and the same byte in every terminal.
const PREFIX: u8 = 0x1c;

const HELP: &str = " d detach  % \" split  x close  z zoom  o/arrows pane  c tab  n p 1-9 tabs  & close tab  b broadcast  Ctrl+\\ send it ";

enum Ev {
    Daemon(UserEvent),
    Input(Vec<u8>),
    InputClosed,
}

/// Puts the controlling terminal in raw mode and restores it on drop
/// (including when unwinding from a panic).
struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    fn enter() -> io::Result<Self> {
        // SAFETY: plain termios calls on stdin with a zeroed struct the
        // kernel fills in.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Undo everything we may have turned on, leave the alternate
        // screen, then restore the saved termios.
        let mut out = io::stdout();
        let _ = out.write_all(
            b"\x1b[?1l\x1b>\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\
              \x1b[?1004l\x1b[=0;1u\x1b[0 q\x1b[0m\x1b[?25h\x1b[?1049l",
        );
        let _ = out.flush();
        // SAFETY: restores the struct saved in `enter`.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
    }
}

fn host_size() -> (usize, usize) {
    // SAFETY: TIOCGWINSZ fills a winsize struct.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            (ws.ws_col as usize, ws.ws_row as usize)
        } else {
            (80, 24)
        }
    }
}

struct PaneState {
    term: Arc<FairMutex<Term<EventProxy>>>,
    shell: Arc<Mutex<ShellState>>,
    pid: u32,
    title: String,
}

#[derive(Clone, PartialEq)]
struct OutCell {
    ch: char,
    extra: Option<Box<[char]>>,
    style: Style,
    spacer: bool,
}

impl OutCell {
    fn blank() -> Self {
        Self {
            ch: ' ',
            extra: None,
            style: Style::default_style(),
            spacer: false,
        }
    }

    fn styled(ch: char, style: Style) -> Self {
        Self {
            ch,
            extra: None,
            style,
            spacer: false,
        }
    }
}

/// Input modes mirrored onto the local terminal.
#[derive(Clone, Copy, Default, PartialEq)]
struct HostModes {
    app_cursor: bool,
    app_keypad: bool,
    bracketed: bool,
    focus: bool,
    /// 0 off, else 1000 / 1002 / 1003.
    mouse: u16,
    kitty: u8,
}

impl HostModes {
    fn of(mode: TermMode) -> Self {
        let kitty = [
            (TermMode::DISAMBIGUATE_ESC_CODES, 1),
            (TermMode::REPORT_EVENT_TYPES, 2),
            (TermMode::REPORT_ALTERNATE_KEYS, 4),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
            (TermMode::REPORT_ASSOCIATED_TEXT, 16),
        ]
        .iter()
        .filter(|(m, _)| mode.contains(*m))
        .map(|(_, b)| b)
        .sum();
        Self {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            bracketed: mode.contains(TermMode::BRACKETED_PASTE),
            focus: mode.contains(TermMode::FOCUS_IN_OUT),
            mouse: if mode.contains(TermMode::MOUSE_MOTION) {
                1003
            } else if mode.contains(TermMode::MOUSE_DRAG) {
                1002
            } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
                1000
            } else {
                0
            },
            kitty,
        }
    }

    /// Escape sequences turning `prev` into `self`.
    fn diff(&self, prev: &HostModes, out: &mut Vec<u8>) {
        let mut flag = |on: bool, was: bool, set: &[u8], reset: &[u8]| {
            if on != was {
                out.extend_from_slice(if on { set } else { reset });
            }
        };
        flag(self.app_cursor, prev.app_cursor, b"\x1b[?1h", b"\x1b[?1l");
        flag(self.app_keypad, prev.app_keypad, b"\x1b=", b"\x1b>");
        flag(
            self.bracketed,
            prev.bracketed,
            b"\x1b[?2004h",
            b"\x1b[?2004l",
        );
        flag(self.focus, prev.focus, b"\x1b[?1004h", b"\x1b[?1004l");
        if self.mouse != prev.mouse {
            if prev.mouse != 0 {
                out.extend_from_slice(format!("\x1b[?{}l\x1b[?1006l", prev.mouse).as_bytes());
            }
            if self.mouse != 0 {
                // Always SGR from the local terminal; coordinates are
                // translated to the pane before forwarding.
                out.extend_from_slice(format!("\x1b[?{}h\x1b[?1006h", self.mouse).as_bytes());
            }
        }
        if self.kitty != prev.kitty {
            out.extend_from_slice(format!("\x1b[={};1u", self.kitty).as_bytes());
        }
    }
}

/// A key typed after the prefix.
#[derive(Debug, PartialEq)]
enum Key {
    Char(char),
    Byte(u8),
    Arrow(Direction),
    Other,
}

/// Parses one key at the start of `bytes`; returns it and its length.
fn parse_key(bytes: &[u8]) -> (Key, usize) {
    match bytes {
        [0x1b, b'[', rest @ ..] | [0x1b, b'O', rest @ ..] => {
            // A CSI/SS3 sequence: parameters, then a final byte.
            let end = rest.iter().position(|b| (0x40..=0x7e).contains(b));
            match end {
                Some(e) => {
                    let key = match rest[e] {
                        b'A' => Key::Arrow(Direction::Up),
                        b'B' => Key::Arrow(Direction::Down),
                        b'C' => Key::Arrow(Direction::Right),
                        b'D' => Key::Arrow(Direction::Left),
                        _ => Key::Other,
                    };
                    (key, 2 + e + 1)
                }
                None => (Key::Other, bytes.len()),
            }
        }
        [b, ..] if b.is_ascii() && !b.is_ascii_control() => (Key::Char(*b as char), 1),
        [b, ..] => (Key::Byte(*b), 1),
        [] => (Key::Other, 0),
    }
}

/// A complete SGR mouse report at the start of `bytes`:
/// (button code, column, row, press), 0-based coordinates, and its length.
fn parse_sgr_mouse(bytes: &[u8]) -> Option<((u32, usize, usize, bool), usize)> {
    let body = bytes.strip_prefix(b"\x1b[<")?;
    let end = body.iter().position(|b| *b == b'M' || *b == b'm')?;
    let text = std::str::from_utf8(&body[..end]).ok()?;
    let mut parts = text.split(';').map(|p| p.parse::<usize>().ok());
    let (b, x, y) = (parts.next()??, parts.next()??, parts.next()??);
    Some((
        (b as u32, x.max(1) - 1, y.max(1) - 1, body[end] == b'M'),
        3 + end + 1,
    ))
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

struct Client {
    daemon: Arc<DaemonClient>,
    session: String,
    settings: TermSettings,
    program: Option<String>,
    args: Vec<String>,
    tabs: Vec<Tab>,
    active: usize,
    next_tab: TabId,
    panes: HashMap<PaneId, PaneState>,
    cols: usize,
    rows: usize,
    prev: Vec<Vec<OutCell>>,
    host_modes: HostModes,
    host_title: String,
    prefix_pending: bool,
    help: bool,
    out: Vec<u8>,
    /// Set to end the client, with the message to print afterwards.
    done: Option<String>,
}

/// Attaches inside the current terminal until detached or the session ends.
pub fn run(target: AttachTarget, config: &CyberConfig) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<Ev>();
    let sink: EventSink = {
        let tx = tx.clone();
        Arc::new(move |e| {
            let _ = tx.send(Ev::Daemon(e));
        })
    };
    let daemon = DaemonClient::connect_or_start(sink)?;
    let (session, layout, infos) = match daemon.attach(target.name.clone(), target.force) {
        Ok(found) => found,
        // Like `tmux new -A`: no detached session to resume means start one.
        Err(_) if target.name.is_none() => daemon.new_session(None)?,
        Err(e) => return Err(e),
    };

    let mut client = Client {
        daemon,
        session,
        settings: crate::mux::settings_from_config(config),
        program: config.shell.program.clone(),
        args: config.shell.args.clone(),
        tabs: Vec::new(),
        active: 0,
        next_tab: 0,
        panes: HashMap::new(),
        cols: 0,
        rows: 0,
        prev: Vec::new(),
        host_modes: HostModes::default(),
        host_title: String::new(),
        prefix_pending: false,
        help: false,
        out: Vec::new(),
        done: None,
    };
    for info in &infos {
        client.adopt(info.id, info.title.clone());
    }
    let ids: Vec<PaneId> = infos.iter().map(|i| i.id).collect();
    let (tabs, active) = layout_doc::decode(&layout, &ids, &mut client.next_tab);
    client.tabs = tabs;
    client.active = active;

    let raw = RawMode::enter()?;
    io::stdout().write_all(b"\x1b[?1049h\x1b[?25l\x1b[H\x1b[2J")?;
    std::thread::Builder::new()
        .name("tty input".into())
        .spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Ev::InputClosed);
                        return;
                    }
                    Ok(n) => {
                        if tx.send(Ev::Input(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
        })?;

    if client.tabs.is_empty() {
        client.new_tab();
    }
    client.fit();
    client.render();
    client.event_loop(rx);

    let message = client.done.take();
    drop(raw);
    if let Some(message) = message {
        println!("{message}");
    }
    Ok(())
}

impl Client {
    fn adopt(&mut self, id: PaneId, title: String) {
        if let Some((term, shell, pid, _)) = self.daemon.replica(id) {
            self.panes.insert(
                id,
                PaneState {
                    term,
                    shell,
                    pid,
                    title,
                },
            );
        }
    }

    fn event_loop(&mut self, rx: Receiver<Ev>) {
        while self.done.is_none() {
            let first = match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let mut dirty = false;
            // Handle everything already queued, then draw once.
            let started = Instant::now();
            let mut next = first;
            while let Some(ev) = next {
                dirty |= self.handle(ev);
                if self.done.is_some() || started.elapsed() > Duration::from_millis(16) {
                    break;
                }
                next = rx.try_recv().ok();
            }
            if (self.cols, self.rows) != host_size() {
                self.fit();
                dirty = true;
            }
            if dirty && self.done.is_none() {
                self.render();
            }
        }
    }

    /// Returns whether the screen needs redrawing.
    fn handle(&mut self, ev: Ev) -> bool {
        match ev {
            Ev::Input(bytes) => {
                self.input(&bytes);
                true
            }
            Ev::InputClosed => {
                self.detach("input closed");
                false
            }
            Ev::Daemon(UserEvent::DaemonLost(reason)) => {
                self.done = Some(format!("[cyberterm: {reason}]"));
                false
            }
            Ev::Daemon(UserEvent::Term(pane, event)) => match event {
                TermEvent::Wakeup => true,
                TermEvent::Title(title) => {
                    if let Some(p) = self.panes.get_mut(&pane) {
                        p.title = title;
                    }
                    true
                }
                TermEvent::ResetTitle => {
                    if let Some(p) = self.panes.get_mut(&pane) {
                        p.title.clear();
                    }
                    true
                }
                TermEvent::Bell => {
                    self.out.push(0x07);
                    self.flush();
                    false
                }
                TermEvent::ClipboardStore(_, text) => {
                    // Hand copies to the local terminal (works over SSH).
                    self.out.extend_from_slice(
                        format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).as_bytes(),
                    );
                    self.flush();
                    false
                }
                TermEvent::Exit | TermEvent::ChildExit(_) => {
                    self.pane_gone(pane);
                    true
                }
                _ => false,
            },
            Ev::Daemon(_) => false,
        }
    }

    // ------------------------------------------------------------------
    // Layout
    // ------------------------------------------------------------------

    fn area(&self) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: self.cols as f32,
            h: self.rows.saturating_sub(1).max(1) as f32,
        }
    }

    fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    fn focused(&self) -> Option<PaneId> {
        self.tab().map(|t| t.focused)
    }

    /// Resizes every pane to its place in the current terminal size.
    fn fit(&mut self) {
        let (cols, rows) = host_size();
        if (cols, rows) != (self.cols, self.rows) {
            self.prev.clear();
        }
        self.cols = cols;
        self.rows = rows;
        let area = self.area();
        for tab in &self.tabs {
            for (id, r) in tab.visible(area, 1.0) {
                let Some(p) = self.panes.get(&id) else {
                    continue;
                };
                let size = GridSize {
                    cols: (r.w as usize).max(2),
                    rows: (r.h as usize).max(1),
                };
                let mut term = p.term.lock();
                if term.columns() != size.cols || term.screen_lines() != size.rows {
                    term.resize(size);
                    drop(term);
                    let _ = self.daemon.send(&ClientMsg::Resize {
                        pane: id,
                        size: Size {
                            cols: size.cols as u16,
                            rows: size.rows as u16,
                            cell_width: 0,
                            cell_height: 0,
                        },
                    });
                }
            }
        }
    }

    fn save_layout(&self) {
        let _ = self.daemon.send(&ClientMsg::SaveLayout {
            layout: layout_doc::encode(&self.tabs, self.active),
        });
    }

    fn spawn(&mut self, cwd: Option<PathBuf>) -> Option<PaneId> {
        let area = self.area();
        let mut env = HashMap::new();
        env.insert("TERM_PROGRAM".to_string(), "cyberterm".to_string());
        let (program, args, settings) = (
            self.program.clone(),
            self.args.clone(),
            self.settings.clone(),
        );
        let info = self
            .daemon
            .spawn(|req| ClientMsg::Spawn {
                req,
                size: Size {
                    cols: area.w as u16,
                    rows: area.h as u16,
                    cell_width: 0,
                    cell_height: 0,
                },
                settings,
                program,
                args,
                cwd,
                env,
            })
            .ok()?;
        self.adopt(info.id, String::new());
        Some(info.id)
    }

    fn focused_cwd(&self) -> Option<PathBuf> {
        let p = self.panes.get(&self.focused()?)?;
        std::fs::read_link(format!("/proc/{}/cwd", p.pid))
            .ok()
            .or_else(|| p.shell.lock().cwd.clone())
    }

    fn new_tab(&mut self) {
        let cwd = self.focused_cwd();
        if let Some(id) = self.spawn(cwd) {
            self.tabs.push(Tab::new(self.next_tab, id));
            self.next_tab += 1;
            self.active = self.tabs.len() - 1;
            self.fit();
            self.save_layout();
        }
    }

    fn split(&mut self, axis: Axis) {
        let cwd = self.focused_cwd();
        let Some(target) = self.focused() else { return };
        if let Some(id) = self.spawn(cwd) {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.zoomed = false;
                tab.root.split(target, axis, id, false);
                tab.focused = id;
            }
            self.fit();
            self.save_layout();
        }
    }

    fn close_pane(&mut self) {
        if let Some(id) = self.focused() {
            let _ = self.daemon.send(&ClientMsg::Kill { pane: id });
            self.pane_gone(id);
        }
    }

    fn close_tab(&mut self) {
        let Some(tab) = self.tab() else { return };
        for id in tab.root.panes() {
            let _ = self.daemon.send(&ClientMsg::Kill { pane: id });
            self.pane_gone(id);
        }
    }

    fn pane_gone(&mut self, id: PaneId) {
        if self.panes.remove(&id).is_none() {
            return;
        }
        self.daemon.forget(id);
        if let Some(index) = self.tabs.iter().position(|t| t.root.contains(id)) {
            let tab = &mut self.tabs[index];
            match tab.root.remove(id) {
                Removed::Empty => {
                    self.tabs.remove(index);
                    if self.active >= self.tabs.len() {
                        self.active = self.tabs.len().saturating_sub(1);
                    }
                }
                _ => {
                    if tab.focused == id {
                        tab.focused = tab.root.panes()[0];
                    }
                    if tab.root.panes().len() == 1 {
                        tab.zoomed = false;
                    }
                }
            }
        }
        if self.tabs.is_empty() {
            self.done = Some(format!("[cyberterm: session {} ended]", self.session));
            return;
        }
        self.fit();
        self.save_layout();
    }

    fn focus_direction(&mut self, dir: Direction) {
        let area = self.area();
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        tab.zoomed = false;
        let rects = tab.root.layout(area, 1.0);
        if let Some(next) = neighbor(&rects, tab.focused, dir) {
            tab.focused = next;
        }
        self.fit();
        self.save_layout();
    }

    fn focus_cycle(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let order = tab.root.panes();
        if let Some(at) = order.iter().position(|p| *p == tab.focused) {
            tab.focused = order[(at + 1) % order.len()];
        }
        tab.zoomed = false;
        self.fit();
        self.save_layout();
    }

    fn goto_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.fit();
            self.save_layout();
        }
    }

    fn detach(&mut self, why: &str) {
        self.save_layout();
        let _ = self.daemon.send(&ClientMsg::Detach);
        self.done = Some(format!(
            "[cyberterm: detached from session {} ({why})]",
            self.session
        ));
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    fn input(&mut self, bytes: &[u8]) {
        let mut forward: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < bytes.len() && self.done.is_none() {
            if self.prefix_pending {
                self.send(std::mem::take(&mut forward));
                let (key, used) = parse_key(&bytes[i..]);
                i += used.max(1);
                self.prefix_pending = false;
                self.command(key);
                continue;
            }
            if bytes[i] == PREFIX {
                self.send(std::mem::take(&mut forward));
                self.prefix_pending = true;
                i += 1;
                continue;
            }
            if let Some((report, used)) = parse_sgr_mouse(&bytes[i..]) {
                self.send(std::mem::take(&mut forward));
                self.mouse(report);
                i += used;
                continue;
            }
            forward.push(bytes[i]);
            i += 1;
        }
        self.send(forward);
    }

    /// Writes typed bytes to the focused pane (every pane while
    /// broadcasting).
    fn send(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let Some(tab) = self.tab() else { return };
        let targets = if tab.broadcast {
            tab.root.panes()
        } else {
            vec![tab.focused]
        };
        for id in targets {
            if let Some(p) = self.panes.get(&id) {
                p.term
                    .lock()
                    .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
            }
            self.daemon.send_input(id, bytes.clone());
        }
    }

    fn command(&mut self, key: Key) {
        self.help = false;
        match key {
            Key::Char('d') => self.detach("Ctrl+\\ d"),
            Key::Char('?') => self.help = true,
            Key::Char('%') => self.split(Axis::Horizontal),
            Key::Char('"') => self.split(Axis::Vertical),
            Key::Char('x') => self.close_pane(),
            Key::Char('&') => self.close_tab(),
            Key::Char('c') => self.new_tab(),
            Key::Char('o') => self.focus_cycle(),
            Key::Char('n') => self.goto_tab((self.active + 1) % self.tabs.len().max(1)),
            Key::Char('p') => {
                let n = self.tabs.len().max(1);
                self.goto_tab((self.active + n - 1) % n);
            }
            Key::Char(c @ '1'..='9') => self.goto_tab(c as usize - '1' as usize),
            Key::Char('z') => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    if tab.root.panes().len() > 1 {
                        tab.zoomed = !tab.zoomed;
                    }
                }
                self.fit();
                self.save_layout();
            }
            Key::Char('b') => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.broadcast = !tab.broadcast;
                }
            }
            Key::Arrow(dir) => self.focus_direction(dir),
            Key::Byte(PREFIX) => self.send(vec![PREFIX]),
            _ => {}
        }
    }

    /// Clicks focus the pane under them; reports go to panes that asked for
    /// them, translated to the pane's own coordinates.
    fn mouse(&mut self, (code, x, y, press): (u32, usize, usize, bool)) {
        let area = self.area();
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let rects = tab.visible(area, 1.0);
        let Some((id, r)) = rects.into_iter().find(|(_, r)| {
            (x as f32) >= r.x
                && (x as f32) < r.x + r.w
                && (y as f32) >= r.y
                && (y as f32) < r.y + r.h
        }) else {
            return;
        };
        let is_click = press && code & 0b1110_0000 == 0;
        if id != tab.focused && is_click {
            tab.focused = id;
            self.save_layout();
            return;
        }
        let Some(p) = self.panes.get(&id) else { return };
        if !p.term.lock().mode().contains(TermMode::SGR_MOUSE) {
            return;
        }
        let (lx, ly) = (x - r.x as usize + 1, y - r.y as usize + 1);
        let fin = if press { 'M' } else { 'm' };
        self.daemon
            .send_input(id, format!("\x1b[<{code};{lx};{ly}{fin}").into_bytes());
    }

    // ------------------------------------------------------------------
    // Output
    // ------------------------------------------------------------------

    fn flush(&mut self) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(&self.out);
        let _ = out.flush();
        self.out.clear();
    }

    fn render(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        let mut screen = vec![vec![OutCell::blank(); cols]; rows];
        let area = self.area();
        let mut cursor: Option<(usize, usize, CursorShape, bool)> = None;

        if let Some(tab) = self.tab() {
            for (id, r) in tab.visible(area, 1.0) {
                let Some(p) = self.panes.get(&id) else {
                    continue;
                };
                let term = p.term.lock();
                let grid = term.grid();
                let offset = grid.display_offset() as i32;
                let (px, py) = (r.x as usize, r.y as usize);
                for row in 0..(r.h as usize).min(term.screen_lines()) {
                    let line = &grid[Line(row as i32 - offset)];
                    for col in 0..(r.w as usize).min(term.columns()) {
                        let (sx, sy) = (px + col, py + row);
                        if sx >= cols || sy >= rows {
                            continue;
                        }
                        let cell = &line[Column(col)];
                        screen[sy][sx] = OutCell {
                            ch: cell.c,
                            extra: cell.zerowidth().filter(|z| !z.is_empty()).map(Box::from),
                            style: Style::of(cell),
                            spacer: cell.flags.intersects(
                                Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
                            ),
                        };
                    }
                }
                if id == tab.focused {
                    let content = term.renderable_content();
                    let point = content.cursor.point;
                    let row = point.line.0 + offset;
                    if row >= 0
                        && (row as usize) < r.h as usize
                        && content.cursor.shape != CursorShape::Hidden
                    {
                        cursor = Some((
                            px + point.column.0,
                            py + row as usize,
                            content.cursor.shape,
                            term.cursor_style().blinking,
                        ));
                    }
                }
            }

            // Dividers between splits.
            if !tab.zoomed {
                let dim = Style {
                    fg: Color::Named(NamedColor::BrightBlack),
                    ..Style::default_style()
                };
                for d in tab.root.dividers(area, 1.0) {
                    let r = d.rect;
                    let (y0, x0) = (r.y as usize, r.x as usize);
                    for line in screen.iter_mut().skip(y0).take(r.h as usize) {
                        for cell in line.iter_mut().skip(x0).take(r.w as usize) {
                            let ch = match (d.axis, cell.ch) {
                                (Axis::Horizontal, '─') | (Axis::Vertical, '│') => '┼',
                                (Axis::Horizontal, _) => '│',
                                (Axis::Vertical, _) => '─',
                            };
                            *cell = OutCell::styled(ch, dim.clone());
                        }
                    }
                }
            }
        }

        // Status line.
        if rows > 0 {
            let bar = Style {
                flags: Flags::INVERSE,
                ..Style::default_style()
            };
            let active = Style {
                flags: Flags::BOLD,
                fg: Color::Named(NamedColor::Black),
                bg: Color::Named(NamedColor::Green),
                underline: None,
            };
            let mut status: Vec<OutCell> = Vec::new();
            let mut push = |text: &str, style: &Style| {
                for ch in text.chars() {
                    status.push(OutCell::styled(ch, style.clone()));
                }
            };
            if self.help {
                push(HELP, &bar);
            } else {
                push(&format!(" [{}] ", self.session), &bar);
                for (i, tab) in self.tabs.iter().enumerate() {
                    let title = tab
                        .title
                        .clone()
                        .or_else(|| {
                            self.panes
                                .get(&tab.focused)
                                .map(|p| p.title.clone())
                                .filter(|t| !t.is_empty())
                        })
                        .unwrap_or_else(|| "shell".into());
                    let mut label =
                        format!(" {} {} ", i + 1, title.chars().take(20).collect::<String>());
                    if tab.zoomed {
                        label.push_str("(z) ");
                    }
                    if tab.broadcast {
                        label.push_str("(b) ");
                    }
                    push(&label, if i == self.active { &active } else { &bar });
                }
                if self.prefix_pending {
                    push(" PREFIX ", &active);
                }
                push(" Ctrl+\\ ? help ", &bar);
            }
            for (x, cell) in screen[rows - 1].iter_mut().enumerate() {
                *cell = status
                    .get(x)
                    .cloned()
                    .unwrap_or_else(|| OutCell::styled(' ', bar.clone()));
            }
        }

        // Redraw changed rows.
        self.out.extend_from_slice(b"\x1b[?25l");
        for (y, row) in screen.iter().enumerate() {
            if self.prev.get(y) == Some(row) {
                continue;
            }
            self.out
                .extend_from_slice(format!("\x1b[{};1H\x1b[0m", y + 1).as_bytes());
            let mut style = Style::default_style();
            let mut buf = [0u8; 4];
            for cell in row {
                if cell.spacer {
                    continue;
                }
                if cell.style != style {
                    cell.style.sgr(&mut self.out);
                    style = cell.style.clone();
                }
                self.out
                    .extend_from_slice(cell.ch.encode_utf8(&mut buf).as_bytes());
                if let Some(extra) = &cell.extra {
                    for ch in extra.iter() {
                        self.out
                            .extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            self.out.extend_from_slice(b"\x1b[0m");
        }
        self.prev = screen;

        // Mirror the focused pane's input modes and title, place the cursor.
        let focused = self.focused().and_then(|id| self.panes.get(&id));
        if let Some(p) = focused {
            let modes = HostModes::of(*p.term.lock().mode());
            modes.diff(&self.host_modes, &mut self.out);
            self.host_modes = modes;
            let title = if p.title.is_empty() {
                format!("cyberterm: {}", self.session)
            } else {
                format!("{} - cyberterm: {}", p.title, self.session)
            };
            if title != self.host_title {
                self.out
                    .extend_from_slice(format!("\x1b]2;{title}\x07").as_bytes());
                self.host_title = title;
            }
        }
        if let Some((x, y, shape, blinking)) = cursor {
            let code = match shape {
                CursorShape::Beam => 5,
                CursorShape::Underline => 3,
                _ => 1,
            } + u8::from(!blinking);
            self.out.extend_from_slice(
                format!("\x1b[{code} q\x1b[{};{}H\x1b[?25h", y + 1, x + 1).as_bytes(),
            );
        }
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_after_the_prefix() {
        assert_eq!(parse_key(b"d"), (Key::Char('d'), 1));
        assert_eq!(parse_key(b"\x1b[A"), (Key::Arrow(Direction::Up), 3));
        assert_eq!(parse_key(b"\x1bOD"), (Key::Arrow(Direction::Left), 3));
        assert_eq!(parse_key(b"\x1b[1;5C"), (Key::Arrow(Direction::Right), 6));
        assert_eq!(parse_key(&[PREFIX]), (Key::Byte(PREFIX), 1));
    }

    #[test]
    fn sgr_mouse_reports() {
        assert_eq!(
            parse_sgr_mouse(b"\x1b[<0;10;5Mrest"),
            Some(((0, 9, 4, true), 10))
        );
        assert_eq!(
            parse_sgr_mouse(b"\x1b[<64;1;1m"),
            Some(((64, 0, 0, false), 10))
        );
        assert_eq!(parse_sgr_mouse(b"\x1b[<0;10"), None);
        assert_eq!(parse_sgr_mouse(b"abc"), None);
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo wörld".as_bytes()), "aMOpbGxvIHfDtnJsZA==");
    }

    #[test]
    fn host_modes_diff_only_changes() {
        let a = HostModes::default();
        let b = HostModes {
            app_cursor: true,
            mouse: 1002,
            kitty: 1,
            ..a
        };
        let mut out = Vec::new();
        b.diff(&a, &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[?1h"));
        assert!(s.contains("\x1b[?1002h\x1b[?1006h"));
        assert!(s.contains("\x1b[=1;1u"));
        let mut out = Vec::new();
        b.diff(&b, &mut out);
        assert!(out.is_empty());
        let mut out = Vec::new();
        a.diff(&b, &mut out);
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("\x1b[?1002l\x1b[?1006l"));
    }
}
