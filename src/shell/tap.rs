// src/shell/tap.rs
//
// A byte-stream filter between the PTY and alacritty_terminal's parser for
// the shell-integration escapes alacritty_terminal doesn't handle itself:
//
// - OSC 7 `file://host/path`: the shell's working directory. Recorded in
//   `ShellState` and swallowed.
// - OSC 133 (FinalTerm semantic prompts): `A` prompt start, `B` prompt end,
//   `C` command output start, `D;<exit>` command finished.
//
// Prompt positions have to live *in the grid*: they scroll into history,
// reflow on resize and fall off the end of the scrollback with the text
// they belong to. alacritty_terminal cells can't carry custom data, but they
// do carry OSC 8 hyperlinks. So `133;A` is rewritten into an OSC 8 open with
// a private `cyberterm-mark:` URI and `133;B`/`133;C` into the matching close.
// Every prompt cell then carries the mark, and the renderer ignores that
// scheme instead of drawing it as a link. Doing it as a stream rewrite also
// means alacritty_terminal's own PTY event loop is used unmodified.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite};
use parking_lot::Mutex;
use polling::{Event, PollMode, Poller};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// URI scheme of the hidden prompt marks.
pub const MARK_SCHEME: &str = "cyberterm-mark:";

/// What the shell has told us about itself through OSC 7/133.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellState {
    pub cwd: Option<PathBuf>,
    /// The directory a shell on another machine reported (OSC 7 with a
    /// foreign host -- a remote shell with integration, over SSH), as
    /// (host, path). Cleared when the local shell reports again.
    pub remote_cwd: Option<(String, PathBuf)>,
    pub last_exit: Option<i32>,
    pub command_running: bool,
    /// Prompts drawn so far; non-zero means shell integration is active.
    pub prompts: u64,
    /// One entry per prompt (newest last, capped), keyed by the id of the
    /// prompt's mark on the grid. Prompts that never ran a command (an
    /// empty Enter, a redraw) have no `command`.
    pub blocks: VecDeque<BlockMeta>,
    /// The latest desktop notification a program asked for (OSC 9 /
    /// OSC 777;notify), as (ms since the epoch, text). Coding agents send
    /// these when they need you.
    pub notice: Option<(u64, String)>,
}

/// What shell integration said about one command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockMeta {
    /// The prompt mark's id (`cyberterm-prompt-<mark>`).
    pub mark: u64,
    pub command: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Milliseconds since the Unix epoch.
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub exit: Option<i32>,
    /// Saved to the history database already.
    pub recorded: bool,
}

impl BlockMeta {
    pub fn is_command(&self) -> bool {
        self.command
            .as_deref()
            .is_some_and(|c| !c.trim().is_empty())
    }

    pub fn running(&self) -> bool {
        self.started_ms.is_some() && self.finished_ms.is_none()
    }

    /// How long it ran (or has been running).
    pub fn duration_ms(&self) -> Option<u64> {
        let start = self.started_ms?;
        Some(
            self.finished_ms
                .unwrap_or_else(now_ms)
                .saturating_sub(start),
        )
    }
}

/// Blocks remembered per pane.
const MAX_BLOCKS: usize = 1000;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Longest OSC payload buffered while deciding/collecting; anything longer
/// is passed through untouched.
const MAX_OSC: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    /// Saw ESC.
    Esc,
    /// Saw `ESC ]`, collecting until the payload prefix decides whether
    /// this is one of ours.
    Prefix,
    /// Collecting an OSC 7/133 payload.
    Intercept,
    /// Saw ESC inside an intercepted payload (expecting `\`).
    InterceptEsc,
}

pub struct OscTap {
    state: State,
    held: Vec<u8>,
    payload: Vec<u8>,
    next_mark: u64,
    shell: Arc<Mutex<ShellState>>,
}

const OURS: [&[u8]; 4] = [b"133;", b"7;", b"9;", b"777;"];

