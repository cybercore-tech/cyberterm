// src/app/overlays.rs
//
// Two in-window tools drawn over the focused pane:
// - Find (Ctrl+Shift+F): a search bar on the pane's last row; matches in
//   the scrollback highlighted, Enter / Shift+Enter step through them.
// - History (Ctrl+Shift+H): the saved command history, searchable by
//   command and output, with a preview of the selected command's output.

use super::*;
use crate::find::{find_all, Match};
use crate::history;

pub(super) struct FindState {
    query: String,
    matches: Vec<Match>,
    current: Option<usize>,
    /// (history size, cursor line) the matches were computed at; new
    /// output shifts line numbers, so they're recomputed when it changes.
    computed_at: (usize, i32),
}

pub(super) struct HistoryUi {
    query: String,
    store: Option<history::Store>,
    entries: Vec<history::Entry>,
    selected: usize,
    preview: Option<(i64, String)>,
    error: Option<String>,
}

fn ago(ms: u64) -> String {
    let now = crate::shell::tap::now_ms();
    let s = now.saturating_sub(ms) / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

impl App {
    // ------------------------------------------------------------------
    // Find
    // ------------------------------------------------------------------

    pub(super) fn toggle_find(&mut self) {
        if self.find.take().is_none() {
            self.find = Some(FindState {
                query: String::new(),
                matches: Vec::new(),
                current: None,
                computed_at: (0, 0),
            });
        }
        self.request_redraw();
    }

    /// Keys while the find bar is open. Returns false for keys it doesn't
    /// use (they're handled normally, e.g. keybindings).
    pub(super) fn find_key(&mut self, event: &KeyEvent) -> bool {
        if event.state != ElementState::Pressed {
            return true;
        }
        if self.mods.control_key() || self.mods.alt_key() || self.mods.super_key() {
            return false;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.find = None;
            }
            Key::Named(NamedKey::Enter) => self.find_step(!self.mods.shift_key()),
            Key::Named(NamedKey::Backspace) => {
                if let Some(f) = &mut self.find {
                    f.query.pop();
                }
                self.find_recompute(true);
            }
            _ => {
                let Some(text) = event
                    .text
                    .as_deref()
                    .filter(|t| !t.chars().any(char::is_control))
                else {
                    return true;
                };
                if let Some(f) = &mut self.find {
                    f.query.push_str(text);
                }
                self.find_recompute(true);
            }
        }
        self.request_redraw();
        true
    }

    /// Recomputes matches; with `jump`, selects the newest match at or
    /// above the bottom of the view and scrolls to it.
    fn find_recompute(&mut self, jump: bool) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let (matches, at, bottom) = {
            let term = pane.session.term.lock();
            let query = self
                .find
                .as_ref()
                .map(|f| f.query.clone())
                .unwrap_or_default();
            let bottom = term.screen_lines() as i32 - 1 - term.grid().display_offset() as i32;
            (
                find_all(&*term, &query),
                (term.grid().history_size(), term.grid().cursor.point.line.0),
                bottom,
            )
        };
        let Some(f) = &mut self.find else { return };
        f.computed_at = at;
        if jump {
            f.current = matches
                .iter()
                .rposition(|m| m.line <= bottom)
                .or_else(|| matches.len().checked_sub(1));
        } else {
            f.current = f.current.filter(|&c| c < matches.len());
        }
        f.matches = matches;
        if jump {
            self.find_scroll_to_current();
        }
    }

    fn find_step(&mut self, older: bool) {
        let Some(f) = &mut self.find else { return };
        let n = f.matches.len();
        if n == 0 {
            return;
        }
        f.current = Some(match f.current {
            Some(c) if older => (c + n - 1) % n,
            Some(c) => (c + 1) % n,
            None => n - 1,
        });
        self.find_scroll_to_current();
    }

    fn find_scroll_to_current(&mut self) {
        let Some(m) = self
            .find
            .as_ref()
            .and_then(|f| f.current.and_then(|c| f.matches.get(c)).copied())
        else {
            return;
        };
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let mut term = pane.session.term.lock();
        let rows = term.screen_lines() as i32;
        let offset = term.grid().display_offset() as i32;
        let (top, bottom) = (-offset, rows - 1 - offset);
        if m.line < top || m.line > bottom {
            let history = term.grid().history_size() as i32;
            let want = (rows / 2 - m.line).clamp(0, history);
            term.scroll_display(Scroll::Delta(want - offset));
        }
    }

    /// Brings overlay state up to date before a frame is drawn: find
    /// matches after new output moved lines, the history preview after the
    /// selection changed.
    pub(super) fn refresh_overlays(&mut self) {
        let stale = match (self.find.as_ref(), self.focused_pane()) {
            (Some(f), Some(p)) => {
                let term = p.session.term.lock();
                f.computed_at != (term.grid().history_size(), term.grid().cursor.point.line.0)
            }
            _ => false,
        };
        if stale {
            self.find_recompute(false);
        }
        if let Some(ui) = &mut self.history_ui {
            if let (Some(entry), Some(store)) = (ui.entries.get(ui.selected), &ui.store) {
                if ui.preview.as_ref().is_none_or(|(id, _)| *id != entry.id) {
                    let text = store.output(entry.id).ok().flatten().unwrap_or_default();
                    ui.preview = Some((entry.id, text));
                }
            }
        }
    }

    /// Highlights matches and draws the find bar into the focused pane's
    /// frame.
    pub(super) fn draw_find(&self, frame: &mut Frame) {
        let Some(f) = &self.find else { return };
        let offset = frame.display_offset as i32;
        let hit = frame::hex_to_rgb(self.palette.ansi[3]);
        let current = frame::hex_to_rgb(self.palette.cursor);
        let bg = frame.bg;
        let cols = frame.cols;
        for (i, m) in f.matches.iter().enumerate() {
            let row = m.line + offset;
            if row < 0 || row as usize >= frame.rows.saturating_sub(1) {
                continue;
            }
            let color = if Some(i) == f.current { current } else { hit };
            for col in m.start..m.end.min(cols) {
                let cell = &mut frame.cells[row as usize * cols + col];
                cell.bg = color;
                cell.fg = bg;
            }
        }
        let fg = frame::hex_to_rgb(self.palette.fg);
        let row = frame.rows - 1;
        let count = match (f.current, f.matches.len()) {
            (_, 0) if !f.query.is_empty() => "no matches".to_string(),
            (Some(c), n) => format!("{}/{n}", c + 1),
            _ => String::new(),
        };
        let mut col = frame.put(row, 0, " Find: ", bg, fg);
        col = frame.put(row, col, &f.query, bg, fg);
        col = frame.put(row, col, "▏ ", bg, fg);
        col = frame.put(row, col, &count, bg, fg);
        col = frame.put(
            row,
            col,
            "   Enter older · Shift+Enter newer · Esc close",
            bg,
            fg,
        );
        frame.fill(row, col, bg, fg);
        frame.cursor = None;
    }

    // ------------------------------------------------------------------
    // History browser
    // ------------------------------------------------------------------

    pub(super) fn toggle_history_ui(&mut self) {
        if self.history_ui.take().is_some() {
            self.request_redraw();
            return;
        }
        let mut ui = HistoryUi {
            query: String::new(),
            store: None,
            entries: Vec::new(),
            selected: 0,
            preview: None,
            error: None,
        };
        match history::Store::open(&history::default_path()) {
            Ok(store) => ui.store = Some(store),
            Err(e) => ui.error = Some(format!("history unavailable: {e}")),
        }
        self.history_ui = Some(ui);
        self.history_requery();
        self.request_redraw();
    }

    fn history_requery(&mut self) {
        let Some(ui) = &mut self.history_ui else {
            return;
        };
        let Some(store) = &ui.store else { return };
        let query = history::Query {
            text: Some(ui.query.clone()).filter(|q| !q.trim().is_empty()),
            limit: 300,
            ..history::Query::default()
        };
        match store.search(&query) {
            Ok(entries) => {
                ui.entries = entries;
                ui.error = None;
            }
            Err(e) => ui.error = Some(e.to_string()),
        }
        ui.selected = 0;
        ui.preview = None;
    }

    pub(super) fn history_key(&mut self, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.control_key();
        let Some(ui) = &mut self.history_ui else {
            return;
        };
        let n = ui.entries.len();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.history_ui = None,
            Key::Named(NamedKey::ArrowDown) if n > 0 => ui.selected = (ui.selected + 1).min(n - 1),
            Key::Named(NamedKey::ArrowUp) => ui.selected = ui.selected.saturating_sub(1),
            Key::Named(NamedKey::PageDown) if n > 0 => ui.selected = (ui.selected + 10).min(n - 1),
            Key::Named(NamedKey::PageUp) => ui.selected = ui.selected.saturating_sub(10),
            Key::Named(NamedKey::Enter) => {
                let Some(entry) = ui.entries.get(ui.selected).cloned() else {
                    return;
                };
                self.history_ui = None;
                // Inserted as a paste so a multi-line command doesn't run
                // line by line; Ctrl+Enter also presses Enter.
                self.paste(&entry.command);
                if ctrl {
                    if let Some(pane) = self.focused_pane() {
                        pane.session.write(&b"\r"[..]);
                    }
                }
            }
            Key::Named(NamedKey::Tab) => {
                let Some(entry) = ui.entries.get(ui.selected).cloned() else {
                    return;
                };
                let output = ui
                    .store
                    .as_ref()
                    .and_then(|s| s.output(entry.id).ok().flatten())
                    .unwrap_or_default();
                self.history_ui = None;
                self.show_text_in_pager(&output, entry.id);
            }
            Key::Named(NamedKey::Backspace) => {
                ui.query.pop();
                self.history_requery();
            }
            _ => {
                if ctrl {
                    return;
                }
                if let Some(text) = event
                    .text
                    .as_deref()
                    .filter(|t| !t.chars().any(char::is_control))
                {
                    ui.query.push_str(text);
                    self.history_requery();
                }
            }
        }
        self.request_redraw();
    }

    /// The history browser as a full-pane frame.
    pub(super) fn draw_history(&self, cols: usize, rows: usize) -> Option<Frame> {
        let ui = self.history_ui.as_ref()?;
        let fg = frame::hex_to_rgb(self.palette.fg);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let dim = frame::hex_to_rgb(self.palette.ansi[8]);
        let accent = frame::hex_to_rgb(self.palette.cursor);
        let ok = frame::hex_to_rgb(self.palette.ansi[2]);
        let bad = frame::hex_to_rgb(self.palette.ansi[1]);
        let mut f = Frame::blank(cols, rows, fg, bg);

        f.put(0, 1, "History", accent, bg);
        f.put(0, 9, "· type to search commands and their output", dim, bg);
        let col = f.put(1, 1, "> ", accent, bg);
        let col = f.put(1, col, &ui.query, fg, bg);
        f.put(1, col, "▏", accent, bg);

        let list_rows = (rows.saturating_sub(4) * 3 / 5).max(3);
        let first = ui.selected.saturating_sub(list_rows.saturating_sub(1));
        if let Some(e) = &ui.error {
            f.put(3, 1, e, bad, bg);
        } else if ui.entries.is_empty() && ui.query.trim().is_empty() {
            f.put(
                3,
                1,
                "No saved commands yet (needs shell integration).",
                dim,
                bg,
            );
        } else if ui.entries.is_empty() {
            f.put(3, 1, "No commands or output match.", dim, bg);
        }
        let home = std::env::var("HOME").unwrap_or_default();
        for (i, e) in ui.entries.iter().enumerate().skip(first).take(list_rows) {
            let row = 3 + i - first;
            let is_sel = i == ui.selected;
            let (rfg, rbg) = if is_sel { (bg, accent) } else { (fg, bg) };
            let (mark, mark_fg) = match e.exit {
                Some(0) => ("✓", if is_sel { bg } else { ok }),
                Some(_) => ("✗", if is_sel { bg } else { bad }),
                None => ("·", rfg),
            };
            let cwd = e
                .cwd
                .clone()
                .map(|c| match c.strip_prefix(&home) {
                    Some(rest) if !home.is_empty() => format!("~{rest}"),
                    _ => c,
                })
                .unwrap_or_default();
            let mut col = f.put(row, 0, " ", rfg, rbg);
            col = f.put(row, col, mark, mark_fg, rbg);
            col = f.put(
                row,
                col,
                &format!(" {:>4} ", ago(e.started_ms)),
                if is_sel { bg } else { dim },
                rbg,
            );
            let cwd_short: String = cwd
                .chars()
                .rev()
                .take(24)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            col = f.put(
                row,
                col,
                &format!("{cwd_short:<24} "),
                if is_sel { bg } else { dim },
                rbg,
            );
            col = f.put(row, col, &e.command.replace('\n', " ⏎ "), rfg, rbg);
            f.fill(row, col, rfg, rbg);
        }

        let preview_top = 3 + list_rows + 1;
        if preview_top + 1 < rows {
            f.put(preview_top - 1, 0, &"─".repeat(cols), dim, bg);
            if let Some((_, text)) = &ui.preview {
                let lines: Vec<&str> = text.lines().collect();
                let space = rows - preview_top - 1;
                for (i, line) in lines.iter().take(space).enumerate() {
                    f.put(preview_top + i, 1, line, dim, bg);
                }
            }
        }
        f.put(
            rows - 1,
            1,
            "↑↓ select · Enter insert · Ctrl+Enter run · Tab output in a pane · Esc close",
            dim,
            bg,
        );
        Some(f)
    }
}
