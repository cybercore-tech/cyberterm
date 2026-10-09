// src/frame.rs
//
// Turns terminal state into a plain grid of resolved cells (`Frame`) for the
// GPU renderer: colors resolved against the theme and any OSC 4/10/11/12
// overrides, attributes applied, selection and cursor folded in. Nothing in
// here touches the GPU, so it's covered by headless tests that feed real
// escape sequences into a real `Term`.

use std::ops::Range;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape as TermCursorShape, NamedColor,
};
use alacritty_terminal::Term;

/// Theme colors as 0xRRGGBB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub ansi: [u32; 16],
    pub fg: u32,
    pub bg: u32,
    pub cursor: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            ansi: [0; 16],
            fg: 0xffffff,
            bg: 0x000000,
            cursor: 0xffffff,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderCell {
    pub ch: char,
    /// Combining marks / variation selectors stacked on `ch`.
    pub zerowidth: Option<Box<[char]>>,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    /// alacritty_terminal's attribute flags (bold, italic, underline
    /// styles, strikeout, wide-char markers, ...).
    pub flags: Flags,
    pub underline_color: Option<[u8; 3]>,
    /// Part of the hyperlink under the mouse pointer.
    pub link_hover: bool,
}

impl RenderCell {
    pub fn blank(fg: [u8; 3], bg: [u8; 3]) -> Self {
        Self {
            ch: ' ',
            zerowidth: None,
            fg,
            bg,
            flags: Flags::empty(),
            underline_color: None,
            link_hover: false,
        }
    }