impl OscTap {
    pub fn new(shell: Arc<Mutex<ShellState>>) -> Self {
        Self {
            state: State::Ground,
            held: Vec::new(),
            payload: Vec::new(),
            next_mark: 0,
            shell,
        }
    }

    /// Filters `input`, appending what the terminal parser should see to
    /// `out`. Sequences split across calls are handled.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &byte in input {
            self.step(byte, out);
        }
    }

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => {
                if byte == 0x1b {
                    self.held.push(byte);
                    self.state = State::Esc;
                } else {
                    out.push(byte);
                }
            }
            State::Esc => {
                if byte == b']' {
                    self.held.push(byte);
                    self.state = State::Prefix;
                } else {
                    self.flush(out);
                    self.step(byte, out);
                }
            }
            State::Prefix => {
                if byte == 0x07 || byte == 0x1b {
                    // Terminated (or aborted) before it could be ours.
                    self.flush(out);
                    self.step(byte, out);
                    return;
                }
                self.held.push(byte);
                let payload = &self.held[2..];
                if OURS.contains(&payload) {
                    self.payload.clear();
                    self.payload.extend_from_slice(payload);
                    self.held.clear();
                    self.state = State::Intercept;
                } else if !OURS.iter().any(|p| p.starts_with(payload)) {
                    self.flush(out);
                }
            }
            State::Intercept => match byte {
                0x07 => self.finish(out),
                0x1b => self.state = State::InterceptEsc,
                _ if self.payload.len() >= MAX_OSC => {
                    // Not a real shell-integration sequence; give up and
                    // pass everything through as it arrived.
                    out.extend_from_slice(b"\x1b]");
                    out.append(&mut self.payload);
                    out.push(byte);
                    self.state = State::Ground;
                }
                _ => self.payload.push(byte),
            },
            State::InterceptEsc => {
                self.finish(out);
                if byte != b'\\' {
                    // ESC + anything else ends the OSC and starts a new
                    // sequence, as in xterm.
                    self.step(0x1b, out);
                    self.step(byte, out);
                }
            }
        }
    }

    fn flush(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
        self.state = State::Ground;
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        self.state = State::Ground;
        let payload = std::mem::take(&mut self.payload);
        let text = String::from_utf8_lossy(&payload);
        let mut shell = self.shell.lock();

        if let Some(url) = text.strip_prefix("7;") {
            if let Some((host, path)) = parse_file_url(url) {
                if is_local_host(&host) {
                    shell.cwd = Some(path);
                    shell.remote_cwd = None;
                } else {
                    shell.remote_cwd = Some((host, path));
                }
            }
            return;
        }

        // Desktop notifications: OSC 9;<text> (iTerm2; ConEmu uses
        // `9;<digit>;...` for other things) and OSC 777;notify;<title>;<body>.
        let notice = match text.strip_prefix("9;") {
            Some(t) if !t.starts_with(|c: char| c.is_ascii_digit()) => Some(t.to_string()),
            Some(_) => return,
            None => match text.strip_prefix("777;") {
                Some(rest) => {
                    let mut parts = rest.splitn(3, ';');
                    if parts.next() != Some("notify") {
                        return;
                    }
                    let title = parts.next().unwrap_or_default();
                    let body = parts.next().unwrap_or_default();
                    Some(match (title.is_empty(), body.is_empty()) {
                        (false, false) => format!("{title}: {body}"),
                        (true, _) => body.to_string(),
                        (_, true) => title.to_string(),
                    })
                }
                None => None,
            },
        };
        if let Some(notice) = notice {
            let notice: String = notice.chars().take(300).collect();
            shell.notice = Some((now_ms(), notice));
            return;
        }

        let Some(rest) = text.strip_prefix("133;") else {
            return;
        };
        let mut fields = rest.split(';');
        match fields.next().unwrap_or_default() {
            "A" => {
                shell.prompts += 1;
                shell.command_running = false;
                self.next_mark += 1;
                let cwd = shell.cwd.clone();
                shell.blocks.push_back(BlockMeta {
                    mark: self.next_mark,
                    cwd,
                    ..BlockMeta::default()
                });
                while shell.blocks.len() > MAX_BLOCKS {
                    shell.blocks.pop_front();
                }
                let exit = shell.last_exit.map(|e| e.to_string()).unwrap_or_default();
                out.extend_from_slice(
                    format!(
                        "\x1b]8;id=cyberterm-prompt-{};{MARK_SCHEME}prompt?exit={exit}\x1b\\",
                        self.next_mark
                    )
                    .as_bytes(),
                );
            }
            "B" => out.extend_from_slice(b"\x1b]8;;\x1b\\"),
            "C" => {
                shell.command_running = true;
                out.extend_from_slice(b"\x1b]8;;\x1b\\");
                // `cmdline_url=` (ours, percent-encoded) or kitty's
                // `cmdline=`.
                let command = fields.find_map(|f| {
                    f.strip_prefix("cmdline_url=")
                        .map(percent_decode)
                        .or_else(|| f.strip_prefix("cmdline=").map(str::to_string))
                });
                let cwd = shell.cwd.clone();
                let mark = self.next_mark;
                if let Some(block) = shell.blocks.back_mut().filter(|b| b.mark == mark) {
                    block.command = command.filter(|c| !c.trim().is_empty());
                    block.started_ms = Some(now_ms());
                    if cwd.is_some() {
                        block.cwd = cwd;
                    }
                }
            }
            "D" => {
                shell.command_running = false;
                shell.last_exit = fields.next().and_then(|f| f.trim().parse().ok());
                let (mark, exit) = (self.next_mark, shell.last_exit);
                if let Some(block) = shell
                    .blocks
                    .back_mut()
                    .filter(|b| b.mark == mark && b.started_ms.is_some() && b.finished_ms.is_none())
                {
                    block.finished_ms = Some(now_ms());
                    block.exit = exit;
                }
            }
            _ => {}
        }
    }
}

