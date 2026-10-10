// src/app/danger.rs
//
// Danger mode. A pane is dangerous while it's connected over SSH to a host
// matching [danger] hosts, running a root shell, sitting in a matching
// directory, or marked by hand (Ctrl+Shift+D). Dangerous panes get a red
// border, a faint red tint, a badge naming the reason and a marked tab,
// and Enter on a risky command line (src/ai.rs's check) waits for a second
// Enter.
//
// Detection reads /proc once a second (`refresh_danger`), so it follows
// `ssh prod` / `exit` without any shell integration.

use super::*;
use crate::ai::{assess, Risk};
use crate::procs;

pub(super) struct DangerConfirm {
    /// The panes the held-back keystroke was going to.
    targets: Vec<PaneId>,
    bytes: Vec<u8>,
    line: String,
    reasons: Vec<String>,
}

impl App {
    /// Why a pane is dangerous right now, if it is.
    fn danger_reason(&self, pane: &Pane) -> Option<String> {
        match pane.danger_manual {
            Some(true) => return Some("marked".into()),
            Some(false) => return None,
            None => {}
        }
        let cfg = &self.config.danger;
        if !cfg.enabled {
            return None;
        }
        let shell = pane.session.pid();
        let fg = procs::foreground(shell).unwrap_or(shell);
        let args = procs::cmdline(fg);
        if let Some(target) = procs::ssh_target(&args) {
            if cfg
                .hosts
                .iter()
                .any(|p| procs::glob(p, &target.host) || procs::glob(p, &target.destination))
            {
                return Some(format!("ssh {}", target.host));
            }
        }
        if cfg.root && (procs::euid(fg) == Some(0) || procs::euid(shell) == Some(0)) {
            return Some("root".into());
        }
        if !cfg.paths.is_empty() {
            if let Some(cwd) = pane.session.cwd() {
                if cfg.paths.iter().any(|p| procs::path_glob(p, &cwd)) {
                    let home = std::env::var("HOME").unwrap_or_default();
                    let shown = cwd.to_string_lossy().into_owned();
                    let shown = match shown.strip_prefix(&home) {
                        Some(rest) if !home.is_empty() => format!("~{rest}"),
                        _ => shown,
                    };
                    return Some(shown);
                }
            }
        }
        None
    }

    /// Re-checks every pane (once a second, from `about_to_wait`).
    pub(super) fn refresh_danger(&mut self) {
        let mut changed = false;
        for i in 0..self.panes.len() {
            let reason = self.danger_reason(&self.panes[i]);
            if self.panes[i].danger != reason {
                self.panes[i].danger = reason;
                changed = true;
            }
        }
        if changed {
            self.request_redraw();
        }
    }

    /// Ctrl+Shift+D: mark the focused pane dangerous, or unmark it
    /// (which also silences automatic detection for it).
    pub(super) fn toggle_danger(&mut self) {
        let Some(pane) = self.pane_mut(self.focused) else {
            return;
        };
        let on = pane.danger.is_none();
        pane.danger_manual = Some(on);
        self.refresh_danger();
    }

    pub(super) fn tab_in_danger(&self, tab: &Tab) -> bool {
        tab.root
            .panes()
            .iter()
            .any(|id| self.pane(*id).is_some_and(|p| p.danger.is_some()))
    }

    /// Called with a keystroke on its way to the panes. Holds it back (and
    /// returns true) when it's Enter on a risky command in a dangerous pane.
    pub(super) fn danger_hold(&mut self, targets: &[PaneId], key: &Key, bytes: &[u8]) -> bool {
        let threshold = match self.config.danger.confirm.as_str() {
            "off" => return false,
            "caution" => Risk::Caution,
            _ => Risk::Dangerous,
        };
        if !matches!(key, Key::Named(NamedKey::Enter)) {
            return false;
        }
        let mods = self.mods;
        if mods.control_key() || mods.alt_key() || mods.super_key() {
            return false;
        }
        for &id in targets {
            let Some(pane) = self.pane(id).filter(|p| p.danger.is_some()) else {
                continue;
            };
            let line = {
                let term = pane.session.term.lock();
                // Full-screen programs (editors, pagers) own Enter.
                if term.mode().contains(TermMode::ALT_SCREEN) {
                    continue;
                }
                crate::danger::command_line(&*term)
            };
            let (risk, reasons) = assess(&line);
            if risk >= threshold {
                self.danger_confirm = Some(DangerConfirm {
                    targets: targets.to_vec(),
                    bytes: bytes.to_vec(),
                    line,
                    reasons,
                });
                self.request_redraw();
                return true;
            }
        }
        false
    }

