// src/app/agent_tabs.rs
//
// Agent sessions in the window (src/agent.rs has the sessions
// themselves). A pane is an agent pane while its foreground process
// carries CYBERTERM_AGENT_ID, checked once a second from /proc along with
// danger mode -- so it doesn't matter whether the agent was started from
// the palette, `cyberterm +agent`, a layout, or survived in the daemon.
// "New agent: ..." in the command palette starts one for the focused
// pane's directory.
//
// The Flight log panel (Ctrl+Shift+L) sits beside a tab's panes -- the
// panes really shrink, so the agent sees its true width -- and shows that
// tab's session (src/ui/flight_panel.rs draws it). Each tab keeps its
// session after the agent quits, so the log stays readable. Tabs are
// marked with the agent's state (◆ working, ◆! needs you, ✓ done), and a
// desktop notification says when an agent you're not looking at needs you
// or has finished. "Needs you" comes from the agent's hooks or from a
// desktop notification it sent through the terminal (OSC 9 / 777).

use super::*;
use crate::flight_log::{Entry, State, Summary};
use crate::layout::TabId;
use crate::procs;
use std::collections::{HashMap, HashSet};

/// One tab's panel.
#[derive(Default)]
pub(super) struct Panel {
    /// The selected entry; `None` follows the newest.
    selected: Option<usize>,
    /// Commands whose output is open.
    expanded: HashSet<usize>,
}

/// A session's log, re-read when its file changes.
struct Log {
    stamp: Option<(u64, std::time::SystemTime)>,
    entries: Vec<Entry>,
    summary: Summary,
    record: Option<crate::agent::Session>,
}

#[derive(Default)]
pub(super) struct FlightState {
    /// The session each tab shows; kept after the agent quits.
    tab_session: HashMap<TabId, String>,
    /// Tabs with the panel open.
    open: HashMap<TabId, Panel>,
    /// The panel has the keyboard.
    pub(super) focused: bool,
    logs: HashMap<String, Log>,
    /// Sessions whose panel was opened automatically already.
    auto_opened: HashSet<String>,
    /// The state last seen per session (to notify on changes).
    seen_state: HashMap<String, State>,
}

impl App {
    /// Re-checks which panes run an agent session (once a second).
    pub(super) fn refresh_agent_panes(&mut self) {
        let mut changed = false;
        for i in 0..self.panes.len() {
            let shell = self.panes[i].session.pid();
            let fg = procs::foreground(shell).unwrap_or(shell);
            let id = procs::env_var(fg, "CYBERTERM_AGENT_ID");
            if self.panes[i].agent.as_ref().map(|a| &a.0) == id.as_ref() {
                continue;
            }
            self.panes[i].agent = id.map(|id| {
                let label = crate::agent::load(&id)
                    .map(|s| s.label)
                    .unwrap_or_else(|_| "agent".into());
                (id, label)
            });
            changed = true;
        }
        if self.refresh_flight() {
            changed = true;
        }
        if changed {
            self.request_redraw();
        }
    }

