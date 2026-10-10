// src/app/agent_changes.rs
//
// The Changes view in the window (Ctrl+Shift+M, or `c` in the Flight log
// panel): what the agent session in this tab changed (src/changes.rs),
// drawn over the tab by src/ui/changes_view.rs. It refreshes every couple
// of seconds while open, so it follows an agent that's still working.
// e opens a file in your editor at its first change, r puts it back the
// way it was when the agent started (after asking), y copies its path.

use super::*;
use crate::changes::Changes;

/// How often an open view re-reads the worktree.
const REFRESH: Duration = Duration::from_secs(2);

pub(super) struct ChangesUi {
    session: crate::agent::Session,
    data: Changes,
    file: usize,
    scroll: usize,
    /// A file waiting for "y" to be reverted.
    confirm: Option<String>,
    refreshed: Instant,
}

impl App {
    pub(super) fn changes_open(&self) -> bool {
        self.changes_ui.is_some()
    }

    /// Opens the view for the agent session in the active tab.
    pub(super) fn open_changes(&mut self) {
        let session = self
            .active()
            .and_then(|t| self.flight_session(t.id))
            .and_then(|id| crate::agent::load(&id).ok());
        let Some(session) = session else {
            self.lua_set_status("No agent session in this tab (start one: New agent)", true);
            return;
        };
        let Some(w) = session.worktree.clone() else {
            self.lua_set_status(
                &format!(
                    "{} runs without a worktree, so there's nothing to compare",
                    session.id
                ),
                true,
            );
            return;
        };
        match crate::changes::collect(&w.path, &w.base) {
            Ok(data) => {
                self.changes_ui = Some(ChangesUi {
                    session,
                    data,
                    file: 0,
                    scroll: 0,
                    confirm: None,
                    refreshed: Instant::now(),
                });
                self.flight.focused = false;
            }
            Err(e) => self.lua_set_status(&format!("Changes: {e}"), true),
        }
        self.request_redraw();
    }

    /// Re-reads the worktree (keeping the selected file) when it's time,
    /// or now with `force`.
    pub(super) fn refresh_changes(&mut self, force: bool) {
        let Some(ui) = &mut self.changes_ui else {
            return;
        };
        if !force && ui.refreshed.elapsed() < REFRESH {
            return;
        }
        ui.refreshed = Instant::now();
        let Some(w) = &ui.session.worktree else {
            return;
        };
        let Ok(data) = crate::changes::collect(&w.path, &w.base) else {
            return;
        };
        if data == ui.data {
            return;
        }
        let path = ui.data.files.get(ui.file).map(|f| f.path.clone());
        ui.file = path
            .and_then(|p| data.files.iter().position(|f| f.path == p))
            .unwrap_or(ui.file.min(data.files.len().saturating_sub(1)));
        if ui.data.files.get(ui.file).map(|f| &f.lines) != data.files.get(ui.file).map(|f| &f.lines)
        {
            ui.scroll = 0;
        }
        ui.data = data;
        self.request_redraw();
    }

    /// Rows of diff the view shows (for paging).
    fn changes_page(&self) -> usize {
        self.gpu
            .as_ref()
            .map(|gpu| {
                let (_, area) = self.areas(gpu);
                let rows = (area.h / gpu.renderer.cell_size().1).floor() as usize;
                ui::changes_view::diff_rows(rows).max(1)
            })
            .unwrap_or(20)
    }