    pub(super) fn danger_confirm_open(&self) -> bool {
        self.danger_confirm.is_some()
    }

    /// Keys while a held-back Enter waits: Enter sends it, anything else
    /// (Esc included) drops it and leaves the command line as it was.
    pub(super) fn danger_confirm_key(&mut self, event: &KeyEvent) {
        if event.state != ElementState::Pressed || event.repeat {
            return;
        }
        let Some(confirm) = self.danger_confirm.take() else {
            return;
        };
        if matches!(event.logical_key, Key::Named(NamedKey::Enter)) {
            for id in &confirm.targets {
                if let Some(pane) = self.pane(*id) {
                    pane.session.term.lock().scroll_display(Scroll::Bottom);
                    pane.session.write(confirm.bytes.clone());
                }
            }
        }
        self.request_redraw();
    }

    // ------------------------------------------------------------------
    // Drawing
    // ------------------------------------------------------------------

    fn danger_color(&self) -> [u8; 3] {
        frame::hex_to_rgb(self.palette.ansi[1])
    }

    /// Border and tint for a dangerous pane.
    pub(super) fn danger_overlays(&self, pane: &Pane, rect: Rect, overlays: &mut Vec<Overlay>) {
        if pane.danger.is_none() {
            return;
        }
        let color = self.danger_color();
        let tint = self.config.danger.tint.clamp(0.0, 0.3);
        if tint > 0.0 {
            overlays.push(Overlay {
                rect,
                color,
                alpha: tint,
            });
        }
        let scale = self
            .gpu
            .as_ref()
            .map_or(1.0, |g| g.window.scale_factor() as f32);
        let w = (2.0 * scale).round().max(1.0);
        for r in [
            Rect { h: w, ..rect },
            Rect {
                y: rect.y + rect.h - w,
                h: w,
                ..rect
            },
            Rect { w, ..rect },
            Rect {
                x: rect.x + rect.w - w,
                w,
                ..rect
            },
        ] {
            overlays.push(Overlay {
                rect: r,
                color,
                alpha: 1.0,
            });
        }
    }

    /// The `⚠ reason` badge in the bottom-right corner; returns its width
    /// so other badges can sit to its left.
    pub(super) fn draw_danger_badge(&self, pane: &Pane, frame: &mut Frame) -> usize {
        let Some(reason) = &pane.danger else { return 0 };
        let short: String = reason.chars().take(30).collect();
        let text = format!(" ⚠ {short} ");
        let width = text.chars().count();
        if frame.rows == 0 || frame.cols < width + 2 {
            return 0;
        }
        let fg = frame::hex_to_rgb(self.palette.bg);
        frame.put(
            frame.rows - 1,
            frame.cols - width,
            &text,
            fg,
            self.danger_color(),
        );
        width
    }

    /// The confirmation bar over the bottom of the focused pane.
    pub(super) fn draw_danger_confirm(&self, frame: &mut Frame) {
        let Some(c) = &self.danger_confirm else {
            return;
        };
        let fg = frame::hex_to_rgb(self.palette.fg);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let red = self.danger_color();
        let cols = frame.cols;
        let width = cols.saturating_sub(4).max(1);
        let mut line: String = c.line.chars().take(width * 2).collect();
        if c.line.chars().count() > width * 2 {
            line.pop();
            line.push('…');
        }
        let chars: Vec<char> = line.chars().collect();
        let rows: Vec<String> = chars.chunks(width).map(|w| w.iter().collect()).collect();
        let height = 2 + rows.len() + 1;
        if frame.rows < height {
            return;
        }
        let reason = self
            .pane(*c.targets.first().unwrap_or(&self.focused))
            .and_then(|p| p.danger.clone())
            .unwrap_or_default();
        let mut row = frame.rows - height;
        frame.fill(row, 0, bg, red);
        frame.put(
            row,
            1,
            &format!("⚠ Dangerous pane ({reason}). Run this?"),
            bg,
            red,
        );
        row += 1;
        for r in &rows {
            frame.fill(row, 0, fg, bg);
            frame.put(row, 2, r, fg, bg);
            row += 1;
        }
        frame.fill(row, 0, red, bg);
        frame.put(row, 2, &c.reasons.join(", "), red, bg);
        row += 1;
        frame.fill(row, 0, fg, bg);
        frame.put(row, 1, "Enter run it · any other key cancel", fg, bg);
        frame.cursor = None;
    }
}