    /// Follows each tab's session: its log, the tab's state, the panel
    /// opening by itself for a new agent, notifications. Returns whether
    /// anything shown changed.
    fn refresh_flight(&mut self) -> bool {
        let mut changed = false;
        let mut relayout = false;
        for t in 0..self.tabs.len() {
            let tab = &self.tabs[t];
            let mut ids = tab.root.panes();
            ids.sort_by_key(|id| *id != tab.focused);
            let session = ids.iter().find_map(|id| {
                self.pane(*id)
                    .and_then(|p| p.agent.as_ref().map(|a| a.0.clone()))
            });
            let tab_id = tab.id;
            if let Some(session) = session {
                if self.flight.tab_session.get(&tab_id) != Some(&session) {
                    self.flight.tab_session.insert(tab_id, session.clone());
                    changed = true;
                }
                if self.config.agents.panel && self.flight.auto_opened.insert(session) {
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        self.flight.open.entry(tab_id)
                    {
                        e.insert(Panel::default());
                        relayout = true;
                    }
                }
            }
        }
        // Forget tabs that closed.
        let live: HashSet<TabId> = self.tabs.iter().map(|t| t.id).collect();
        self.flight.tab_session.retain(|t, _| live.contains(t));
        self.flight.open.retain(|t, _| live.contains(t));

        let sessions: HashSet<String> = self.flight.tab_session.values().cloned().collect();
        for id in &sessions {
            let path = crate::flight_log::log_path(id);
            let stamp = std::fs::metadata(&path)
                .ok()
                .and_then(|m| Some((m.len(), m.modified().ok()?)));
            let log = self.flight.logs.entry(id.clone()).or_insert_with(|| Log {
                stamp: None,
                entries: Vec::new(),
                summary: crate::flight_log::summary(&[]),
                record: crate::agent::load(id).ok(),
            });
            if log.stamp != stamp
                || (stamp.is_none() && log.entries.is_empty() && log.stamp.is_some())
            {
                log.stamp = stamp;
                log.entries = crate::flight_log::timeline(&crate::flight_log::read(&path));
                log.summary = crate::flight_log::summary(&log.entries);
                changed = true;
            }
        }
        self.flight.logs.retain(|id, _| sessions.contains(id));

        // Notify about agents that need you or finished, unless you're
        // looking at them.
        for t in 0..self.tabs.len() {
            let tab_id = self.tabs[t].id;
            let Some(id) = self.flight.tab_session.get(&tab_id).cloned() else {
                continue;
            };
            let Some((state, running, text)) = self.agent_state(tab_id) else {
                continue;
            };
            let before = self.flight.seen_state.insert(id.clone(), state);
            if before == Some(state) {
                continue;
            }
            changed = true;
            let watching = self.window_focused && t == self.active_tab;
            if !should_notify(before, state, watching, running) {
                continue;
            }
            let label = self
                .flight
                .logs
                .get(&id)
                .and_then(|l| l.record.as_ref())
                .map(|r| r.label.clone())
                .unwrap_or_else(|| "Agent".into());
            let (title, body) = if state == State::Waiting {
                (format!("◆ {label} needs you"), text.unwrap_or_default())
            } else {
                (format!("■ {label} finished"), id.clone())
            };
            spawn_detached(
                Command::new("notify-send")
                    .arg("--app-name=Cyberterm")
                    .arg(title)
                    .arg(body),
            );
        }
        if relayout {
            self.relayout();
        }
        changed
    }

    /// A tab's agent: its state (with "needs you" from a desktop
    /// notification the agent sent, if that's newer than its log), whether
    /// it's still running, and what it's waiting for.
    fn agent_state(&self, tab: TabId) -> Option<(State, bool, Option<String>)> {
        let id = self.flight.tab_session.get(&tab)?;
        let log = self.flight.logs.get(id)?;
        let t = self.tabs.iter().find(|t| t.id == tab)?;
        let panes: Vec<&Pane> = t
            .root
            .panes()
            .iter()
            .filter_map(|p| self.pane(*p))
            .collect();
        let running = panes
            .iter()
            .any(|p| p.agent.as_ref().is_some_and(|a| &a.0 == id));
        let notice = panes
            .iter()
            .filter(|p| p.agent.as_ref().is_some_and(|a| &a.0 == id))
            .filter_map(|p| p.session.shell.lock().notice.clone())
            .max_by_key(|n| n.0);
        let mut state = log.summary.state;
        let mut text = match log.entries.last() {
            Some(Entry::Waiting { text, .. }) => text.clone(),
            _ => None,
        };
        if let Some((at, msg)) = notice {
            if at > log.summary.last_t && running {
                state = State::Waiting;
                text = Some(msg);
            }
        }
        if !running && state == State::Working {
            state = State::Idle;
        }
        Some((state, running, text))
    }