    /// The second half of a double-width character; draws no glyph.
    pub fn is_spacer(&self) -> bool {
        self.flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Beam,
    Underline,
    Hollow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorDraw {
    pub col: usize,
    pub row: usize,
    pub shape: CursorShape,
    pub color: [u8; 3],
    pub wide: bool,
}

pub struct Frame {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<RenderCell>,
    pub cursor: Option<CursorDraw>,
    /// Lines scrolled back from the live screen.
    pub display_offset: usize,
    /// Lines of scrollback available above the screen.
    pub history: usize,
    pub bg: [u8; 3],
}

impl Frame {
    pub fn row(&self, row: usize) -> &[RenderCell] {
        &self.cells[row * self.cols..(row + 1) * self.cols]
    }

    /// The text of a row plus, for each char, the column it sits in.
    #[cfg(test)]
    pub fn row_text(&self, row: usize) -> (String, Vec<usize>) {
        let mut text = String::new();
        let mut cols = Vec::new();
        for (col, cell) in self.row(row).iter().enumerate() {
            if cell.is_spacer() {
                continue;
            }
            text.push(cell.ch);
            cols.push(col);
        }
        (text, cols)
    }

    /// A frame of plain colored text, used for overlays like the theme menu.
    pub fn from_spans(
        lines: &[Vec<(String, [u8; 3])>],
        cols: usize,
        rows: usize,
        bg: [u8; 3],
    ) -> Self {
        let mut cells = vec![RenderCell::blank([0xff; 3], bg); cols * rows];
        for (row, line) in lines.iter().enumerate().take(rows) {
            let mut col = 0;
            for (text, color) in line {
                for ch in text.chars() {
                    if col >= cols {
                        break;
                    }
                    cells[row * cols + col].ch = ch;
                    cells[row * cols + col].fg = *color;
                    col += 1;
                }
            }
        }
        Self {
            cols,
            rows,
            cells,
            cursor: None,
            display_offset: 0,
            history: 0,
            bg,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CursorOptions {
    /// False during the "off" half of a blink.
    pub visible: bool,
    pub focused: bool,
    pub unfocused_hollow: bool,
}

pub struct FrameOptions<'a> {
    pub palette: &'a Palette,
    pub bold_is_bright: bool,
    pub cursor: CursorOptions,
    /// Viewport (row, column range) spans to underline as a hovered link.
    pub link_hover: &'a [(usize, Range<usize>)],
}

pub fn hex_to_rgb(color: u32) -> [u8; 3] {
    [(color >> 16) as u8, (color >> 8) as u8, color as u8]
}

/// The xterm 256-color formula: a 6x6x6 cube (16-231) plus a 24-step
/// grayscale ramp (232-255).
pub fn indexed_256_to_rgb(idx: u8) -> [u8; 3] {
    if idx >= 232 {
        let level = 8 + (idx - 232) * 10;
        [level, level, level]
    } else {
        let i = idx.saturating_sub(16);
        let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        [scale(i / 36), scale((i / 6) % 6), scale(i % 6)]
    }
}

/// The color reported for an OSC 4/10/11/12 query the program hasn't
/// overridden itself (alacritty_terminal answers overridden ones directly).
/// 256/257/258 are foreground/background/cursor, as in alacritty_terminal.
pub fn query_color(palette: &Palette, index: usize) -> [u8; 3] {
    match index {
        256 => hex_to_rgb(palette.fg),
        257 => hex_to_rgb(palette.bg),
        258 => hex_to_rgb(palette.cursor),
        i if i < 16 => hex_to_rgb(palette.ansi[i]),
        i if i <= 255 => indexed_256_to_rgb(i as u8),
        _ => hex_to_rgb(palette.fg),
    }
}

fn dim(rgb: [u8; 3]) -> [u8; 3] {
    rgb.map(|c| (c as f32 * 0.66) as u8)
}

struct Resolver<'a> {
    palette: &'a Palette,
    overrides: &'a Colors,
}

impl Resolver<'_> {
    fn indexed(&self, idx: usize) -> [u8; 3] {
        if let Some(rgb) = self.overrides[idx] {
            return [rgb.r, rgb.g, rgb.b];
        }
        if idx < 16 {
            hex_to_rgb(self.palette.ansi[idx])
        } else {
            indexed_256_to_rgb(idx as u8)
        }
    }

    fn named(&self, named: NamedColor) -> [u8; 3] {
        use NamedColor::*;
        if let Some(rgb) = self.overrides[named] {
            return [rgb.r, rgb.g, rgb.b];
        }
        match named {
            Foreground | BrightForeground => hex_to_rgb(self.palette.fg),
            DimForeground => dim(hex_to_rgb(self.palette.fg)),
            Background => hex_to_rgb(self.palette.bg),
            Cursor => hex_to_rgb(self.palette.cursor),
            DimBlack | DimRed | DimGreen | DimYellow | DimBlue | DimMagenta | DimCyan
            | DimWhite => dim(self.indexed(named as usize - DimBlack as usize)),
            _ => self.indexed(named as usize),
        }
    }

    fn color(&self, color: AnsiColor) -> [u8; 3] {
        match color {
            AnsiColor::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
            AnsiColor::Indexed(idx) => self.indexed(idx as usize),
            AnsiColor::Named(named) => self.named(named),
        }
    }

    /// Foreground with the bold/dim attribute rules applied.
    fn fg(&self, color: AnsiColor, flags: Flags, bold_is_bright: bool) -> [u8; 3] {
        let bold = flags.contains(Flags::BOLD);
        let dimmed = flags.contains(Flags::DIM);
        match color {
            AnsiColor::Named(named) if dimmed => self.named(named.to_dim()),
            AnsiColor::Named(named) if bold && bold_is_bright => self.named(named.to_bright()),
            AnsiColor::Indexed(idx) if bold && bold_is_bright && idx < 8 => {
                self.indexed(idx as usize + 8)
            }
            other if dimmed => dim(self.color(other)),
            other => self.color(other),
        }
    }
}

pub fn build<T: EventListener>(term: &Term<T>, opts: &FrameOptions<'_>) -> Frame {
    let content = term.renderable_content();
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let offset = content.display_offset as i32;
    let resolver = Resolver {
        palette: opts.palette,
        overrides: content.colors,
    };
    let default_fg = resolver.named(NamedColor::Foreground);
    let default_bg = resolver.named(NamedColor::Background);

    let mut cells = vec![RenderCell::blank(default_fg, default_bg); cols * rows];
    let selection = content.selection;

    for indexed in content.display_iter {
        let row = indexed.point.line.0 + offset;
        let col = indexed.point.column.0;
        if row < 0 || row as usize >= rows || col >= cols {
            continue;
        }
        let cell = indexed.cell;
        let mut fg = resolver.fg(cell.fg, cell.flags, opts.bold_is_bright);
        let mut bg = resolver.color(cell.bg);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if selection.is_some_and(|s| s.contains(indexed.point)) {
            std::mem::swap(&mut fg, &mut bg);
        }
        cells[row as usize * cols + col] = RenderCell {
            ch: cell.c,
            zerowidth: cell.zerowidth().filter(|z| !z.is_empty()).map(Box::from),
            fg,
            bg,
            flags: cell.flags,
            underline_color: cell.underline_color().map(|c| resolver.color(c)),
            link_hover: false,
        };
    }

    for (row, range) in opts.link_hover {
        if *row < rows {
            for col in range.clone().filter(|c| *c < cols) {
                cells[row * cols + col].link_hover = true;
            }
        }
    }

    let cursor = cursor_draw(
        &content.cursor,
        offset,
        rows,
        cols,
        &resolver,
        opts,
        &mut cells,
    );

    Frame {
        cols,
        rows,
        cells,
        cursor,
        display_offset: content.display_offset,
        history: grid.history_size(),
        bg: default_bg,
    }
}

fn cursor_draw(
    cursor: &alacritty_terminal::term::RenderableCursor,
    offset: i32,
    rows: usize,
    cols: usize,
    resolver: &Resolver<'_>,
    opts: &FrameOptions<'_>,
    cells: &mut [RenderCell],
) -> Option<CursorDraw> {
    let row = cursor.point.line.0 + offset;
    let col = cursor.point.column.0;
    if row < 0 || row as usize >= rows || col >= cols {
        return None;
    }
    let row = row as usize;
    let mut shape = match cursor.shape {
        TermCursorShape::Hidden => return None,
        TermCursorShape::Block => CursorShape::Block,
        TermCursorShape::Beam => CursorShape::Beam,
        TermCursorShape::Underline => CursorShape::Underline,
        TermCursorShape::HollowBlock => CursorShape::Hollow,
    };
    if !opts.cursor.focused && opts.cursor.unfocused_hollow {
        shape = CursorShape::Hollow;
    } else if opts.cursor.focused && !opts.cursor.visible {
        return None;
    }

    let color = resolver.named(NamedColor::Cursor);
    let index = row * cols + col;
    let wide = cells[index].flags.contains(Flags::WIDE_CHAR);
    if shape == CursorShape::Block {
        // The cell under a block cursor is drawn inverted: cursor color
        // behind, the cell's own background color for the glyph.
        let span = if wide && col + 1 < cols { 2 } else { 1 };
        for cell in &mut cells[index..index + span] {
            cell.fg = cell.bg;
            cell.bg = color;
        }
    }
    Some(CursorDraw {
        col,
        row,
        shape,
        color,
        wide,
    })
}

/// Plain text of grid lines `from..=to` (negative = scrollback). Lines the
/// terminal soft-wrapped are joined back together, trailing blanks are
/// trimmed, and the second half of wide characters is skipped.
pub fn lines_text<T>(term: &Term<T>, from: i32, to: i32) -> String {
    let grid = term.grid();
    let cols = grid.columns();
    let from = from.max(grid.topmost_line().0);
    let to = to.min(grid.bottommost_line().0);
    let mut out = String::new();
    let mut line_buf = String::new();
    for line in from..=to {
        let row = &grid[Line(line)];
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            line_buf.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                line_buf.extend(extra.iter());
            }
        }
        let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        if !wrapped {
            out.push_str(line_buf.trim_end());
            out.push('\n');
            line_buf.clear();
        }
    }
    out.push_str(line_buf.trim_end());
    // Drop blank lines below the last output.
    let trimmed = out.trim_end_matches('\n').len();
    out.truncate(trimmed);
    out
}

