// src/mux/snapshot.rs
//
// Serializes a terminal's state as an escape-sequence stream that, written
// into a fresh terminal of the same size, rebuilds it: scrollback and
// screen (text, colors, attributes, hyperlinks -- which includes the hidden
// shell-integration prompt marks), cursor, the alternate screen, modes,
// keyboard protocol flags, cursor style and palette overrides.
//
// This is how a window attaching to the daemon gets an exact replica of
// each pane; after the snapshot it receives the same live byte stream the
// daemon parses.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, StdSyncHandler};
use alacritty_terminal::Term;

/// Snapshot of `term`. Taking one while the alternate screen is active is
/// destructive inside alacritty_terminal (its screen swap clears the
/// alternate grid), so in that case `term` is rebuilt from the snapshot
/// itself with `config` and `listener`, leaving it equivalent.
pub fn snapshot<T: EventListener + Clone>(
    term: &mut Term<T>,
    config: &Config,
    listener: T,
    title: Option<&str>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mode = *term.mode();
    let alt = mode.contains(TermMode::ALT_SCREEN);

    let alt_part = alt.then(|| {
        let mut part = Vec::new();
        let rows = term.screen_lines() as i32;
        part.extend_from_slice(b"\x1b[H");
        write_lines(term.grid(), 0, rows - 1, &mut part);
        write_cursor(term.grid(), &mut part);
        part
    });
    if alt {
        // Primary screen becomes the active grid (alternate kept aside).
        term.swap_alt();
    }

    let grid = term.grid();
    let top = grid.topmost_line().0;
    let bottom = grid.bottommost_line().0;
    write_lines(grid, top, bottom, &mut out);
    write_cursor(grid, &mut out);

    if let Some(part) = alt_part {
        out.extend_from_slice(b"\x1b[?1049h");
        out.extend_from_slice(&part);
    }

    write_template(term, alt, &mut out);
    write_modes(mode, &mut out);
    write_cursor_style(term, config, &mut out);
    write_colors(term, &mut out);
    if let Some(title) = title.filter(|t| !t.is_empty()) {
        out.extend_from_slice(format!("\x1b]2;{title}\x1b\\").as_bytes());
    }

    if alt {
        let size = Size(term.columns(), term.screen_lines());
        let mut rebuilt = Term::new(config.clone(), &size, listener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut rebuilt, &out);
        *term = rebuilt;
    }
    out
}

struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

#[derive(Clone, PartialEq)]
struct Style {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

impl Style {
    const RENDER_FLAGS: Flags = Flags::BOLD
        .union(Flags::DIM)
        .union(Flags::ITALIC)
        .union(Flags::ALL_UNDERLINES)
        .union(Flags::INVERSE)
        .union(Flags::HIDDEN)
        .union(Flags::STRIKEOUT);

    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & Self::RENDER_FLAGS,
            underline: cell.underline_color(),
        }
    }

    fn default_style() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline: None,
        }
    }

    fn sgr(&self, out: &mut Vec<u8>) {
        let mut params = vec!["0".to_string()];
        let f = self.flags;
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
        ] {
            if f.contains(flag) {
                params.push(code.to_string());
            }
        }
        if let Some(fg) = color_param(self.fg, 30) {
            params.push(fg);
        }
        if let Some(bg) = color_param(self.bg, 40) {
            params.push(bg);
        }
        if let Some(ul) = self.underline {
            match ul {
                Color::Spec(c) => params.push(format!("58:2::{}:{}:{}", c.r, c.g, c.b)),
                Color::Indexed(i) => params.push(format!("58:5:{i}")),
                Color::Named(n) if (n as usize) < 16 => params.push(format!("58:5:{}", n as usize)),
                Color::Named(_) => {}
            }
        }
        out.extend_from_slice(format!("\x1b[{}m", params.join(";")).as_bytes());
    }
}

/// SGR parameter for a foreground (`base` 30) or background (40) color;
/// `None` for the default.
fn color_param(color: Color, base: u16) -> Option<String> {
    match color {
        Color::Spec(c) => Some(format!("{};2;{};{};{}", base + 8, c.r, c.g, c.b)),
        Color::Indexed(i) => Some(format!("{};5;{i}", base + 8)),
        Color::Named(n) => {
            let i = n as usize;
            if i < 8 {
                Some(format!("{}", base as usize + i))
            } else if i < 16 {
                Some(format!("{}", base as usize + 60 + i - 8))
            } else {
                // Foreground/Background (and anything else) are defaults.
                None
            }
        }
    }
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && Style::of(cell) == Style::default_style()
        && cell.hyperlink().is_none()
        && cell.zerowidth().is_none()
}