    /// The mark on an agent tab's label: (symbol, needs attention).
    pub(super) fn agent_tab_mark(&self, tab: TabId) -> Option<(&'static str, bool)> {
        let (state, running, _) = self.agent_state(tab)?;
        match (state, running) {
            (State::Waiting, true) => Some(("◆!", true)),
            (State::Done, _) => Some(("✓", false)),
            (_, true) => Some(("◆", false)),
            _ => None,
        }
    }

    // ------------------------------------------------------------------
    // The panel
    // ------------------------------------------------------------------

    /// Columns the panel takes in a window this many columns wide; 0 when
    /// there isn't room for it.
    fn panel_cols(&self, total: usize) -> usize {
        let want = match self.config.agents.panel_width {
            0 => (total * 36 / 100).clamp(34, 64),
            w => w,
        };
        if total < want + 40 {
            0
        } else {
            want
        }
    }

    /// The area a tab's panes get: the content area, less the panel when
    /// it's open there.
    pub(super) fn tab_area(&self, gpu: &Gpu, tab: TabId) -> Rect {
        let (_, mut area) = self.areas(gpu);
        if self.flight.open.contains_key(&tab) {
            let cw = gpu.renderer.cell_size().0;
            let total = (area.w / cw).floor() as usize;
            let cols = self.panel_cols(total);
            if cols > 0 {
                area.w -= cols as f32 * cw + self.gap(gpu);
            }
        }
        area
    }

    /// Where the active tab's panel goes, if it's open.
    pub(super) fn flight_panel_rect(&self, gpu: &Gpu) -> Option<Rect> {
        let tab = self.active()?.id;
        if !self.flight.open.contains_key(&tab) {
            return None;
        }
        let (_, area) = self.areas(gpu);
        let cw = gpu.renderer.cell_size().0;
        let cols = self.panel_cols((area.w / cw).floor() as usize);
        (cols > 0).then_some(Rect {
            x: area.x + area.w - cols as f32 * cw,
            w: cols as f32 * cw,
            ..area
        })
    }

    /// Opens the active tab's panel without taking the keyboard (for an
    /// agent being started there, so it starts at its final width).
    pub(super) fn open_flight_panel(&mut self) {
        if !self.config.agents.panel {
            return;
        }
        if let Some(tab) = self.active().map(|t| t.id) {
            if let std::collections::hash_map::Entry::Vacant(e) = self.flight.open.entry(tab) {
                e.insert(Panel::default());
                self.relayout();
            }
        }
    }

