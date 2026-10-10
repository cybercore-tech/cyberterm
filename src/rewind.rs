// src/rewind.rs
//
// Rewind: every pane keeps a recording of its recent output so you can
// step back through what its screen looked like -- a TUI's earlier state,
// output wiped by `clear`, a progress display mid-run.
//
// Recorder: output chunks (with times) and resizes, capped in bytes. When
// the cap is hit, the oldest chunks are played into a `base` terminal (a
// screen-only copy of the pane's terminal as it was at the oldest chunk
// still kept), so the earliest moment can always be rebuilt.
//
// Timeline: built when rewind opens. One pass replays the recording and
// keeps a checkpoint (a screen snapshot, src/mux/snapshot.rs) every
// CHECKPOINT_BYTES, so showing any moment replays at most that much.
// "Moments" are the points where output paused -- screens as they settled.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use alacritty_terminal::Term;

use crate::session::GridSize;

/// How much output each pane keeps ([rewind]); 0 turns recording off.
/// Global because the daemon client creates recorders too.
pub static CAP_BYTES: AtomicUsize = AtomicUsize::new(2 * 1024 * 1024);

/// Output this close together is one chunk.
/// Replayed terminals answer nobody (no PTY behind them).
#[derive(Clone, Copy)]
pub struct Quiet;

impl EventListener for Quiet {}

/// A rebuilt screen.
pub type ReplayTerm = Term<Quiet>;

const MERGE_MS: u64 = 30;
/// A pause this long after output makes a moment.
const MOMENT_GAP_MS: u64 = 150;
const CHECKPOINT_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug)]
enum Event {
    Output(Vec<u8>),
    Resize(GridSize),
}

#[derive(Clone, Debug)]
struct Chunk {
    ms: u64,
    event: Event,
}

fn config() -> Config {
    Config {
        scrolling_history: 0,
        ..Config::default()
    }
}

fn fresh(size: GridSize) -> Term<Quiet> {
    Term::new(config(), &size, Quiet)
}

pub struct Recorder {
    base: Term<Quiet>,
    base_parser: Processor<StdSyncHandler>,
    base_size: GridSize,
    /// When the oldest kept state begins.
    base_ms: u64,
    chunks: VecDeque<Chunk>,
    bytes: usize,
    cap: usize,
}

impl Recorder {
    /// A recorder with the configured cap ([rewind] buffer_kb).
    pub fn new(size: GridSize) -> Self {
        Self::with_cap(size, CAP_BYTES.load(Ordering::Relaxed))
    }

    pub fn with_cap(size: GridSize, cap_bytes: usize) -> Self {
        Self {
            base: fresh(size),
            base_parser: Processor::new(),
            base_size: size,
            base_ms: crate::shell::tap::now_ms(),
            chunks: VecDeque::new(),
            bytes: 0,
            cap: cap_bytes,
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.feed_at(crate::shell::tap::now_ms(), data);
    }

    fn feed_at(&mut self, ms: u64, data: &[u8]) {
        if data.is_empty() || self.cap == 0 {
            return;
        }
        self.bytes += data.len();
        match self.chunks.back_mut() {
            Some(Chunk {
                ms: last,
                event: Event::Output(buf),
            }) if ms.saturating_sub(*last) < MERGE_MS && buf.len() < 64 * 1024 => {
                buf.extend_from_slice(data);
                *last = ms;
            }
            _ => self.chunks.push_back(Chunk {
                ms,
                event: Event::Output(data.to_vec()),
            }),
        }
        self.trim();
    }

    pub fn resize(&mut self, size: GridSize) {
        self.chunks.push_back(Chunk {
            ms: crate::shell::tap::now_ms(),
            event: Event::Resize(size),
        });
    }

    /// Starts over from a known screen (a daemon pane's snapshot).
    pub fn reset(&mut self, size: GridSize, screen: &[u8]) {
        self.base = fresh(size);
        self.base_parser = Processor::new();
        self.base_parser.advance(&mut self.base, screen);
        self.base_size = size;
        self.base_ms = crate::shell::tap::now_ms();
        self.chunks.clear();
        self.bytes = 0;
    }

    /// Folds the oldest chunks into `base` until under the cap.
    fn trim(&mut self) {
        while self.bytes > self.cap {
            let Some(chunk) = self.chunks.pop_front() else {
                break;
            };
            match chunk.event {
                Event::Output(data) => {
                    self.bytes -= data.len();
                    self.base_parser.advance(&mut self.base, &data);
                }
                Event::Resize(size) => {
                    self.base.resize(size);
                    self.base_size = size;
                }
            }
            self.base_ms = chunk.ms;
        }
    }

    pub fn timeline(&mut self) -> Timeline {
        let start = crate::mux::snapshot::snapshot(&mut self.base, &config(), Quiet, None);
        Timeline::build(
            start,
            self.base_size,
            self.base_ms,
            self.chunks.iter().cloned().collect(),
        )
    }
}

struct Checkpoint {
    /// Chunks before this index are in the snapshot.
    chunk: usize,
    screen: Vec<u8>,
    size: GridSize,
}

pub struct Timeline {
    chunks: Vec<Chunk>,
    /// For each moment, the number of chunks it includes (so moment i is
    /// the state after chunks[..moments[i]]). Moment 0 is the start.
    moments: Vec<usize>,
    checkpoints: Vec<Checkpoint>,
    start_ms: u64,
}

impl Timeline {
    fn build(start: Vec<u8>, start_size: GridSize, start_ms: u64, chunks: Vec<Chunk>) -> Self {
        let mut moments = vec![0];
        for (i, c) in chunks.iter().enumerate() {
            let next = chunks.get(i + 1).map(|n| n.ms);
            let paused = next.is_none_or(|n| n.saturating_sub(c.ms) >= MOMENT_GAP_MS);
            if matches!(c.event, Event::Output(_)) && paused {
                moments.push(i + 1);
            }
        }

        // One replay, checkpointing as it goes.
        let mut checkpoints = vec![Checkpoint {
            chunk: 0,
            screen: start.clone(),
            size: start_size,
        }];
        let mut term = fresh(start_size);
        let mut size = start_size;
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, &start);
        let mut since = 0;
        for (i, c) in chunks.iter().enumerate() {
            match &c.event {
                Event::Output(data) => {
                    parser.advance(&mut term, data);
                    since += data.len();
                }
                Event::Resize(s) => {
                    term.resize(*s);
                    size = *s;
                }
            }
            if since >= CHECKPOINT_BYTES {
                since = 0;
                let screen = crate::mux::snapshot::snapshot(&mut term, &config(), Quiet, None);
                checkpoints.push(Checkpoint {
                    chunk: i + 1,
                    screen,
                    size,
                });
                // Restart the parser with the checkpoint, as a replay from
                // it will, so the two can't drift apart.
                parser = Processor::new();
            }
        }
        Self {
            chunks,
            moments,
            checkpoints,
            start_ms,
        }
    }