/// `file://host/some%20path` -> (`host`, `/some path`).
fn parse_file_url(url: &str) -> Option<(String, PathBuf)> {
    let rest = url.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let host = percent_decode(&rest[..slash]);
    Some((host, PathBuf::from(percent_decode(&rest[slash..]))))
}

/// An OSC 7 host that means this machine: empty, localhost, or our
/// hostname (with or without its domain).
fn is_local_host(host: &str) -> bool {
    use std::sync::OnceLock;
    static NAME: OnceLock<String> = OnceLock::new();
    let name = NAME.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_lowercase())
            .unwrap_or_default()
    });
    let host = host.to_lowercase();
    let short = |h: &str| h.split('.').next().unwrap_or_default().to_string();
    host.is_empty()
        || host == "localhost"
        || host == *name
        || (!name.is_empty() && short(&host) == short(name))
}

fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The real PTY, with reads passed through an `OscTap`. Implements the same
/// traits as `tty::Pty` so alacritty_terminal's `EventLoop` drives it as is.
pub struct TappedPty {
    inner: tty::Pty,
    reader: TapReader,
}

impl TappedPty {
    pub fn new(inner: tty::Pty, shell: Arc<Mutex<ShellState>>) -> io::Result<Self> {
        // A second descriptor for the same open PTY master: same
        // (non-blocking) file description, so reads behave identically.
        let file = inner.file().try_clone()?;
        Ok(Self {
            inner,
            reader: TapReader {
                file,
                tap: OscTap::new(shell),
                raw: vec![0; 64 * 1024],
                pending: Vec::new(),
                pos: 0,
                recorder: None,
                graphics: None,
                stage: Vec::new(),
            },
        })
    }

    /// Also decodes inline images (kitty graphics protocol), answering the
    /// program through the PTY.
    pub fn with_graphics(
        mut self,
        images: Arc<Mutex<crate::graphics::Images>>,
    ) -> io::Result<Self> {
        use std::io::Write as _;
        let mut pty = self.inner.file().try_clone()?;
        let respond: crate::graphics::Responder = Box::new(move |bytes: &[u8]| {
            let _ = pty.write_all(bytes);
        });
        self.reader.graphics = Some(crate::graphics::GraphicsTap::new(
            Some(images),
            true,
            Some(respond),
        ));
        Ok(self)
    }