    /// Ctrl+Shift+L: opens the panel (with the keyboard), focuses it when
    /// open, closes it when it has the keyboard.
    pub(super) fn toggle_flight_log(&mut self) {
        let Some(tab) = self.active().map(|t| t.id) else {
            return;
        };
        match self.flight.open.entry(tab) {
            std::collections::hash_map::Entry::Occupied(e) => {
                if self.flight.focused {
                    e.remove();
                    self.flight.focused = false;
                    self.relayout();
                } else {
                    self.flight.focused = true;
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(Panel::default());
                self.flight.focused = true;
                self.relayout();
            }
        }
        self.request_redraw();
    }

    fn panel_entries(&self, tab: TabId) -> &[Entry] {
        self.flight
            .tab_session
            .get(&tab)
            .and_then(|id| self.flight.logs.get(id))
            .map(|l| l.entries.as_slice())
            .unwrap_or_default()
    }

    /// Keys while the panel has the keyboard. Returns false for keys it
    /// leaves to the bindings (anything with Ctrl or Alt).
    pub(super) fn flight_key(&mut self, event: &KeyEvent) -> bool {
        use winit::keyboard::{Key, NamedKey};
        let Some(tab) = self.active().map(|t| t.id) else {
            return false;
        };
        if !self.flight.focused || !self.flight.open.contains_key(&tab) {
            self.flight.focused = false;
            return false;
        }
        if self.mods.control_key() || self.mods.alt_key() {
            return false;
        }
        if event.state != winit::event::ElementState::Pressed {
            return true;
        }
        let len = self.panel_entries(tab).len();
        let entry = self
            .flight
            .open
            .get(&tab)
            .and_then(|p| p.selected)
            .unwrap_or(len.saturating_sub(1));
        let mut copy = None;
        if let Some(panel) = self.flight.open.get_mut(&tab) {
            let last = len.saturating_sub(1);
            let to = |i: usize| if i >= last { None } else { Some(i) };
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => self.flight.focused = false,
                Key::Named(NamedKey::ArrowUp) => panel.selected = to(entry.saturating_sub(1)),
                Key::Named(NamedKey::ArrowDown) => panel.selected = to(entry + 1),
                Key::Named(NamedKey::PageUp) => panel.selected = to(entry.saturating_sub(10)),
                Key::Named(NamedKey::PageDown) => panel.selected = to(entry + 10),
                Key::Named(NamedKey::Home) => panel.selected = to(0),
                Key::Named(NamedKey::End) => panel.selected = None,
                Key::Named(NamedKey::Enter) => {
                    if !panel.expanded.remove(&entry) {
                        panel.expanded.insert(entry);
                    }
                }
                Key::Character(c) if c.as_str() == "y" => {
                    copy = self.panel_entries(tab).get(entry).map(|e| match e {
                        Entry::Prompt { text, .. } => text.clone(),
                        Entry::Command { command, .. } => command.clone(),
                        Entry::Edit { path, .. } => path.clone(),
                        Entry::Tool { tool, text, .. } => {
                            text.clone().unwrap_or_else(|| tool.clone())
                        }
                        Entry::Waiting { text, .. } => text.clone().unwrap_or_default(),
                        Entry::Done { .. } => String::new(),
                    });
                }
                _ => {}
            }
        }
        if let Some(text) = copy.filter(|t| !t.is_empty()) {
            self.copy_text(&text);
            self.lua_set_status("Copied", false);
        }
        self.request_redraw();
        true
    }