    /// Keys while the view is open. Returns false for Ctrl/Alt
    /// combinations, which go to the bindings.
    pub(super) fn changes_key(&mut self, event: &KeyEvent) -> bool {
        use winit::keyboard::{Key, NamedKey};
        if self.mods.control_key() || self.mods.alt_key() {
            return false;
        }
        if event.state != winit::event::ElementState::Pressed {
            return true;
        }
        let page = self.changes_page();
        let Some(ui) = &mut self.changes_ui else {
            return false;
        };
        let key = &event.logical_key;
        let ch = match key {
            Key::Character(c) => c.as_str(),
            _ => "",
        };

        // Answering "revert this file?".
        if let Some(path) = ui.confirm.take() {
            if ch == "y" {
                if let Some(w) = &ui.session.worktree {
                    let result = crate::changes::revert(&w.path, &w.base, &path);
                    match result {
                        Ok(()) => self.lua_set_status(&format!("Reverted {path}"), false),
                        Err(e) => self.lua_set_status(&format!("Revert failed: {e}"), true),
                    }
                    self.refresh_changes(true);
                }
            }
            self.request_redraw();
            return true;
        }

        let files = ui.data.files.len();
        let lines = ui
            .data
            .files
            .get(ui.file)
            .map(|f| f.lines.len())
            .unwrap_or(0);
        let max_scroll = lines.saturating_sub(page);
        let mut open = None;
        let mut copy = None;
        match (key, ch) {
            (Key::Named(NamedKey::Escape), _) | (_, "q") => {
                self.changes_ui = None;
                self.request_redraw();
                return true;
            }
            (Key::Named(NamedKey::ArrowUp), _) | (_, "k") => {
                ui.file = ui.file.saturating_sub(1);
                ui.scroll = 0;
            }
            (Key::Named(NamedKey::ArrowDown), _) | (_, "j") => {
                ui.file = (ui.file + 1).min(files.saturating_sub(1));
                ui.scroll = 0;
            }
            (Key::Named(NamedKey::PageDown), _) | (Key::Named(NamedKey::Space), _) => {
                ui.scroll = (ui.scroll + page.saturating_sub(2).max(1)).min(max_scroll);
            }
            (Key::Named(NamedKey::PageUp), _) => {
                ui.scroll = ui.scroll.saturating_sub(page.saturating_sub(2).max(1));
            }
            (Key::Named(NamedKey::Home), _) => ui.scroll = 0,
            (Key::Named(NamedKey::End), _) => ui.scroll = max_scroll,
            (_, "e") => {
                if let (Some(f), Some(w)) = (ui.data.files.get(ui.file), &ui.session.worktree) {
                    open = Some(FileTarget {
                        path: w.path.join(&f.path),
                        line: f.first_line(),
                        col: None,
                    });
                }
            }
            (_, "r") => ui.confirm = ui.data.files.get(ui.file).map(|f| f.path.clone()),
            (_, "y") => copy = ui.data.files.get(ui.file).map(|f| f.path.clone()),
            _ => {}
        }
        if let Some(target) = open {
            // The editor may open in a pane: get out of its way.
            self.changes_ui = None;
            self.open_file(&target);
        }
        if let Some(path) = copy {
            self.copy_text(&path);
            self.lua_set_status(&format!("Copied {path}"), false);
        }
        self.request_redraw();
        true
    }

    /// The view over the tab's area.
    pub(super) fn draw_changes(&self, gpu: &Gpu, list: &mut DrawList) {
        let Some(ui) = &self.changes_ui else {
            return;
        };
        let (_, area) = self.areas(gpu);
        let (cw, ch) = gpu.renderer.cell_size();
        let cols = (area.w / cw).floor() as usize;
        let rows = (area.h / ch).floor() as usize;
        let fg = frame::hex_to_rgb(self.palette.fg);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let mix = |a: [u8; 3], b: [u8; 3], t: f32| {
            let m = |x: u8, y: u8| (x as f32 * t + y as f32 * (1.0 - t)) as u8;
            [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
        };
        let accent = frame::hex_to_rgb(self.palette.ansi[5]);
        let colors = ui::changes_view::Colors {
            fg,
            bg,
            dim: mix(fg, bg, 0.55),
            accent,
            added: frame::hex_to_rgb(self.palette.ansi[2]),
            removed: frame::hex_to_rgb(self.palette.ansi[1]),
            warn: frame::hex_to_rgb(self.palette.ansi[3]),
            selected: mix(accent, bg, 0.25),
        };
        let title = ui.session.title();
        let question = ui
            .confirm
            .as_ref()
            .map(|p| format!("Put {p} back the way it was when the agent started?"));
        let frame = ui::changes_view::build(
            &ui::changes_view::View {
                title: &title,
                branch: ui.session.worktree.as_ref().map(|w| w.branch.as_str()),
                changes: &ui.data,
                file: ui.file,
                scroll: ui.scroll,
                confirm: question.as_deref(),
            },
            cols,
            rows,
            &colors,
        );
        list.panes.push((area, frame, 0.0, 0.0));
    }
}