/// Writes grid lines `from..=to` (negative = scrollback), one per output
/// line; soft-wrapped lines are written in full so the terminal re-wraps
/// them identically.
fn write_lines(grid: &Grid<Cell>, from: i32, to: i32, out: &mut Vec<u8>) {
    let cols = grid.columns();
    let mut style = Style::default_style();
    let mut link: Option<(String, String)> = None;
    out.extend_from_slice(b"\x1b[0m");

    for line in from..=to {
        let row = &grid[Line(line)];
        let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let end = if wrapped {
            cols
        } else {
            (0..cols)
                .rev()
                .find(|&c| !is_blank(&row[Column(c)]))
                .map_or(0, |c| c + 1)
        };
        for col in 0..end {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let cell_style = Style::of(cell);
            if cell_style != style {
                cell_style.sgr(out);
                style = cell_style;
            }
            let cell_link = cell
                .hyperlink()
                .map(|h| (h.id().to_string(), h.uri().to_string()));
            if cell_link != link {
                match &cell_link {
                    Some((id, uri)) => {
                        out.extend_from_slice(format!("\x1b]8;id={id};{uri}\x1b\\").as_bytes())
                    }
                    None => out.extend_from_slice(b"\x1b]8;;\x1b\\"),
                }
                link = cell_link;
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
            if let Some(extra) = cell.zerowidth() {
                for ch in extra {
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        // Links and colors never carry across a line break.
        if link.take().is_some() {
            out.extend_from_slice(b"\x1b]8;;\x1b\\");
        }
        if style != Style::default_style() {
            out.extend_from_slice(b"\x1b[0m");
            style = Style::default_style();
        }
        if line != to && !wrapped {
            out.extend_from_slice(b"\r\n");
        }
    }
}

fn write_cursor(grid: &Grid<Cell>, out: &mut Vec<u8>) {
    let p = grid.cursor.point;
    out.extend_from_slice(format!("\x1b[{};{}H", p.line.0 + 1, p.column.0 + 1).as_bytes());
}

/// The attributes new text will be written with.
fn write_template<T>(term: &Term<T>, _alt: bool, out: &mut Vec<u8>) {
    let template = &term.grid().cursor.template;
    let style = Style::of(template);
    if style != Style::default_style() {
        style.sgr(out);
    }
}

fn write_modes(mode: TermMode, out: &mut Vec<u8>) {
    let mut set = |on: bool, seq: &[u8]| {
        if on {
            out.extend_from_slice(seq);
        }
    };
    set(mode.contains(TermMode::APP_CURSOR), b"\x1b[?1h");
    set(mode.contains(TermMode::APP_KEYPAD), b"\x1b=");
    set(!mode.contains(TermMode::SHOW_CURSOR), b"\x1b[?25l");
    set(!mode.contains(TermMode::LINE_WRAP), b"\x1b[?7l");
    set(mode.contains(TermMode::INSERT), b"\x1b[4h");
    set(mode.contains(TermMode::LINE_FEED_NEW_LINE), b"\x1b[20h");
    set(mode.contains(TermMode::MOUSE_REPORT_CLICK), b"\x1b[?1000h");
    set(mode.contains(TermMode::MOUSE_DRAG), b"\x1b[?1002h");
    set(mode.contains(TermMode::MOUSE_MOTION), b"\x1b[?1003h");
    set(mode.contains(TermMode::UTF8_MOUSE), b"\x1b[?1005h");
    set(mode.contains(TermMode::SGR_MOUSE), b"\x1b[?1006h");
    set(mode.contains(TermMode::FOCUS_IN_OUT), b"\x1b[?1004h");
    set(mode.contains(TermMode::BRACKETED_PASTE), b"\x1b[?2004h");
    set(!mode.contains(TermMode::ALTERNATE_SCROLL), b"\x1b[?1007l");
    set(!mode.contains(TermMode::URGENCY_HINTS), b"\x1b[?1042l");

    let kitty = [
        (TermMode::DISAMBIGUATE_ESC_CODES, 1),
        (TermMode::REPORT_EVENT_TYPES, 2),
        (TermMode::REPORT_ALTERNATE_KEYS, 4),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16),
    ]
    .iter()
    .filter(|(m, _)| mode.contains(*m))
    .map(|(_, bit)| bit)
    .sum::<u8>();
    if kitty != 0 {
        out.extend_from_slice(format!("\x1b[>{kitty}u").as_bytes());
    }
}

fn write_cursor_style<T>(term: &Term<T>, config: &Config, out: &mut Vec<u8>) {
    let style = term.cursor_style();
    if style == config.default_cursor_style {
        return;
    }
    let base = match style.shape {
        CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => 1,
        CursorShape::Underline => 3,
        CursorShape::Beam => 5,
    };
    let code = if style.blinking { base } else { base + 1 };
    out.extend_from_slice(format!("\x1b[{code} q").as_bytes());
}

fn write_colors<T>(term: &Term<T>, out: &mut Vec<u8>) {
    let colors = term.colors();
    for index in 0..=258 {
        let Some(c) = colors[index] else { continue };
        let spec = format!("rgb:{:02x}/{:02x}/{:02x}", c.r, c.g, c.b);
        let seq = match index {
            256 => format!("\x1b]10;{spec}\x1b\\"),
            257 => format!("\x1b]11;{spec}\x1b\\"),
            258 => format!("\x1b]12;{spec}\x1b\\"),
            i => format!("\x1b]4;{i};{spec}\x1b\\"),
        };
        out.extend_from_slice(seq.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listener that ignores events (alacritty's own isn't `Clone`).
    #[derive(Clone, Copy)]
    struct Quiet;
    impl EventListener for Quiet {}

    fn term(cols: usize, rows: usize) -> Term<Quiet> {
        Term::new(config(), &Size(cols, rows), Quiet)
    }

    fn config() -> Config {
        Config {
            kitty_keyboard: true,
            scrolling_history: 1000,
            ..Config::default()
        }
    }

    fn feed(term: &mut Term<Quiet>, bytes: &[u8]) {
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(term, bytes);
    }

    /// Everything about a terminal a snapshot must reproduce, as data.
    fn fingerprint(term: &Term<Quiet>) -> Vec<String> {
        let grid = term.grid();
        let mut lines = Vec::new();
        for line in grid.topmost_line().0..=grid.bottommost_line().0 {
            let row = &grid[Line(line)];
            let cells: Vec<String> = (0..grid.columns())
                .map(|c| {
                    let cell = &row[Column(c)];
                    format!(
                        "{}{:?}/{:?}/{:?}/{:?}/{:?}/{:?}",
                        cell.c,
                        cell.zerowidth(),
                        cell.fg,
                        cell.bg,
                        cell.flags - Flags::WRAPLINE,
                        cell.underline_color(),
                        cell.hyperlink().map(|h| h.uri().to_string()),
                    )
                })
                .collect();
            let wrapped = row[Column(grid.columns() - 1)]
                .flags
                .contains(Flags::WRAPLINE);
            lines.push(format!("{line}:{wrapped}:{}", cells.join("|")));
        }
        lines.push(format!("cursor {:?}", grid.cursor.point));
        lines.push(format!("mode {:?}", *term.mode()));
        lines.push(format!("style {:?}", term.cursor_style()));
        lines.push(format!(
            "colors {:?}",
            (0..=258).map(|i| term.colors()[i]).collect::<Vec<_>>()
        ));
        lines
    }

    fn roundtrip(source: &mut Term<Quiet>) -> Term<Quiet> {
        let before = fingerprint(source);
        let snap = snapshot(source, &config(), Quiet, None);
        assert_eq!(fingerprint(source), before, "snapshot changed the source");
        let mut replica = term(source.columns(), source.screen_lines());
        feed(&mut replica, &snap);
        assert_eq!(fingerprint(&replica), before);
        replica
    }

    #[test]
    fn plain_shell_history_round_trips() {
        let mut t = term(20, 5);
        for i in 0..30 {
            feed(&mut t, format!("line {i}\r\n").as_bytes());
        }
        feed(&mut t, b"$ partial");
        roundtrip(&mut t);
    }

    #[test]
    fn attributes_colors_links_and_wide_chars_round_trip() {
        let mut t = term(24, 6);
        feed(
            &mut t,
            "\x1b[1;3;31mbold\x1b[0m \x1b[4:3;58;2;1;2;3mcurl\x1b[0m \x1b[38;2;9;8;7;48;5;200mrgb\x1b[0m\r\n\
             \x1b]8;id=x;https://e.com\x1b\\link\x1b]8;;\x1b\\ 界e\u{301} \x1b[7minv\x1b[0m\r\n\
             0123456789012345678901234567 wrapped\r\n\x1b[44m   \x1b[0m bg\r\n"
                .as_bytes(),
        );
        roundtrip(&mut t);
    }

    #[test]
    fn modes_cursor_style_and_palette_round_trip() {
        let mut t = term(20, 4);
        feed(
            &mut t,
            b"\x1b[?1h\x1b=\x1b[?2004h\x1b[?1000;1006h\x1b[?1004h\x1b[>5u\x1b[5 q\
              \x1b]4;1;rgb:12/34/56\x1b\\\x1b]11;rgb:01/02/03\x1b\\\x1b[3;7Hx\x1b[1;32m",
        );
        let replica = roundtrip(&mut t);
        // New text after attach keeps the pending attributes.
        let mut replica = replica;
        feed(&mut replica, b"y");
        feed(&mut t, b"y");
        assert_eq!(fingerprint(&replica), fingerprint(&t));
    }

    #[test]
    fn alternate_screen_and_its_primary_history_survive() {
        let mut t = term(20, 4);
        for i in 0..10 {
            feed(&mut t, format!("shell {i}\r\n").as_bytes());
        }
        feed(&mut t, b"$ vim\x1b[?1049h\x1b[H\x1b[2J~ editing\x1b[3;2H");
        let mut replica = roundtrip(&mut t);
        // Leaving the full-screen app restores the same shell screen on
        // both.
        feed(&mut t, b"\x1b[?1049l");
        feed(&mut replica, b"\x1b[?1049l");
        assert_eq!(fingerprint(&replica), fingerprint(&t));
    }

    #[test]
    fn prompt_marks_survive() {
        let mut t = term(20, 4);
        feed(
            &mut t,
            b"\x1b]8;id=cyberterm-prompt-1;cyberterm-mark:prompt?exit=\x1b\\$ \x1b]8;;\x1b\\ls\r\nout",
        );
        let replica = roundtrip(&mut t);
        assert_eq!(crate::shell::prompt_lines(&replica), vec![0]);
    }
}