    /// The panel for the active tab, if it's open.
    pub(super) fn draw_flight_panel(&self, gpu: &Gpu, list: &mut DrawList) {
        let Some(rect) = self.flight_panel_rect(gpu) else {
            return;
        };
        let Some(tab) = self.active().map(|t| t.id) else {
            return;
        };
        let Some(panel) = self.flight.open.get(&tab) else {
            return;
        };
        let (cw, ch) = gpu.renderer.cell_size();
        let cols = (rect.w / cw).floor() as usize;
        let rows = (rect.h / ch).floor() as usize;
        let fg = frame::hex_to_rgb(self.palette.fg);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let mix = |a: [u8; 3], b: [u8; 3], t: f32| {
            let m = |x: u8, y: u8| (x as f32 * t + y as f32 * (1.0 - t)) as u8;
            [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
        };
        let accent = frame::hex_to_rgb(self.palette.ansi[5]);
        let colors = ui::flight_panel::Colors {
            fg,
            bg: mix(fg, bg, 0.05),
            dim: mix(fg, bg, 0.55),
            accent,
            ok: frame::hex_to_rgb(self.palette.ansi[2]),
            bad: frame::hex_to_rgb(self.palette.ansi[1]),
            warn: frame::hex_to_rgb(self.palette.ansi[3]),
            selected: mix(accent, bg, 0.25),
        };
        let session = self.flight.tab_session.get(&tab);
        let log = session.and_then(|id| self.flight.logs.get(id));
        let record = log.and_then(|l| l.record.as_ref());
        let title = match (record, session) {
            (Some(r), _) => r.title(),
            (None, Some(id)) => id.clone(),
            (None, None) => "No agent in this tab".into(),
        };
        let root = record
            .and_then(|r| r.worktree.as_ref())
            .map(|w| w.path.to_string_lossy().into_owned());
        let (state, running, _) = self.agent_state(tab).unwrap_or((State::Idle, false, None));
        let mut summary = log
            .map(|l| l.summary)
            .unwrap_or_else(|| crate::flight_log::summary(&[]));
        summary.state = state;
        let key = self.bindings.hint(Action::FlightLog);
        let entries = log.map(|l| l.entries.as_slice()).unwrap_or_default();
        let frame = ui::flight_panel::build(
            &ui::flight_panel::View {
                title: &title,
                branch: record
                    .and_then(|r| r.worktree.as_ref())
                    .map(|w| w.branch.as_str()),
                running,
                summary,
                entries,
                selected: panel.selected,
                expanded: &panel.expanded,
                focused: self.flight.focused,
                root: root.as_deref(),
                key: if key.is_empty() { "Ctrl+Shift+L" } else { &key },
            },
            cols,
            rows,
            &colors,
        );
        // A line between the panes and the panel.
        let line = (gpu.window.scale_factor() as f32).round().max(1.0);
        let gap = self.gap(gpu);
        list.overlays.push(Overlay {
            rect: Rect {
                x: (rect.x - (gap + line) / 2.0).round(),
                w: line,
                ..rect
            },
            color: if self.flight.focused {
                accent
            } else {
                mix(fg, bg, 0.25)
            },
            alpha: 1.0,
        });
        list.panes.push((rect, frame, 0.0, 0.0));
    }

    /// Starts an agent session in a new tab, in a worktree of the focused
    /// pane's repository (when [agents] worktrees is on and it is one).
    pub(super) fn start_agent(&mut self, name: &str) {
        let launchers = crate::agent::launchers(&self.config.agents);
        let Some(launcher) = crate::agent::find(&launchers, name).cloned() else {
            self.lua_set_status(&format!("No agent called {name}"), true);
            return;
        };
        let cwd = self
            .inherited_cwd()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let session = match crate::agent::create(
            &self.config.agents,
            crate::agent::Request {
                launcher: &launcher,
                task: None,
                cwd,
                worktree: self.config.agents.worktrees,
            },
        ) {
            Ok(s) => s,
            Err(e) => {
                self.lua_set_status(&format!("{}: {e}", launcher.label), true);
                return;
            }
        };
        let pane = match self.open_tab(Some(session.dir.clone())) {
            Ok(id) => id,
            Err(e) => {
                self.lua_set_status(&format!("couldn't open a tab: {e}"), true);
                return;
            }
        };
        if let Some(tab) = self.active_mut() {
            tab.title = Some(session.title());
        }
        self.open_flight_panel();
        self.run_in(pane, Some(&crate::agent::run_command(&session.id)));
        let note = match &session.worktree {
            Some(w) => format!("{} · worktree on {}", session.label, w.branch),
            None => format!(
                "{} · in {}",
                session.label,
                crate::agent::short(&session.dir)
            ),
        };
        self.lua_set_status(&note, false);
    }
}

/// Whether a session's new state is worth a desktop notification: it
/// needs you or finished, it's still running, you're not looking at it,
/// and it isn't just the state found when the window first saw it.
fn should_notify(before: Option<State>, now: State, watching: bool, running: bool) -> bool {
    matches!(now, State::Waiting | State::Done)
        && before.is_some()
        && before != Some(now)
        && !watching
        && running
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_only_for_news_you_are_not_watching() {
        use State::*;
        assert!(should_notify(Some(Working), Waiting, false, true));
        assert!(should_notify(Some(Working), Done, false, true));
        // Looking at it, already known, just found, or not running: quiet.
        assert!(!should_notify(Some(Working), Waiting, true, true));
        assert!(!should_notify(Some(Waiting), Waiting, false, true));
        assert!(!should_notify(None, Waiting, false, true));
        assert!(!should_notify(Some(Working), Waiting, false, false));
        assert!(!should_notify(Some(Waiting), Working, false, true));
    }
}
