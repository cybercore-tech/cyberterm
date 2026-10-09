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

/// URI scheme of the hidden prompt marks.
pub const MARK_SCHEME: &str = "cyberterm-mark:";

/// What the shell has told us about itself through OSC 7/133.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellState {
    pub cwd: Option<PathBuf>,
    pub last_exit: Option<i32>,
    pub command_running: bool,
    /// Prompts drawn so far; non-zero means shell integration is active.
    pub prompts: u64,
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

const OURS: [&[u8]; 2] = [b"133;", b"7;"];

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
            if let Some(path) = parse_file_url(url) {
                shell.cwd = Some(path);
            }
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
            }
            "D" => {
                shell.command_running = false;
                shell.last_exit = fields.next().and_then(|f| f.trim().parse().ok());
            }
            _ => {}
        }
    }
}

/// `file://host/some%20path` -> `/some path`. The host is ignored: a
/// remote shell's directory isn't a local path, but the cwd is still
/// worth showing, and Phase 1's SSH-aware panes will need it.
fn parse_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    Some(PathBuf::from(percent_decode(path)))
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
            },
        })
    }
}

pub struct TapReader {
    file: File,
    tap: OscTap,
    raw: Vec<u8>,
    pending: Vec<u8>,
    pos: usize,
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
            self.tap.feed(&self.raw[..n], &mut self.pending);
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
        let (out, state) = run(&[b"a\x1b]7;file://box/home/raven/My%20Dir\x07b"]);
        assert_eq!(out, b"ab".to_vec());
        assert_eq!(state.cwd, Some(PathBuf::from("/home/raven/My Dir")));
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
    fn sequences_split_across_reads_are_reassembled() {
        let (out, state) = run(&[
            b"x\x1b",
            b"]13",
            b"3;D;1",
            b"\x1b",
            b"\\y\x1b]7;file:",
            b"//h/tmp\x1b\\",
        ]);
        assert_eq!(out, b"xy".to_vec());
        assert_eq!(state.last_exit, Some(1));
        assert_eq!(state.cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn similar_but_foreign_osc_codes_pass_through() {
        for input in [
            &b"\x1b]1337;foo\x07"[..],
            b"\x1b]777;notify;a;b\x07",
            b"\x1b]13\x07",
            b"\x1b]7\x1b[m",
        ] {
            assert_eq!(run(&[input]).0, input.to_vec(), "{input:?}");
        }
    }

    #[test]
    fn escape_inside_payload_ends_it_and_starts_a_new_sequence() {
        let (out, _) = run(&[b"\x1b]133;B\x1b[1m"]);
        assert_eq!(out, b"\x1b]8;;\x1b\\\x1b[1m".to_vec());
    }
}
