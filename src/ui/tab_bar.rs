// src/ui/tab_bar.rs
//
// The one-row tab bar: a cell frame drawn on the same renderer path as the
// panes, plus the column range of each tab so clicks can be mapped back.

use std::ops::Range;

use alacritty_terminal::term::cell::Flags;

use crate::frame::{Frame, RenderCell};

pub struct TabLabel {
    pub title: String,
    pub active: bool,
    /// A bell rang in a pane of this tab since it was last shown.
    pub bell: bool,
    pub zoomed: bool,
    pub broadcast: bool,
    /// A pane in this tab is in danger mode.
    pub danger: bool,
}

pub struct Colors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub dim: [u8; 3],
    pub accent: [u8; 3],
    pub alert: [u8; 3],
}

pub struct TabBar {
    pub frame: Frame,
    /// Columns covered by each tab, in tab order.
    pub hits: Vec<Range<usize>>,
}

/// `status` is shown right-aligned (e.g. a pending leader key).
pub fn build(labels: &[TabLabel], cols: usize, status: Option<&str>, colors: &Colors) -> TabBar {
    let mut cells = vec![RenderCell::blank(colors.fg, colors.bg); cols];
    let mut hits = Vec::new();
    let status: Vec<char> = status
        .map(|s| format!(" {s} ").chars().collect())
        .unwrap_or_default();
    let room = cols.saturating_sub(status.len());

    // Shrink titles evenly when everything doesn't fit.
    let count = labels.len().max(1);
    let fixed = 6; // " 12 " + markers + separator
    let max_title = (room / count).saturating_sub(fixed).clamp(3, 32);

    let mut col = 0;
    for (index, label) in labels.iter().enumerate() {
        let mut title: String = label.title.chars().take(max_title).collect();
        if label.title.chars().count() > max_title {
            title.pop();
            title.push('…');
        }
        let mut text = if label.danger {
            format!(" ⚠ {} {title}", index + 1)
        } else {
            format!(" {} {title}", index + 1)
        };
        if label.zoomed {
            text.push_str(" (z)");
        }
        if label.broadcast {
            text.push_str(" (b)");
        }
        if label.bell && !label.active {
            text.push_str(" •");
        }
        text.push(' ');
        let (fg, bg) = if label.active && label.danger {
            (colors.bg, colors.alert)
        } else if label.active {
            (colors.bg, colors.accent)
        } else if label.danger || label.bell {
            (colors.alert, colors.bg)
        } else {
            (colors.dim, colors.bg)
        };
        let start = col;
        for ch in text.chars() {
            if col >= room {
                break;
            }
            cells[col] = RenderCell {
                ch,
                fg,
                bg,
                flags: if label.active {
                    Flags::BOLD
                } else {
                    Flags::empty()
                },
                ..RenderCell::blank(fg, bg)
            };
            col += 1;
        }
        hits.push(start..col);
        // A one-cell gap between tabs.
        col += 1;
        if col >= room {
            break;
        }
    }

    for (i, ch) in status.iter().enumerate() {
        let c = cols - status.len() + i;
        cells[c] = RenderCell {
            ch: *ch,
            flags: Flags::BOLD,
            ..RenderCell::blank(colors.bg, colors.alert)
        };
    }

    TabBar {
        frame: Frame {
            cols,
            rows: 1,
            cells,
            cursor: None,
            display_offset: 0,
            history: 0,
            bg: colors.bg,
        },
        hits,
    }
}

/// Which tab a click on column `col` hit.
pub fn tab_at(bar: &TabBar, col: usize) -> Option<usize> {
    bar.hits.iter().position(|r| r.contains(&col))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Colors {
        Colors {
            fg: [200; 3],
            bg: [0; 3],
            dim: [100; 3],
            accent: [0, 255, 0],
            alert: [255, 0, 0],
        }
    }

    fn label(title: &str, active: bool) -> TabLabel {
        TabLabel {
            title: title.into(),
            active,
            bell: false,
            zoomed: false,
            broadcast: false,
            danger: false,
        }
    }

    fn text(bar: &TabBar) -> String {
        bar.frame.cells.iter().map(|c| c.ch).collect()
    }

    #[test]
    fn numbers_tabs_and_highlights_the_active_one() {
        let bar = build(
            &[label("zsh", true), label("logs", false)],
            40,
            None,
            &colors(),
        );
        assert!(text(&bar).starts_with(" 1 zsh   2 logs "));
        assert_eq!(bar.frame.cells[1].bg, [0, 255, 0]);
        assert_eq!(bar.frame.cells[8].fg, [100; 3]);
        assert_eq!(tab_at(&bar, 2), Some(0));
        assert_eq!(tab_at(&bar, 9), Some(1));
        assert_eq!(tab_at(&bar, 30), None);
    }

    #[test]
    fn markers_and_status() {
        let mut l = label("build", false);
        l.bell = true;
        let mut z = label("vim", true);
        z.zoomed = true;
        z.broadcast = true;
        let bar = build(&[z, l], 50, Some("LEADER"), &colors());
        let t = text(&bar);
        assert!(t.contains("vim (z) (b)"));
        assert!(t.contains("build •"));
        assert!(t.trim_end().ends_with("LEADER"));
    }

    #[test]
    fn long_titles_are_truncated_to_fit() {
        let labels: Vec<_> = (0..4).map(|i| label(&"x".repeat(100), i == 0)).collect();
        let bar = build(&labels, 60, None, &colors());
        assert_eq!(bar.hits.len(), 4);
        assert!(text(&bar).contains('…'));
    }
}
