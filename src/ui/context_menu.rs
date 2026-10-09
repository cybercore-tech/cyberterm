// src/ui/context_menu.rs
//
// The right-click menu: layout (placed at the pointer, flipped to stay on
// screen), hit-testing, and drawing onto a pane's cell grid. Drawing into
// the grid rather than a separate surface keeps it on the same renderer
// path as everything else, and its border uses box-drawing characters,
// which the renderer draws as seamless lines.

use crate::frame::{Frame, RenderCell};
use alacritty_terminal::term::cell::Flags;

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub label: String,
    /// Keyboard shortcut shown right-aligned, if any.
    pub hint: String,
    pub enabled: bool,
}

/// Where the menu sits on the grid, border included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize,
}

pub fn layout(items: &[Item], at_row: usize, at_col: usize, rows: usize, cols: usize) -> Layout {
    let inner = items
        .iter()
        .map(|i| {
            let hint = i.hint.chars().count();
            i.label.chars().count() + if hint > 0 { hint + 3 } else { 0 }
        })
        .max()
        .unwrap_or(0);
    let width = (inner + 4).min(cols);
    let height = (items.len() + 2).min(rows);
    // Open down-right of the pointer; flip up/left when it wouldn't fit.
    let row = if at_row + height <= rows {
        at_row
    } else {
        (at_row + 1).saturating_sub(height)
    };
    let col = if at_col + width <= cols {
        at_col
    } else {
        (at_col + 1).saturating_sub(width)
    };
    Layout {
        row,
        col,
        width,
        height,
    }
}

pub fn contains(layout: &Layout, row: usize, col: usize) -> bool {
    (layout.row..layout.row + layout.height).contains(&row)
        && (layout.col..layout.col + layout.width).contains(&col)
}

/// The item under a cell, if it's inside the menu body.
pub fn item_at(layout: &Layout, count: usize, row: usize, col: usize) -> Option<usize> {
    if !contains(layout, row, col) || row == layout.row {
        return None;
    }
    let index = row - layout.row - 1;
    (index < count && col > layout.col && col + 1 < layout.col + layout.width).then_some(index)
}

pub struct Colors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub dim: [u8; 3],
    pub accent: [u8; 3],
}

pub fn draw(
    frame: &mut Frame,
    layout: &Layout,
    items: &[Item],
    hover: Option<usize>,
    colors: &Colors,
) {
    let (cols, rows) = (frame.cols, frame.rows);
    let mut put = |row: usize, col: usize, ch: char, fg: [u8; 3], bg: [u8; 3]| {
        if row < rows && col < cols {
            frame.cells[row * cols + col] = RenderCell {
                ch,
                zerowidth: None,
                fg,
                bg,
                flags: Flags::empty(),
                underline_color: None,
                link_hover: false,
            };
        }
    };
    let Layout {
        row: top,
        col: left,
        width,
        height,
    } = *layout;
    if width < 2 || height < 2 {
        return;
    }
    let (right, bottom) = (left + width - 1, top + height - 1);
    for c in left..=right {
        let (t, b) = match c {
            _ if c == left => ('┌', '└'),
            _ if c == right => ('┐', '┘'),
            _ => ('─', '─'),
        };
        put(top, c, t, colors.accent, colors.bg);
        put(bottom, c, b, colors.accent, colors.bg);
    }
    for (i, item) in items.iter().enumerate().take(height - 2) {
        let row = top + 1 + i;
        let hovered = hover == Some(i) && item.enabled;
        let (fg, bg) = match (hovered, item.enabled) {
            (true, _) => (colors.bg, colors.accent),
            (false, true) => (colors.fg, colors.bg),
            (false, false) => (colors.dim, colors.bg),
        };
        put(row, left, '│', colors.accent, colors.bg);
        put(row, right, '│', colors.accent, colors.bg);
        let inner = width.saturating_sub(2);
        let hint: Vec<char> = item.hint.chars().collect();
        let label: Vec<char> = item.label.chars().collect();
        for x in 0..inner {
            // One space of padding each side; hint right-aligned.
            let ch = if x >= 1 && x - 1 < label.len() {
                label[x - 1]
            } else if !hint.is_empty() && x + 1 + hint.len() >= inner && x + 1 < inner {
                hint[x + 1 + hint.len() - inner]
            } else {
                ' '
            };
            let hint_fg = if hovered || !item.enabled {
                fg
            } else {
                colors.dim
            };
            let is_hint = !hint.is_empty() && x + 1 + hint.len() >= inner && x + 1 < inner;
            put(
                row,
                left + 1 + x,
                ch,
                if is_hint { hint_fg } else { fg },
                bg,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Item> {
        vec![
            Item {
                label: "Copy".into(),
                hint: "Ctrl+Shift+C".into(),
                enabled: false,
            },
            Item {
                label: "Paste".into(),
                hint: "Ctrl+Shift+V".into(),
                enabled: true,
            },
        ]
    }

    #[test]
    fn opens_at_the_pointer_and_flips_near_edges() {
        let l = layout(&items(), 2, 3, 24, 80);
        assert_eq!((l.row, l.col, l.height), (2, 3, 4));
        assert_eq!(l.width, "Paste".len() + 3 + "Ctrl+Shift+V".len() + 4);
        let flipped = layout(&items(), 23, 79, 24, 80);
        assert_eq!(flipped.row + flipped.height, 24);
        assert_eq!(flipped.col + flipped.width, 80);
    }

    #[test]
    fn hit_testing_skips_border() {
        let l = layout(&items(), 0, 0, 24, 80);
        assert_eq!(item_at(&l, 2, 0, 5), None);
        assert_eq!(item_at(&l, 2, 1, 5), Some(0));
        assert_eq!(item_at(&l, 2, 2, 5), Some(1));
        assert_eq!(item_at(&l, 2, 3, 5), None);
        assert_eq!(item_at(&l, 2, 1, 0), None);
        assert_eq!(item_at(&l, 2, 1, l.width), None);
    }

    #[test]
    fn draws_labels_hints_and_hover() {
        let mut frame = Frame::from_spans(&[], 40, 6, [0, 0, 0]);
        let its = items();
        let l = layout(&its, 0, 0, 6, 40);
        let colors = Colors {
            fg: [200; 3],
            bg: [10; 3],
            dim: [90; 3],
            accent: [0, 255, 255],
        };
        draw(&mut frame, &l, &its, Some(1), &colors);
        let row = |r: usize| -> String { frame.row(r)[..l.width].iter().map(|c| c.ch).collect() };
        assert_eq!(row(0).chars().next(), Some('┌'));
        assert!(row(1).starts_with("│ Copy"));
        assert!(row(1).ends_with("Ctrl+Shift+C │"));
        assert!(row(2).starts_with("│ Paste"));
        // Disabled item is dim; hovered item is drawn in accent.
        assert_eq!(frame.row(1)[2].fg, [90; 3]);
        assert_eq!(frame.row(2)[2].bg, [0, 255, 255]);
    }
}
