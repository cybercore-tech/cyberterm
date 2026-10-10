// src/app/ports.rs
//
// Live ports: each pane shows the TCP ports that programs started in it
// are listening on -- `npm run dev`, `cargo run`, `python -m http.server`
// -- as chips in its bottom-right corner. Clicking one (or the right-click
// menu) opens it in the browser. Read from /proc once a second with the
// danger check, so a server appears when it starts listening and goes
// away when it stops.

use std::cell::RefCell;

use super::*;
use crate::procs;

/// Where a chip was drawn: (pane, row, column range, port).
pub(super) type PortHit = (PaneId, usize, std::ops::Range<usize>, u16);

/// Most chips shown per pane; the rest are summarised as "+N".
const MAX_CHIPS: usize = 4;

#[derive(Default)]
pub(super) struct PortHits(RefCell<Vec<PortHit>>);

impl App {
    /// Re-reads every pane's listening ports (once a second).
    pub(super) fn refresh_ports(&mut self) {
        if !self.config.ports.enabled || self.panes.is_empty() {
            return;
        }
        let listening = procs::listening();
        let parents = if listening.is_empty() {
            Default::default()
        } else {
            procs::parents()
        };
        let mut changed = false;
        for pane in &mut self.panes {
            let ports = procs::ports_under(pane.session.pid(), &parents, &listening);
            if pane.ports != ports {
                pane.ports = ports;
                changed = true;
            }
        }
        if changed {
            self.request_redraw();
        }
    }

    pub(super) fn port_url(&self, port: u16) -> String {
        self.config.ports.url.replace("{port}", &port.to_string())
    }

    pub(super) fn open_port(&self, port: u16) {
        open_link(&self.port_url(port));
    }

    /// Draws a pane's port chips on its bottom row, `right` columns from
    /// the right edge (left of the other badges); returns their width.
    pub(super) fn draw_port_chips(&self, pane: &Pane, frame: &mut Frame, right: usize) -> usize {
        if pane.ports.is_empty() || frame.rows == 0 {
            return 0;
        }
        let shown = &pane.ports[..pane.ports.len().min(MAX_CHIPS)];
        let mut chips: Vec<(String, Option<u16>)> = shown
            .iter()
            .map(|p| (format!(" :{p} "), Some(*p)))
            .collect();
        if pane.ports.len() > MAX_CHIPS {
            chips.push((format!(" +{} ", pane.ports.len() - MAX_CHIPS), None));
        }
        let width: usize = chips.iter().map(|(t, _)| t.chars().count() + 1).sum();
        if frame.cols < width + right + 2 {
            return 0;
        }
        let row = frame.rows - 1;
        let fg = frame::hex_to_rgb(self.palette.bg);
        let bg = frame::hex_to_rgb(self.palette.ansi[2]);
        let mut col = frame.cols - right - width;
        let mut hits = self.port_hits.0.borrow_mut();
        hits.retain(|(id, ..)| *id != pane.id);
        for (text, port) in chips {
            let start = col;
            col = frame.put(row, col, &text, fg, bg) + 1;
            if let Some(port) = port {
                hits.push((pane.id, row, start..col - 1, port));
            }
        }
        width
    }

    /// The port chip under a cell of the focused pane, if any.
    pub(super) fn port_chip_at(&self, row: usize, col: usize) -> Option<u16> {
        let pane = self.focused_pane()?;
        if pane.ports.is_empty() {
            return None;
        }
        self.port_hits
            .0
            .borrow()
            .iter()
            .find(|(id, r, cols, _)| *id == pane.id && *r == row && cols.contains(&col))
            .map(|(.., port)| *port)
    }
}