/// Grid point (scrollback-aware) for a viewport cell.
pub fn viewport_to_point(row: usize, col: usize, display_offset: usize) -> Point {
    Point::new(Line(row as i32 - display_offset as i32), Column(col))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::Side;
    use alacritty_terminal::selection::{Selection, SelectionType};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

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

    pub fn test_term(cols: usize, rows: usize) -> Term<VoidListener> {
        let config = Config {
            kitty_keyboard: true,
            ..Config::default()
        };
        Term::new(config, &Size(cols, rows), VoidListener)
    }

    pub fn feed(term: &mut Term<VoidListener>, bytes: &[u8]) {
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(term, bytes);
    }

    fn palette() -> Palette {
        let mut ansi = [0u32; 16];
        for (i, slot) in ansi.iter_mut().enumerate() {
            *slot = 0x101010 * i as u32;
        }
        Palette {
            ansi,
            fg: 0xc0c0c0,
            bg: 0x050505,
            cursor: 0x00ff00,
        }
    }

    fn render(term: &Term<VoidListener>) -> Frame {
        let palette = palette();
        build(
            term,
            &FrameOptions {
                palette: &palette,
                bold_is_bright: false,
                cursor: CursorOptions {
                    visible: true,
                    focused: true,
                    unfocused_hollow: true,
                },
                link_hover: &[],
            },
        )
    }

    fn line(frame: &Frame, row: usize) -> String {
        frame.row_text(row).0.trim_end().to_string()
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"hello\r\nworld");
        let frame = render(&term);
        assert_eq!(line(&frame, 0), "hello");
        assert_eq!(line(&frame, 1), "world");
        assert_eq!(frame.row(0)[0].fg, [0xc0; 3]);
        assert_eq!(frame.bg, [5, 5, 5]);
    }

    #[test]
    fn sgr_attributes_and_colors_resolve() {
        let mut term = test_term(20, 2);
        feed(
            &mut term,
            b"\x1b[1;3;31mA\x1b[0m\x1b[4:3;58;2;1;2;3mB\x1b[0m\x1b[38;2;10;20;30;48;5;196mC\x1b[0m\x1b[9;2;32mD",
        );
        let frame = render(&term);
        let row = frame.row(0);
        assert!(row[0].flags.contains(Flags::BOLD | Flags::ITALIC));
        assert_eq!(row[0].fg, hex_to_rgb(0x101010));
        assert!(row[1].flags.contains(Flags::UNDERCURL));
        assert_eq!(row[1].underline_color, Some([1, 2, 3]));
        assert_eq!(row[2].fg, [10, 20, 30]);
        assert_eq!(row[2].bg, indexed_256_to_rgb(196));
        assert!(row[3].flags.contains(Flags::STRIKEOUT));
        assert_eq!(row[3].fg, dim(hex_to_rgb(0x202020)));
    }

    #[test]
    fn bold_is_bright_only_when_enabled() {
        let mut term = test_term(5, 1);
        feed(&mut term, b"\x1b[1;34mX");
        assert_eq!(render(&term).row(0)[0].fg, hex_to_rgb(0x404040));
        let palette = palette();
        let frame = build(
            &term,
            &FrameOptions {
                palette: &palette,
                bold_is_bright: true,
                cursor: CursorOptions {
                    visible: true,
                    focused: true,
                    unfocused_hollow: true,
                },
                link_hover: &[],
            },
        );
        assert_eq!(frame.row(0)[0].fg, hex_to_rgb(0xc0c0c0));
    }

    #[test]
    fn osc_color_overrides_beat_the_theme() {
        let mut term = test_term(5, 1);
        feed(
            &mut term,
            b"\x1b]4;1;rgb:aa/bb/cc\x07\x1b]11;rgb:01/02/03\x07\x1b[31mX",
        );
        let frame = render(&term);
        assert_eq!(frame.row(0)[0].fg, [0xaa, 0xbb, 0xcc]);
        assert_eq!(frame.bg, [1, 2, 3]);
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        let mut term = test_term(10, 1);
        feed(&mut term, "a界b".as_bytes());
        let frame = render(&term);
        let row = frame.row(0);
        assert_eq!(row[1].ch, '界');
        assert!(row[1].flags.contains(Flags::WIDE_CHAR));
        assert!(row[2].is_spacer());
        assert_eq!(row[3].ch, 'b');
        let (text, cols) = frame.row_text(0);
        assert!(text.starts_with("a界b"));
        assert_eq!(&cols[..3], &[0, 1, 3]);
    }

    #[test]
    fn combining_marks_stay_with_their_base_character() {
        let mut term = test_term(10, 1);
        feed(&mut term, "e\u{301}x".as_bytes());
        let frame = render(&term);
        assert_eq!(frame.row(0)[0].zerowidth.as_deref(), Some(&['\u{301}'][..]));
        assert_eq!(frame.row(0)[1].ch, 'x');
    }

    #[test]
    fn cursor_shape_follows_decscusr_and_focus() {
        let mut term = test_term(10, 2);
        feed(&mut term, b"ab");
        let frame = render(&term);
        let cursor = frame.cursor.unwrap();
        assert_eq!(
            (cursor.row, cursor.col, cursor.shape),
            (0, 2, CursorShape::Block)
        );
        // Block cursor inverts its cell.
        assert_eq!(frame.row(0)[2].bg, [0, 0xff, 0]);

        feed(&mut term, b"\x1b[6 q");
        assert_eq!(render(&term).cursor.unwrap().shape, CursorShape::Beam);
        feed(&mut term, b"\x1b[4 q");
        assert_eq!(render(&term).cursor.unwrap().shape, CursorShape::Underline);
        feed(&mut term, b"\x1b[?25l");
        assert!(render(&term).cursor.is_none());

        feed(&mut term, b"\x1b[?25h");
        let palette = palette();
        let unfocused = build(
            &term,
            &FrameOptions {
                palette: &palette,
                bold_is_bright: false,
                cursor: CursorOptions {
                    visible: false,
                    focused: false,
                    unfocused_hollow: true,
                },
                link_hover: &[],
            },
        );
        assert_eq!(unfocused.cursor.unwrap().shape, CursorShape::Hollow);
    }

    #[test]
    fn scrollback_viewport_follows_display_offset() {
        let mut term = test_term(10, 3);
        for i in 0..10 {
            feed(&mut term, format!("line{i}\r\n").as_bytes());
        }
        let frame = render(&term);
        assert_eq!(line(&frame, 0), "line8");
        assert_eq!(frame.history, 8);

        term.scroll_display(alacritty_terminal::grid::Scroll::Delta(5));
        let frame = render(&term);
        assert_eq!(frame.display_offset, 5);
        assert_eq!(line(&frame, 0), "line3");
        // The live cursor is below the viewport while scrolled back.
        assert!(frame.cursor.is_none());
        assert_eq!(viewport_to_point(0, 0, 5), Point::new(Line(-5), Column(0)));
    }

    #[test]
    fn selection_is_drawn_inverted_and_copied_as_text() {
        let mut term = test_term(12, 2);
        feed(&mut term, b"select me\r\nnot me");
        let mut sel = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Side::Left,
        );
        sel.update(Point::new(Line(0), Column(5)), Side::Right);
        term.selection = Some(sel);
        let frame = render(&term);
        assert_eq!(frame.row(0)[0].bg, [0xc0; 3]);
        assert_eq!(frame.row(1)[0].bg, [5, 5, 5]);
        assert_eq!(term.selection_to_string().as_deref(), Some("select"));
    }

    #[test]
    fn osc8_hyperlinks_are_kept_on_cells() {
        let mut term = test_term(20, 1);
        feed(
            &mut term,
            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ x",
        );
        let grid = term.grid();
        let link = grid[Line(0)][Column(0)].hyperlink().unwrap();
        assert_eq!(link.uri(), "https://example.com");
        assert!(grid[Line(0)][Column(5)].hyperlink().is_none());
    }

    #[test]
    fn lines_text_joins_wraps_and_reaches_into_scrollback() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"first\r\n0123456789abc\r\nlast");
        // 0123456789abc wrapped onto two rows; with 3 rows and 4 lines of
        // output, "first" has scrolled into history.
        let all = lines_text(&term, -10, 2);
        assert_eq!(all, "first\n0123456789abc\nlast");
        assert_eq!(lines_text(&term, 1, 2), "abc\nlast");
        let mut term = test_term(10, 2);
        feed(&mut term, "界x".as_bytes());
        assert_eq!(lines_text(&term, 0, 1), "界x");
    }

    #[test]
    fn spans_build_overlay_frames() {
        let frame = Frame::from_spans(&[vec![("hi".into(), [1, 2, 3])]], 4, 2, [9, 9, 9]);
        assert_eq!(frame.row(0)[1].ch, 'i');
        assert_eq!(frame.row(0)[1].fg, [1, 2, 3]);
        assert_eq!(frame.row(1)[0].bg, [9, 9, 9]);
    }
}
