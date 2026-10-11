// src/agent_home.rs
//
// `cyberterm +agent` on its own, in a terminal: the agent home. The
// Cyberterm wordmark, then the agents found here with how much each tells
// the flight log, the agent sessions with their state and what they've
// changed, the keys for the Flight log and Changes, and how to start one.
// Coloured from the active theme; printed once, so it scrolls away like
// any other output. Piped, `+agent` prints the plain listing instead.

use crate::agent_setup::Coverage;
use crate::flight_log::State;
use crate::theme::Theme;

type Rgb = [u8; 3];

// ----------------------------------------------------------------------
// What it shows
// ----------------------------------------------------------------------

pub struct AgentRow {
    pub name: String,
    pub coverage: Coverage,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mark {
    Working,
    Waiting,
    Done,
    Stopped,
}

pub struct SessionRow {
    pub id: String,
    pub agent: String,
    pub mark: Mark,
    /// Milliseconds since its last flight-log entry.
    pub idle_ms: Option<u64>,
    /// Its branch, or where it runs when it has no worktree.
    pub place: String,
    /// Lines added and removed, and files changed, against where it began;
    /// `None` when that can't be told (no worktree, or it's gone).
    pub changes: Option<(usize, usize, usize)>,
    /// Its agent is still running.
    pub running: bool,
    /// Commits on its branch since it started.
    pub commits: usize,
    /// The group of attempts at one task it's part of.
    pub group: Option<String>,
}

pub struct Home {
    pub version: &'static str,
    pub theme: String,
    pub dir: String,
    pub agents: Vec<AgentRow>,
    pub sessions: Vec<SessionRow>,
    /// Sessions not shown.
    pub more: usize,
    /// (key, what it does) for the footer.
    pub keys: Vec<(String, &'static str)>,
    /// The agent the example command starts.
    pub example: Option<String>,
}

/// Sessions shown before "+N more".
const MAX_SESSIONS: usize = 6;

// ----------------------------------------------------------------------
// Colours
// ----------------------------------------------------------------------

pub struct Palette {
    pub fg: Rgb,
    pub muted: Rgb,
    /// Hairlines and unlit meter segments.
    pub line: Rgb,
    /// Key caps.
    pub panel: Rgb,
    /// The wordmark's sweep: magenta, blue, cyan.
    pub sweep: [Rgb; 3],
    pub ok: Rgb,
    pub warn: Rgb,
    pub err: Rgb,
}

fn hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    let n = u32::from_str_radix(s.get(..6)?, 16).ok()?;
    Some(slot(n))
}

fn slot(n: u32) -> Rgb {
    [(n >> 16) as u8, (n >> 8) as u8, n as u8]
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    std::array::from_fn(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8)
}

fn sweep(stops: &[Rgb; 3], t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0) * 2.0;
    if t <= 1.0 {
        mix(stops[0], stops[1], t)
    } else {
        mix(stops[1], stops[2], t - 1.0)
    }
}

impl Palette {
    /// From a theme's background, foreground and ANSI colours, so it
    /// matches whatever the window shows (light themes included).
    pub fn from_theme(t: &Theme) -> Self {
        let c = |i: usize| slot(t.colors[i]);
        let bg = hex(&t.background).unwrap_or(c(0));
        let fg = hex(&t.foreground).unwrap_or(c(7));
        let sweep = [c(5), c(4), c(6)];
        Palette {
            fg,
            muted: mix(bg, fg, 0.55),
            line: mix(bg, sweep[1], 0.35),
            panel: mix(bg, fg, 0.12),
            sweep,
            ok: c(2),
            warn: c(3),
            err: c(1),
        }
    }

    pub fn classic() -> Self {
        let bg = [0x0b, 0x0e, 0x14];
        let fg = [0xd8, 0xde, 0xe9];
        let sweep = [[0xff, 0x2e, 0xa6], [0x8b, 0x5c, 0xff], [0x22, 0xd3, 0xee]];
        Palette {
            fg,
            muted: mix(bg, fg, 0.55),
            line: mix(bg, sweep[1], 0.35),
            panel: mix(bg, fg, 0.12),
            sweep,
            ok: [0x4a, 0xde, 0x80],
            warn: [0xfb, 0xbf, 0x24],
            err: [0xf8, 0x71, 0x71],
        }
    }
}