    /// Only the DA1 answer (the daemon: its panes' images are decoded by
    /// the windows, but DA1 must be answered once, where the PTY is).
    pub fn with_da1_answer(mut self) -> io::Result<Self> {
        use std::io::Write as _;
        let mut pty = self.inner.file().try_clone()?;
        let respond: crate::graphics::Responder = Box::new(move |bytes: &[u8]| {
            let _ = pty.write_all(bytes);
        });
        self.reader.graphics = Some(crate::graphics::GraphicsTap::new(None, true, Some(respond)));
        Ok(self)
    }

    /// Also records everything read (after the tap) for rewind.
    pub fn with_recorder(mut self, recorder: Arc<Mutex<crate::rewind::Recorder>>) -> Self {
        self.reader.recorder = Some(recorder);
        self
    }
}

pub struct TapReader {
    file: File,
    tap: OscTap,
    raw: Vec<u8>,
    pending: Vec<u8>,
    pos: usize,
    recorder: Option<Arc<Mutex<crate::rewind::Recorder>>>,
    graphics: Option<crate::graphics::GraphicsTap>,
    /// Between the OSC tap and the graphics tap.
    stage: Vec<u8>,
}

impl Read for TapReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.pending.len() {
            let n = self.file.read(&mut self.raw)?;
            if n == 0 {
                return Ok(0);
            }
            self.pending.clear();
            self.pos = 0;
            match &mut self.graphics {
                Some(graphics) => {
                    self.stage.clear();
                    self.tap.feed(&self.raw[..n], &mut self.stage);
                    graphics.feed(&self.stage, &mut self.pending);
                }
                None => self.tap.feed(&self.raw[..n], &mut self.pending),
            }
            if let Some(rec) = &self.recorder {
                rec.lock().feed(&self.pending);
            }
            // A read that was nothing but swallowed escapes loops for more
            // instead of returning Ok(0), which callers treat as EOF.
        }
        let n = buf.len().min(self.pending.len() - self.pos);
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl EventedReadWrite for TappedPty {
    type Reader = TapReader;
    type Writer = File;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        event: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        // SAFETY: forwarded verbatim; the caller upholds `register`'s
        // contract (deregister before the PTY is dropped).
        unsafe { self.inner.register(poll, event, mode) }
    }

    fn reregister(&mut self, poll: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        self.inner.reregister(poll, event, mode)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.inner.deregister(poll)
    }

    fn reader(&mut self) -> &mut TapReader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut File {
        self.inner.writer()
    }
}

impl EventedPty for TappedPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.inner.next_child_event()
    }
}

