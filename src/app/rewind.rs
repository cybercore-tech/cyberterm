// src/app/rewind.rs
//
// Rewind (Ctrl+Shift+U): step back through what a pane's screen looked
// like. The pane shows the replayed screen (src/rewind.rs) with a bar
// along the bottom; arrows move between moments (points where output
// paused), y copies the screen as it was, Esc returns to the live pane.
// The shell keeps running underneath -- rewind only changes what's drawn.

use super::*;
use crate::rewind::{ReplayTerm, Timeline};

pub(super) struct RewindUi {
    pane: PaneId,
    timeline: Timeline,
    moment: usize,
    screen: ReplayTerm,
}

fn ago(ms: u64) -> String {
    let s = crate::shell::tap::now_ms().saturating_sub(ms) / 1000;
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m {}s ago", s / 60, s % 60),
        _ => format!("{}h {}m ago", s / 3600, (s % 3600) / 60),
    }
}

/// Applies [rewind] to new recorders.
pub(super) fn apply_rewind_config(cfg: &crate::config::RewindConfig) {
    let cap = if cfg.enabled {
        cfg.buffer_kb.clamp(64, 64 * 1024) * 1024
    } else {
        0
    };
    crate::rewind::CAP_BYTES.store(cap, std::sync::atomic::Ordering::Relaxed);
}

impl App {
    pub(super) fn rewind_open(&self) -> bool {
        self.rewind.is_some()
    }

    pub(super) fn toggle_rewind(&mut self) {
        if self.rewind.take().is_some() {
            self.request_redraw();
            return;
        }
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let timeline = pane.session.rewind.lock().timeline();
        let moment = timeline.len() - 1;
        let screen = timeline.render(moment);
        self.rewind = Some(RewindUi {
            pane: pane.id,
            timeline,
            moment,
            screen,
        });
        self.request_redraw();
    }

    pub(super) fn rewind_key(&mut self, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let shift = self.mods.shift_key();
        let Some(ui) = self.rewind.as_mut() else {
            return;
        };
        let last = ui.timeline.len() - 1;
        let step = if shift { 10 } else { 1 };
        let target = match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.rewind = None;
                self.request_redraw();
                return;
            }
            Key::Named(NamedKey::ArrowLeft) => ui.moment.saturating_sub(step),
            Key::Named(NamedKey::ArrowRight) => (ui.moment + step).min(last),
            Key::Named(NamedKey::PageUp) => ui.moment.saturating_sub(10),
            Key::Named(NamedKey::PageDown) => (ui.moment + 10).min(last),
            Key::Named(NamedKey::Home) => 0,
            Key::Named(NamedKey::End) => last,
            Key::Character(c) => match c.to_lowercase().as_str() {
                "h" => ui.moment.saturating_sub(step),
                "l" => (ui.moment + step).min(last),
                "q" => {
                    self.rewind = None;
                    self.request_redraw();
                    return;
                }
                "y" => {
                    let rows = ui.screen.screen_lines() as i32;
                    let text = frame::lines_text(&ui.screen, 0, rows - 1);
                    self.copy_text(&text);
                    return;
                }
                _ => return,
            },
            _ => return,
        };
        if target != ui.moment {
            ui.moment = target;
            ui.screen = ui.timeline.render(target);
        }
        self.request_redraw();
    }

    /// The rewound screen for `pane` (only the pane being rewound), fitted
    /// to its current size, with the rewind bar on the bottom row.
    pub(super) fn draw_rewind(&self, pane: PaneId, cols: usize, rows: usize) -> Option<Frame> {
        let ui = self.rewind.as_ref().filter(|ui| ui.pane == pane)?;
        let fg = frame::hex_to_rgb(self.palette.fg);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let accent = frame::hex_to_rgb(self.palette.ansi[3]);
        let replay = frame::build(
            &ui.screen,
            &FrameOptions {
                palette: &self.palette,
                bold_is_bright: self.config.font.bold_is_bright,
                // "Focused but blinked off": no cursor in a recording.
                cursor: CursorOptions {
                    visible: false,
                    focused: true,
                    unfocused_hollow: false,
                },
                link_hover: &[],
            },
        );
        // The recording may be from another size: copy what fits.
        let mut f = Frame::blank(cols, rows, fg, bg);
        for r in 0..rows.min(replay.rows) {
            for c in 0..cols.min(replay.cols) {
                f.cells[r * cols + c] = replay.cells[r * replay.cols + c].clone();
            }
        }
        if rows == 0 {
            return Some(f);
        }
        let total = ui.timeline.len();
        let at = ui.timeline.time_ms(ui.moment);
        let live = ui.moment + 1 == total;
        let when = if live {
            "now".to_string()
        } else {
            format!("{} · {}", crate::history::format_time(at), ago(at))
        };
        let bar = format!(
            " ⏪ REWIND  {when}  ({}/{total})   ←/→ step · Shift ×10 · Home/End · y copy · Esc live ",
            ui.moment + 1
        );
        let row = rows - 1;
        f.fill(row, 0, bg, accent);
        f.put(row, 0, &bar, bg, accent);
        // A position marker along the bar's last columns.
        let track = 20.min(cols / 4);
        if track > 4 && cols > bar.chars().count() + track + 2 {
            let start = cols - track - 1;
            let pos = if total > 1 {
                ui.moment * (track - 1) / (total - 1)
            } else {
                track - 1
            };
            let line: String = (0..track)
                .map(|i| if i == pos { '●' } else { '─' })
                .collect();
            f.put(row, start, &line, bg, accent);
        }
        f.cursor = None;
        Some(f)
    }
}