// ----------------------------------------------------------------------
// Styled lines
// ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Plain,
    Ansi256,
    Truecolor,
}

#[derive(Clone, Copy, Default)]
struct Style {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    bold: bool,
}

fn fg(c: Rgb) -> Style {
    Style {
        fg: Some(c),
        ..Default::default()
    }
}

fn bold(c: Rgb) -> Style {
    Style {
        fg: Some(c),
        bold: true,
        ..Default::default()
    }
}

#[derive(Default)]
struct Line(Vec<(String, Style)>);

impl Line {
    fn add(mut self, text: impl Into<String>, style: Style) -> Self {
        self.push(text, style);
        self
    }

    fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if !text.is_empty() {
            self.0.push((text, style));
        }
    }

    fn width(&self) -> usize {
        self.0.iter().map(|(t, _)| t.chars().count()).sum()
    }

    fn pad(&mut self, to: usize) {
        let w = self.width();
        if w < to {
            self.push(" ".repeat(to - w), Style::default());
        }
    }

    fn append(&mut self, other: Line) {
        self.0.extend(other.0);
    }
}

/// The nearest xterm-256 colour.
fn to_256(c: Rgb) -> u8 {
    let level = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v - 35) / 40
        }
    };
    let [r, g, b] = c.map(level);
    16 + 36 * r + 6 * g + b
}

fn emit(lines: &[Line], mode: Mode) -> String {
    let mut out = String::new();
    for line in lines {
        let mut s = String::new();
        for (text, style) in &line.0 {
            let mut codes: Vec<String> = Vec::new();
            if mode != Mode::Plain {
                if style.bold {
                    codes.push("1".into());
                }
                let color = |base: u8, c: Rgb| match mode {
                    Mode::Truecolor => format!("{base};2;{};{};{}", c[0], c[1], c[2]),
                    _ => format!("{base};5;{}", to_256(c)),
                };
                if let Some(c) = style.fg {
                    codes.push(color(38, c));
                }
                if let Some(c) = style.bg {
                    codes.push(color(48, c));
                }
            }
            if codes.is_empty() {
                s.push_str(text);
            } else {
                s.push_str(&format!("\x1b[{}m{text}\x1b[0m", codes.join(";")));
            }
        }
        out.push_str(s.trim_end_matches(' '));
        out.push('\n');
    }
    out
}

/// `s` cut to `width` chars, ending in "…" when cut.
fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut t: String = s.chars().take(width - 1).collect();
    t.push('…');
    t
}