impl OnResize for TappedPty {
    fn on_resize(&mut self, size: WindowSize) {
        self.inner.on_resize(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, ShellState) {
        let shell = Arc::new(Mutex::new(ShellState::default()));
        let mut tap = OscTap::new(shell.clone());
        let mut out = Vec::new();
        for chunk in chunks {
            tap.feed(chunk, &mut out);
        }
        let state = shell.lock().clone();
        (out, state)
    }

    #[test]
    fn ordinary_output_passes_through_unchanged() {
        let input =
            b"hello \x1b[31mred\x1b[0m \x1b]0;title\x07 \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\";
        assert_eq!(run(&[input]).0, input.to_vec());
    }

    #[test]
    fn osc7_sets_cwd_and_is_swallowed() {
        let (out, state) = run(&[b"a\x1b]7;file://localhost/home/raven/My%20Dir\x07b"]);
        assert_eq!(out, b"ab".to_vec());
        assert_eq!(state.cwd, Some(PathBuf::from("/home/raven/My Dir")));
        assert_eq!(state.remote_cwd, None);
    }

    #[test]
    fn osc7_from_another_host_is_a_remote_cwd() {
        let (_, state) = run(&[
            b"\x1b]7;file:///home/me\x07",
            b"\x1b]7;file://web1.prod.example/srv/app\x07",
        ]);
        assert_eq!(state.cwd, Some(PathBuf::from("/home/me")));
        assert_eq!(
            state.remote_cwd,
            Some(("web1.prod.example".into(), PathBuf::from("/srv/app")))
        );
        // Back home: the local shell reports again.
        let (_, state) = run(&[b"\x1b]7;file://web1/srv\x07", b"\x1b]7;file:///home/me\x07"]);
        assert_eq!(state.remote_cwd, None);
    }

    #[test]
    fn prompt_marks_become_hidden_hyperlinks() {
        let (out, state) =
            run(&[b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\x1b]133;D;2\x07"]);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "\x1b]8;id=cyberterm-prompt-1;cyberterm-mark:prompt?exit=\x1b\\$ \x1b]8;;\x1b\\ls\r\n\x1b]8;;\x1b\\out"
        );
        assert_eq!(state.last_exit, Some(2));
        assert_eq!(state.prompts, 1);
        assert!(!state.command_running);
    }

    #[test]
    fn blocks_record_command_cwd_timing_and_exit() {
        let (_, state) = run(&[
            b"\x1b]7;file:///srv\x07\x1b]133;A\x07$ \x1b]133;B\x07",
            b"\x1b]133;C;cmdline_url=make%20test\x07out\x1b]133;D;3\x07",
            b"\x1b]133;A\x07$ \x1b]133;B\x07",
        ]);
        assert_eq!(state.blocks.len(), 2);
        let b = &state.blocks[0];
        assert_eq!(b.mark, 1);
        assert_eq!(b.command.as_deref(), Some("make test"));
        assert_eq!(b.cwd, Some(PathBuf::from("/srv")));
        assert_eq!(b.exit, Some(3));
        assert!(b.is_command() && !b.running() && b.duration_ms().is_some());
        // The newest prompt hasn't run anything yet.
        assert!(!state.blocks[1].is_command());
    }

    #[test]
    fn sequences_split_across_reads_are_reassembled() {
        let (out, state) = run(&[
            b"x\x1b",
            b"]13",
            b"3;D;1",
            b"\x1b",
            b"\\y\x1b]7;file:",
            b"//localhost/tmp\x1b\\",
        ]);
        assert_eq!(out, b"xy".to_vec());
        assert_eq!(state.last_exit, Some(1));
        assert_eq!(state.cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn desktop_notifications_are_recorded_and_swallowed() {
        let (out, state) = run(&[b"a\x1b]9;Claude needs your input\x07b"]);
        assert_eq!(out, b"ab".to_vec());
        assert_eq!(
            state.notice.map(|n| n.1).as_deref(),
            Some("Claude needs your input")
        );
        let (out, state) = run(&[b"\x1b]777;notify;Codex;", b"Approve the command?\x1b\\"]);
        assert!(out.is_empty());
        assert_eq!(
            state.notice.map(|n| n.1).as_deref(),
            Some("Codex: Approve the command?")
        );
        // ConEmu's OSC 9;4 (progress) isn't a notification.
        let (_, state) = run(&[b"\x1b]9;4;1;50\x07"]);
        assert_eq!(state.notice, None);
    }

    #[test]
    fn similar_but_foreign_osc_codes_pass_through() {
        for input in [&b"\x1b]1337;foo\x07"[..], b"\x1b]13\x07", b"\x1b]7\x1b[m"] {
            assert_eq!(run(&[input]).0, input.to_vec(), "{input:?}");
        }
    }

    #[test]
    fn escape_inside_payload_ends_it_and_starts_a_new_sequence() {
        let (out, _) = run(&[b"\x1b]133;B\x1b[1m"]);
        assert_eq!(out, b"\x1b]8;;\x1b\\\x1b[1m".to_vec());
    }
}