    /// How many moments there are (at least one: the start).
    pub fn len(&self) -> usize {
        self.moments.len()
    }

    /// When a moment happened (ms since the epoch).
    pub fn time_ms(&self, moment: usize) -> u64 {
        let n = self.moments[moment.min(self.moments.len() - 1)];
        if n == 0 {
            self.start_ms
        } else {
            self.chunks[n - 1].ms
        }
    }

    /// The screen at a moment.
    pub fn render(&self, moment: usize) -> Term<Quiet> {
        let n = self.moments[moment.min(self.moments.len() - 1)];
        let cp = self
            .checkpoints
            .iter()
            .rev()
            .find(|c| c.chunk <= n)
            .expect("the start is a checkpoint");
        let mut term = fresh(cp.size);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, &cp.screen);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        for c in &self.chunks[cp.chunk..n] {
            match &c.event {
                Event::Output(data) => parser.advance(&mut term, data),
                Event::Resize(s) => term.resize(*s),
            }
        }
        term
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};

    fn size() -> GridSize {
        GridSize { cols: 20, rows: 3 }
    }

    fn row(term: &Term<Quiet>, line: i32) -> String {
        let r = &term.grid()[Line(line)];
        (0..term.columns())
            .map(|c| r[Column(c)].c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn moments_are_pauses_and_replay_shows_each_screen() {
        let mut rec = Recorder::with_cap(size(), 1 << 20);
        rec.feed_at(1000, b"first");
        rec.feed_at(1010, b" part");
        rec.feed_at(2000, b"\x1b[2J\x1b[Hsecond");
        rec.feed_at(3000, b"\x1b[2J\x1b[Hthird");
        let t = rec.timeline();
        assert_eq!(t.len(), 4, "start + three pauses");
        assert_eq!(row(&t.render(1), 0), "first part");
        assert_eq!(row(&t.render(2), 0), "second");
        assert_eq!(row(&t.render(3), 0), "third");
        assert_eq!(t.time_ms(2), 2000);
    }

    #[test]
    fn trimmed_output_lives_on_in_the_base() {
        let mut rec = Recorder::with_cap(size(), 64 * 1024);
        rec.feed_at(1000, b"kept on screen");
        // Push it out of the recording with lots of later output that
        // doesn't touch the first row... then check it's still drawn at
        // the oldest moment.
        let filler = b"\x1b[3;1Hxxxxxxxxxx".repeat(2500);
        rec.feed_at(2001, &filler);
        rec.feed_at(3000, &filler);
        rec.feed_at(4000, &filler);
        assert!(rec.bytes <= 64 * 1024);
        let t = rec.timeline();
        assert_eq!(row(&t.render(0), 0), "kept on screen");
    }

    #[test]
    fn checkpoints_and_resizes_replay_the_same_screen() {
        let mut rec = Recorder::with_cap(size(), 4 << 20);
        let mut ms = 1000;
        for i in 0..2000 {
            ms += 200;
            // Short visible lines, padded with invisible SGR resets so the
            // recording passes several checkpoints.
            rec.feed_at(
                ms,
                format!("\r\nline {i:04}{}", "\x1b[0m".repeat(20)).as_bytes(),
            );
        }
        rec.resize(GridSize { cols: 30, rows: 4 });
        rec.feed_at(ms + 500, b"\r\nafter resize");
        let t = rec.timeline();
        assert!(t.checkpoints.len() > 1);
        let last = t.render(t.len() - 1);
        assert_eq!(last.columns(), 30);
        assert_eq!(last.screen_lines(), 4);
        assert!((0..4).any(|l| row(&last, l) == "after resize"));
        let mid = t.render(1500);
        let text: String = (0..3).map(|l| row(&mid, l)).collect::<Vec<_>>().join("\n");
        assert!(text.contains("line 1499"), "{text}");
    }
}