fn ago(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

// ----------------------------------------------------------------------
// The wordmark
// ----------------------------------------------------------------------

/// CYBERTERM, 8 px tall: 2 px stems, 1 px bars, cut corners.
const FONT: [(char, [&str; 8]); 7] = [
    (
        'C',
        [
            "..#####", ".##....", "##.....", "##.....", "##.....", "##.....", ".##....", "..#####",
        ],
    ),
    (
        'Y',
        [
            "##....##", ".##..##.", "..####..", "...##...", "...##...", "...##...", "...##...",
            "...##...",
        ],
    ),
    (
        'B',
        [
            "######.", "##...##", "##...##", "######.", "##...##", "##...##", "##...##", "######.",
        ],
    ),
    (
        'E',
        [
            "######", "##....", "##....", "#####.", "##....", "##....", "##....", "######",
        ],
    ),
    (
        'R',
        [
            "######.", "##...##", "##...##", "######.", "##.##..", "##..##.", "##...##", "##...##",
        ],
    ),
    (
        'T',
        [
            "########", "...##...", "...##...", "...##...", "...##...", "...##...", "...##...",
            "...##...",
        ],
    ),
    (
        'M',
        [
            "###...###",
            "####.####",
            "##.###.##",
            "##..#..##",
            "##.....##",
            "##.....##",
            "##.....##",
            "##.....##",
        ],
    ),
];

/// Quadrant blocks, indexed by (top-left, top-right, bottom-left,
/// bottom-right) as bits 8, 4, 2, 1. Cyberterm draws these from the cell
/// geometry (src/boxdraw.rs), so the edges come out pixel-exact.
const QUADS: [char; 16] = [
    ' ', '▗', '▖', '▄', '▝', '▐', '▞', '▟', '▘', '▚', '▌', '▙', '▀', '▜', '▛', '█',
];

/// The wordmark as rows of cells.
fn wordmark() -> Vec<Vec<char>> {
    let mut px: Vec<String> = vec![String::new(); 8];
    for (i, ch) in "CYBERTERM".chars().enumerate() {
        let glyph = &FONT.iter().find(|(c, _)| *c == ch).expect("in the font").1;
        for (row, bits) in px.iter_mut().zip(glyph) {
            if i > 0 {
                row.push_str("..");
            }
            row.push_str(bits);
        }
    }
    let width = px[0].len().div_ceil(2) * 2;
    let on = |r: usize, c: usize| px[r].as_bytes().get(c) == Some(&b'#');
    (0..4)
        .map(|y| {
            (0..width / 2)
                .map(|x| {
                    let (r, c) = (y * 2, x * 2);
                    let i = (on(r, c) as usize) << 3
                        | (on(r, c + 1) as usize) << 2
                        | (on(r + 1, c) as usize) << 1
                        | on(r + 1, c + 1) as usize;
                    QUADS[i]
                })
                .collect()
        })
        .collect()
}

// ----------------------------------------------------------------------
// Layout
// ----------------------------------------------------------------------

const MARGIN: usize = 2;

fn spaced(label: &str) -> String {
    label
        .chars()
        .map(String::from)
        .collect::<Vec<_>>()
        .join(" ")
}

fn header(h: &Home, inner: usize, p: &Palette) -> Vec<Line> {
    let mut out = Vec::new();
    let meta = [
        Line::default()
            .add("v", fg(p.muted))
            .add(h.version, bold(p.fg)),
        Line::default()
            .add("theme ", fg(p.muted))
            .add(h.theme.clone(), fg(p.fg)),
        Line::default().add(h.dir.clone(), fg(p.muted)),
    ];
    let mark = wordmark();
    let mark_w = mark[0].len();
    let meta_w = meta.iter().map(Line::width).max().unwrap_or(0);
    if inner >= mark_w {
        let beside = inner >= mark_w + 4 + meta_w;
        for (r, cells) in mark.iter().enumerate() {
            let mut line = Line::default();
            for (x, ch) in cells.iter().enumerate() {
                let t = x as f32 / (mark_w - 1) as f32;
                line.push(ch.to_string(), fg(sweep(&p.sweep, t)));
            }
            if beside && r > 0 {
                let m = &meta[r - 1];
                line.pad(inner - m.width());
                line.append(Line(m.0.clone()));
            }
            out.push(line);
        }
        out.push(rule(inner, p));
        if !beside {
            out.push(meta_line(h, inner, p));
        }
    } else {
        let title = spaced("CYBERTERM");
        let n = title.chars().count().max(2);
        let mut line = Line::default();
        for (x, ch) in title.chars().enumerate() {
            line.push(
                ch.to_string(),
                bold(sweep(&p.sweep, x as f32 / (n - 1) as f32)),
            );
        }
        out.push(line);
        out.push(rule(inner, p));
        out.push(meta_line(h, inner, p));
    }
    out
}

fn meta_line(h: &Home, inner: usize, p: &Palette) -> Line {
    let text = format!("v{} · {} · {}", h.version, h.theme, h.dir);
    Line::default().add(fit(&text, inner), fg(p.muted))
}

/// A heavy hairline in the sweep's colours, fading into the line colour.
fn rule(inner: usize, p: &Palette) -> Line {
    let mut line = Line::default();
    for x in 0..inner {
        let k = x as f32 / inner.saturating_sub(1).max(1) as f32;
        let c = mix(
            sweep(&p.sweep, (k * 2.2).min(1.0)),
            p.line,
            (k * 1.6).min(1.0),
        );
        line.push("━", fg(c));
    }
    line
}

fn meter(full: bool, p: &Palette) -> Line {
    let lit = if full { 3 } else { 1 };
    let mut line = Line::default();
    for i in 0..3 {
        if i > 0 {
            line.push(" ", Style::default());
        }
        line.push("━━", fg(if i < lit { p.sweep[2] } else { p.line }));
    }
    line
}

/// The agents column, `width` wide: a row each for the agents that
/// report in full (or could, after a setup step), then the rest as one
/// wrapped group -- a machine with fifteen agents shouldn't push the
/// wordmark off the screen.
fn agents_column(h: &Home, width: usize, p: &Palette) -> Vec<Line> {
    let mut out = vec![
        Line::default().add(spaced("AGENTS"), bold(p.sweep[2])),
        Line::default(),
    ];
    if h.agents.is_empty() {
        out.push(Line::default().add("None found on PATH.", fg(p.muted)));
        out.push(Line::default().add("Add one under [agents.launch].", fg(p.muted)));
        return out;
    }
    let (rows, rest): (Vec<&AgentRow>, Vec<&AgentRow>) = h
        .agents
        .iter()
        .partition(|a| a.coverage.full || a.coverage.setup.is_some());
    let name_w = rows
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(6, 14)
        + 2;
    let indent = 8 + 2;
    for a in &rows {
        let mut line = meter(a.coverage.full, p);
        line.push("  ", Style::default());
        line.push(fit(&a.name, name_w - 2), fg(p.fg));
        line.pad(indent + name_w);
        let room = width.saturating_sub(indent + name_w);
        match a.coverage.setup {
            Some(cmd) => line.push(fit(cmd, room), fg(p.warn)),
            None => line.push(fit(a.coverage.note, room), fg(p.muted)),
        }
        out.push(line);
    }
    if !rest.is_empty() {
        let mut line = meter(false, p);
        line.push("  ", Style::default());
        line.push("commands only", fg(p.muted));
        out.push(line);
        let room = width.saturating_sub(indent).max(12);
        let mut line = Line::default();
        for (i, a) in rest.iter().enumerate() {
            let sep = if i + 1 < rest.len() { " · " } else { "" };
            let w = a.name.chars().count() + sep.chars().count();
            if line.width() > indent && line.width() + w > indent + room {
                out.push(std::mem::take(&mut line));
            }
            line.pad(indent);
            line.push(fit(&a.name, room), fg(p.fg));
            line.push(sep, fg(p.muted));
        }
        out.push(line);
    }
    out
}

/// Added vs removed as a green/red line `width` long.
fn diff_bar(added: usize, removed: usize, width: usize, p: &Palette) -> Line {
    let total = added + removed;
    let mut line = Line::default();
    if total == 0 || width == 0 {
        return line;
    }
    let mut green = (width * added + total / 2) / total;
    if added > 0 {
        green = green.max(1);
    }
    if removed > 0 {
        green = green.min(width - 1);
    }
    line.push("━".repeat(green), fg(p.ok));
    line.push("━".repeat(width - green), fg(p.err));
    line
}

fn sessions_column(h: &Home, width: usize, p: &Palette) -> Vec<Line> {
    let running = h
        .sessions
        .iter()
        .filter(|s| matches!(s.mark, Mark::Working | Mark::Waiting))
        .count();
    let mut title = Line::default().add(spaced("SESSIONS"), bold(p.sweep[2]));
    if running > 0 {
        title.push(format!("   {running} running"), fg(p.muted));
    }
    let mut out = vec![title, Line::default()];
    if h.sessions.is_empty() {
        out.push(Line::default().add("No sessions yet.", fg(p.muted)));
        return out;
    }
    let id_w = width.saturating_sub(3 + 9 + 16).clamp(8, 24);
    for s in &h.sessions {
        let (glyph, glyph_c, state, state_c) = match s.mark {
            Mark::Working => (
                "◆",
                p.sweep[2],
                match s.idle_ms {
                    Some(ms) => format!("working · {}", ago(ms)),
                    None => "working".into(),
                },
                p.fg,
            ),
            Mark::Waiting => ("◆!", p.warn, "needs you".into(), p.warn),
            Mark::Done => (
                "✓",
                p.ok,
                match s.changes {
                    Some((_, _, files)) if files > 0 => {
                        format!("done · {files} file{}", if files == 1 { "" } else { "s" })
                    }
                    _ => "done".into(),
                },
                p.fg,
            ),
            Mark::Stopped => ("○", p.muted, "stopped".into(), p.muted),
        };
        let mut first = Line::default().add(glyph, bold(glyph_c));
        first.pad(3);
        first.push(fit(&s.id, id_w), bold(p.fg));
        first.pad(3 + id_w + 2);
        first.push(fit(&s.agent, 8), fg(p.muted));
        first.pad(3 + id_w + 2 + 9);
        first.push(state, fg(state_c));
        out.push(first);

        let mut second = Line::default().add("   ", Style::default());
        second.push(fit(&s.place, id_w + 2), fg(p.muted));
        second.pad(3 + id_w + 2 + 1);
        match s.changes {
            Some((0, 0, _)) => second.push("no changes yet", fg(p.muted)),
            Some((added, removed, _)) => {
                let plus = format!("+{added}");
                let minus = format!("−{removed}");
                let at = second.width();
                second.push(plus, fg(p.ok));
                second.pad(at + 6);
                second.push(minus, fg(p.err));
                second.pad(at + 12);
                second.append(diff_bar(added, removed, 10, p));
            }
            None => {}
        }
        out.push(second);
        out.push(Line::default());
    }
    out.pop();
    if h.more > 0 {
        out.push(Line::default());
        out.push(Line::default().add(
            format!("+{} more · cyberterm +agent list", h.more),
            fg(p.muted),
        ));
    }
    out
}

fn footer(h: &Home, inner: usize, p: &Palette) -> Vec<Line> {
    let mut out = Vec::new();
    let mut line = Line::default();
    for (key, what) in &h.keys {
        let cap = format!(" {key} ");
        let w = cap.chars().count() + 1 + what.chars().count();
        if line.width() > 0 && line.width() + 3 + w > inner {
            out.push(std::mem::take(&mut line));
        } else if line.width() > 0 {
            line.push("   ", Style::default());
        }
        line.push(
            cap,
            Style {
                fg: Some(p.fg),
                bg: Some(p.panel),
                bold: false,
            },
        );
        line.push(format!(" {what}"), fg(p.muted));
    }
    if line.width() > 0 {
        out.push(line);
    }
    out.push(Line::default());
    let agent = h.example.as_deref().unwrap_or("<agent>");
    let mut cmd = Line::default()
        .add("› ", bold(p.sweep[0]))
        .add("cyberterm +agent ", fg(p.fg))
        .add(agent, fg(p.sweep[2]))
        .add(" \"what to do\"", fg(p.muted));
    let more = "list · log · setup · rm · help";
    if cmd.width() + 6 + more.len() <= inner {
        cmd.pad(inner - more.chars().count());
        cmd.push(more, fg(p.muted));
    }
    out.push(cmd);
    out
}

/// The whole page for a terminal `cols` wide.
fn layout(h: &Home, cols: usize, p: &Palette) -> Vec<Line> {
    let cols = cols.min(110);
    let inner = cols.saturating_sub(2 * MARGIN).max(20);
    let mut body = vec![Line::default()];
    body.extend(header(h, inner, p));
    body.push(Line::default());

    const LEFT: usize = 46;
    if inner >= LEFT + 3 + 46 {
        let left = agents_column(h, LEFT - 4, p);
        let left_w = LEFT;
        let right = sessions_column(h, inner - left_w - 3, p);
        let rows = left.len().max(right.len());
        let mut left = left.into_iter();
        let mut right = right.into_iter();
        for _ in 0..rows {
            let mut line = left.next().unwrap_or_default();
            line.pad(left_w);
            line.push("│  ", fg(p.line));
            line.append(right.next().unwrap_or_default());
            body.push(line);
        }
    } else {
        body.extend(agents_column(h, inner, p));
        body.push(Line::default());
        body.extend(sessions_column(h, inner, p));
    }
    body.push(Line::default());
    body.extend(footer(h, inner, p));
    body.push(Line::default());

    body.into_iter()
        .map(|l| {
            if l.width() == 0 {
                l
            } else {
                let mut m = Line::default().add(" ".repeat(MARGIN), Style::default());
                m.append(l);
                m
            }
        })
        .collect()
}

pub fn render(h: &Home, cols: usize, p: &Palette, mode: Mode) -> String {
    emit(&layout(h, cols, p), mode)
}

// ----------------------------------------------------------------------
// Gathering it
// ----------------------------------------------------------------------

pub fn mark(state: State, running: bool) -> Mark {
    match (state, running) {
        (State::Done, _) => Mark::Done,
        (State::Waiting, true) => Mark::Waiting,
        (_, true) => Mark::Working,
        _ => Mark::Stopped,
    }
}

/// A session at a glance (for the agent home and the Tower): its state,
/// where it works and what it has changed. Runs git, so it takes a moment.
pub fn session_row(s: &crate::agent::Session, now: u64) -> SessionRow {
    let st = crate::agent::status(s);
    let events = crate::flight_log::read(&crate::flight_log::log_path(&s.id));
    let summary = crate::flight_log::summary(&crate::flight_log::timeline(&events));
    let changes = s
        .worktree
        .as_ref()
        .filter(|_| !st.worktree_missing)
        .and_then(|w| crate::changes::collect(&w.path, &w.base).ok())
        .map(|c| {
            let (a, r) = c.totals();
            (a, r, c.files.len())
        });
    SessionRow {
        id: s.id.clone(),
        agent: s.agent.clone(),
        mark: mark(summary.state, st.running),
        idle_ms: (summary.last_t > 0).then(|| now.saturating_sub(summary.last_t)),
        place: match &s.worktree {
            Some(_) if st.worktree_missing => "worktree gone".into(),
            Some(w) => w.branch.clone(),
            None => crate::agent::short(&s.dir),
        },
        changes,
        running: st.running,
        commits: st.commits,
        group: s.group.clone(),
    }
}

pub fn gather(cfg: &crate::config::CyberConfig, theme: &str) -> Home {
    use crate::input::bindings::{Action, Bindings};
    let launchers = crate::agent::launchers(&cfg.agents);
    let agents: Vec<AgentRow> = launchers
        .iter()
        .map(|l| AgentRow {
            name: l.name.clone(),
            coverage: crate::agent_setup::coverage_of(crate::agent::adapter(&l.argv)),
        })
        .collect();
    let example = agents
        .iter()
        .find(|a| a.coverage.full)
        .or(agents.first())
        .map(|a| a.name.clone());

    let now = crate::shell::tap::now_ms();
    let all = crate::agent::sessions();
    let more = all.len().saturating_sub(MAX_SESSIONS);
    let sessions = all
        .iter()
        .take(MAX_SESSIONS)
        .map(|s| session_row(s, now))
        .collect();

    let (bindings, _) = Bindings::new(&cfg.keybindings, cfg.keyboard.leader.as_deref());
    let keys = [
        (Action::FlightLog, "flight log"),
        (Action::AgentChanges, "changes"),
        (Action::NewTab, "new tab"),
    ]
    .into_iter()
    .map(|(a, what)| (bindings.hint(a), what))
    .filter(|(k, _)| !k.is_empty())
    .collect();

    let dir = std::env::current_dir()
        .map(|d| crate::agent::short(&d))
        .unwrap_or_default();
    Home {
        version: env!("CARGO_PKG_VERSION"),
        theme: theme.to_string(),
        dir,
        agents,
        sessions,
        more,
        keys,
        example,
    }
}

fn term_cols() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 {
        ws.ws_col as usize
    } else {
        std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(80)
    }
}

fn mode() -> Mode {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return Mode::Plain;
    }
    match std::env::var("COLORTERM").as_deref() {
        Ok("truecolor" | "24bit") => Mode::Truecolor,
        _ => Mode::Ansi256,
    }
}

pub fn run(cfg: &crate::config::CyberConfig, config_root: &std::path::Path) {
    let mut registry = crate::theme::ThemeRegistry::load_from_dir(config_root.join("themes"));
    let shared = registry.append_cybercore_themes();
    let theme = registry.initial(shared.as_deref(), &cfg.theme);
    let (palette, name) = match &theme {
        Some(t) => (Palette::from_theme(t), t.name.clone()),
        None => (Palette::classic(), "classic".to_string()),
    };
    let home = gather(cfg, &name);
    print!("{}", render(&home, term_cols(), &palette, mode()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Home {
        let cov = |full, note, setup| Coverage { full, note, setup };
        Home {
            version: "0.3.1",
            theme: "neosynth-drive".into(),
            dir: "~/work/api".into(),
            agents: vec![
                AgentRow {
                    name: "claude".into(),
                    coverage: cov(true, "hooks · per session", None),
                },
                AgentRow {
                    name: "gemini".into(),
                    coverage: cov(false, "commands", Some("+agent setup gemini")),
                },
                AgentRow {
                    name: "crush".into(),
                    coverage: cov(false, "commands", None),
                },
            ],
            sessions: vec![
                SessionRow {
                    id: "fix-login".into(),
                    agent: "claude".into(),
                    mark: Mark::Working,
                    idle_ms: Some(4 * 60_000 + 10),
                    place: "agent/fix-login".into(),
                    changes: Some((42, 7, 3)),
                    running: true,
                    commits: 1,
                    group: None,
                },
                SessionRow {
                    id: "add-auth".into(),
                    agent: "codex".into(),
                    mark: Mark::Waiting,
                    idle_ms: None,
                    place: "agent/add-auth".into(),
                    changes: Some((0, 0, 0)),
                    running: true,
                    commits: 0,
                    group: None,
                },
            ],
            more: 2,
            keys: vec![
                ("Ctrl+Shift+L".into(), "flight log"),
                ("Ctrl+Shift+M".into(), "changes"),
            ],
            example: Some("claude".into()),
        }
    }

    fn page(h: &Home, cols: usize) -> Vec<String> {
        render(h, cols, &Palette::classic(), Mode::Plain)
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn the_wordmark_is_four_rows_of_quadrants() {
        let m = wordmark();
        assert_eq!(m.len(), 4);
        assert!(m.iter().all(|r| r.len() == m[0].len()));
        assert!(m.iter().flatten().all(|c| QUADS.contains(c)));
        // Every letter's left stem fills the first column.
        assert!(m.iter().skip(1).take(2).all(|r| r[0] == '█'));
    }

    #[test]
    fn a_wide_terminal_gets_two_columns_and_everything_fits() {
        let h = sample();
        let t = page(&h, 104);
        let all = t.join("\n");
        assert!(t.iter().all(|l| l.chars().count() <= 104), "{all}");
        let row = t.iter().find(|l| l.contains("A G E N T S")).unwrap();
        assert!(
            row.contains("S E S S I O N S") && row.contains("2 running"),
            "{all}"
        );
        assert!(all.contains("━━ ━━ ━━  claude"), "{all}");
        assert!(all.contains("hooks · per session"), "{all}");
        assert!(all.contains("+agent setup gemini"), "{all}");
        let group = t.iter().position(|l| l.contains("commands only")).unwrap();
        assert!(t[group + 1].contains("crush"), "{all}");
        assert!(all.contains("◆  fix-login"), "{all}");
        assert!(all.contains("working · 4m"), "{all}");
        assert!(
            all.contains("◆! add-auth") && all.contains("needs you"),
            "{all}"
        );
        assert!(all.contains("+42") && all.contains("−7"), "{all}");
        assert!(all.contains("no changes yet"), "{all}");
        assert!(all.contains("+2 more · cyberterm +agent list"), "{all}");
        assert!(all.contains(" Ctrl+Shift+L  flight log"), "{all}");
        assert!(
            all.contains("› cyberterm +agent claude \"what to do\""),
            "{all}"
        );
        assert!(
            all.contains("v0.3.1") && all.contains("theme neosynth-drive"),
            "{all}"
        );
    }

    #[test]
    fn a_narrow_terminal_stacks_and_still_fits() {
        let h = sample();
        for cols in [40, 60, 80] {
            let t = page(&h, cols);
            let all = t.join("\n");
            assert!(
                t.iter().all(|l| l.chars().count() <= cols),
                "{cols}:\n{all}"
            );
            let agents = t.iter().position(|l| l.contains("A G E N T S")).unwrap();
            let sessions = t
                .iter()
                .position(|l| l.contains("S E S S I O N S"))
                .unwrap();
            assert!(sessions > agents, "{all}");
        }
        // Too narrow for the wordmark: the name spelled out instead.
        assert!(page(&h, 40).join("\n").contains("C Y B E R T E R M"));
    }

    #[test]
    fn agents_without_hooks_share_one_wrapped_group() {
        let mut h = sample();
        for name in [
            "agy",
            "cursor-agent",
            "copilot",
            "grok",
            "hermes",
            "muse",
            "omp",
            "opencode",
            "ori",
            "pi",
            "demo",
            "aider",
        ] {
            h.agents.push(AgentRow {
                name: name.into(),
                coverage: Coverage {
                    full: false,
                    note: "commands",
                    setup: None,
                },
            });
        }
        for cols in [70, 104] {
            let t = page(&h, cols);
            let all = t.join("\n");
            assert!(t.iter().all(|l| l.chars().count() <= cols), "{all}");
            // Fifteen agents, still a page that fits a screen.
            assert!(t.len() <= 40, "{} lines:\n{all}", t.len());
            assert!(all.contains("commands only"), "{all}");
            for a in &h.agents {
                assert!(all.contains(&a.name), "{} missing:\n{all}", a.name);
            }
            // Only claude and gemini get a row of their own.
            assert_eq!(
                t.iter().filter(|l| l.contains("━━ ━━ ━━")).count(),
                3,
                "{all}"
            );
        }
    }

    #[test]
    fn no_sessions_and_no_agents_say_so() {
        let mut h = sample();
        h.sessions.clear();
        h.more = 0;
        h.agents.clear();
        h.example = None;
        let all = page(&h, 104).join("\n");
        assert!(all.contains("No sessions yet."), "{all}");
        assert!(all.contains("None found on PATH."), "{all}");
        assert!(all.contains("cyberterm +agent <agent>"), "{all}");
    }

    #[test]
    fn colour_modes() {
        let h = sample();
        let p = Palette::classic();
        let tc = render(&h, 104, &p, Mode::Truecolor);
        assert!(tc.contains("\x1b[38;2;"));
        let c256 = render(&h, 104, &p, Mode::Ansi256);
        assert!(c256.contains("\x1b[38;5;") && !c256.contains(";2;"));
        assert!(!render(&h, 104, &p, Mode::Plain).contains('\x1b'));
        assert_eq!(to_256([0, 0, 0]), 16);
        assert_eq!(to_256([255, 255, 255]), 231);
    }

    #[test]
    fn the_diff_bar_keeps_both_sides_visible() {
        let p = Palette::classic();
        let w = |a, r| {
            let l = diff_bar(a, r, 10, &p);
            (
                l.0.first().map(|s| s.0.chars().count()).unwrap_or(0),
                l.width(),
            )
        };
        assert_eq!(w(42, 7), (9, 10));
        assert_eq!(w(1000, 1), (9, 10));
        assert_eq!(w(1, 1000), (1, 10));
        assert_eq!(w(0, 0), (0, 0));
    }

    #[test]
    fn states_follow_the_tab_marks() {
        assert_eq!(mark(State::Waiting, true), Mark::Waiting);
        assert_eq!(mark(State::Waiting, false), Mark::Stopped);
        assert_eq!(mark(State::Done, false), Mark::Done);
        assert_eq!(mark(State::Idle, true), Mark::Working);
        assert_eq!(mark(State::Working, false), Mark::Stopped);
    }
}
