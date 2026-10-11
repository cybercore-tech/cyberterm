// src/app/agent_tower.rs
//
// The Tower (Ctrl+Shift+S): every agent session at once, over the tab.
// The sessions on the left (src/ui/tower_view.rs), the selected one's
// flight log on the right (src/ui/flight_panel.rs). From here: Enter
// jumps to a session's tab (or opens a shell in its worktree), c opens its
// Changes, m merges its work into the branch it started from and d
// discards it (both after a y/n question; src/agent_merge.rs), n starts a
// new agent.
//
// Reading sessions runs git for each, so it happens on a thread every
// couple of seconds while the Tower is open; merges and discards run on
// one too, so a slow commit hook never freezes the window.

use super::*;
use crate::agent_home::{Mark, SessionRow};
use crate::flight_log::{Entry, State, Summary};
use std::thread::JoinHandle;

/// How often an open Tower re-reads the sessions.
const REFRESH: Duration = Duration::from_secs(2);

enum Ask {
    /// The session, and whether its agent (done, but still open) is quit
    /// first.
    Merge(String, bool),
    Discard(String, bool),
    /// The other attempts at a task, after one was merged.
    DiscardAll(Vec<String>),
}

/// The selected session's flight log.
struct SelectedLog {
    id: String,
    title: String,
    branch: Option<String>,
    root: Option<String>,
    entries: Vec<Entry>,
    summary: Summary,
}

pub(super) struct TowerUi {
    rows: Vec<SessionRow>,
    selected: usize,
    log: Option<SelectedLog>,
    /// A question waiting for y / n.
    ask: Option<(Ask, String)>,
    /// A merge or discard running: what it is, and its result.
    job: Option<(String, JoinHandle<Result<String, String>>)>,
    /// The other attempts at the task being merged: offered for
    /// discarding once the merge is through.
    after_merge: Vec<String>,
    /// The sessions the running job removes, whose tabs then close.
    job_ids: Vec<String>,
    /// Sessions being read.
    loading: Option<JoinHandle<Vec<SessionRow>>>,
    refreshed: Option<Instant>,
}

fn read_rows() -> Vec<SessionRow> {
    let now = crate::shell::tap::now_ms();
    crate::agent::sessions()
        .iter()
        .map(|s| crate::agent_home::session_row(s, now))
        .collect()
}

fn read_log(id: &str) -> Option<SelectedLog> {
    let s = crate::agent::load(id).ok()?;
    let entries =
        crate::flight_log::timeline(&crate::flight_log::read(&crate::flight_log::log_path(id)));
    let summary = crate::flight_log::summary(&entries);
    Some(SelectedLog {
        id: id.to_string(),
        title: s.title(),
        branch: s.worktree.as_ref().map(|w| match &w.onto {
            Some(onto) => format!("{} → {onto}", w.branch),
            None => w.branch.clone(),
        }),
        root: s
            .worktree
            .as_ref()
            .map(|w| w.path.to_string_lossy().into_owned()),
        entries,
        summary,
    })
}

impl App {
    pub(super) fn tower_open(&self) -> bool {
        self.tower.is_some()
    }

    pub(super) fn toggle_tower(&mut self) {
        if self.tower.take().is_none() {
            self.tower = Some(TowerUi {
                rows: Vec::new(),
                selected: 0,
                log: None,
                ask: None,
                job: None,
                after_merge: Vec::new(),
                job_ids: Vec::new(),
                loading: None,
                refreshed: None,
            });
            self.flight.focused = false;
            self.refresh_tower(true);
        }
        self.request_redraw();
    }

    /// Picks up finished reads and jobs, and starts a new read when it's
    /// time (now, with `force`). Called from the poll loop.
    pub(super) fn refresh_tower(&mut self, force: bool) {
        let Some(ui) = &mut self.tower else {
            return;
        };
        let mut changed = false;
        let mut reload = force;

        if ui.job.as_ref().is_some_and(|(_, h)| h.is_finished()) {
            let (what, handle) = ui.job.take().expect("checked");
            let result = handle
                .join()
                .unwrap_or_else(|_| Err(format!("{what} stopped unexpectedly")));
            let (others, ids) = self
                .tower
                .as_mut()
                .map(|ui| {
                    (
                        std::mem::take(&mut ui.after_merge),
                        std::mem::take(&mut ui.job_ids),
                    )
                })
                .unwrap_or_default();
            if result.is_ok() {
                for id in &ids {
                    self.close_session_tab(id);
                }
            }
            // The first line: the rest is for the command line.
            let first = |t: &str| t.lines().next().unwrap_or_default().to_string();
            match result {
                Ok(note) if !others.is_empty() => {
                    let n = others.len();
                    let question = format!(
                        "{}. Discard the other attempt{} ({})?",
                        first(&note).trim_end_matches('.'),
                        if n == 1 { "" } else { "s" },
                        others.join(", ")
                    );
                    if let Some(ui) = &mut self.tower {
                        ui.ask = Some((Ask::DiscardAll(others), question));
                    }
                }
                Ok(note) => self.lua_set_status(&first(&note), false),
                Err(e) => self.lua_set_status(&first(&e), true),
            }
            reload = true;
            changed = true;
        }
        let Some(ui) = &mut self.tower else {
            return;
        };

        if ui.loading.as_ref().is_some_and(|h| h.is_finished()) {
            let rows = ui
                .loading
                .take()
                .expect("checked")
                .join()
                .unwrap_or_default();
            let keep = ui.rows.get(ui.selected).map(|r| r.id.clone());
            ui.rows = rows;
            ui.selected = keep
                .and_then(|id| ui.rows.iter().position(|r| r.id == id))
                .unwrap_or(ui.selected)
                .min(ui.rows.len().saturating_sub(1));
            changed = true;
            self.tower_sync_marks();
            self.tower_load_log();
        }
        let Some(ui) = &mut self.tower else {
            return;
        };
        let due = ui.refreshed.is_none_or(|t| t.elapsed() >= REFRESH);
        if ui.loading.is_none() && (reload || due) {
            ui.refreshed = Some(Instant::now());
            ui.loading = Some(std::thread::spawn(read_rows));
        }
        if changed {
            self.request_redraw();
        }
    }

    /// The window knows better than the logs whether an agent in one of
    /// its tabs needs you (it sees desktop notifications too).
    fn tower_sync_marks(&mut self) {
        let marks: Vec<Option<Mark>> = match &self.tower {
            Some(ui) => ui
                .rows
                .iter()
                .map(|r| {
                    let index = self.tab_for_session(&r.id)?;
                    let (state, running, _) = self.agent_state(self.tabs[index].id)?;
                    Some(crate::agent_home::mark(state, running))
                })
                .collect(),
            None => return,
        };
        if let Some(ui) = &mut self.tower {
            for (row, mark) in ui.rows.iter_mut().zip(marks) {
                if let Some(m) = mark {
                    row.mark = m;
                }
            }
        }
    }

    fn tower_load_log(&mut self) {
        let Some(ui) = &mut self.tower else {
            return;
        };
        ui.log = ui.rows.get(ui.selected).and_then(|r| read_log(&r.id));
    }

    fn tower_selected(&self) -> Option<&SessionRow> {
        let ui = self.tower.as_ref()?;
        ui.rows.get(ui.selected)
    }

    /// Enter: the session's tab, or a shell in its worktree.
    fn tower_jump(&mut self) {
        let Some(id) = self.tower_selected().map(|r| r.id.clone()) else {
            return;
        };
        if let Some(index) = self.tab_for_session(&id) {
            self.tower = None;
            self.activate_tab(index);
            return;
        }
        let Ok(session) = crate::agent::load(&id) else {
            return;
        };
        if !session.dir.is_dir() {
            self.lua_set_status(&format!("{id}'s worktree is gone"), true);
            return;
        }
        self.tower = None;
        match self.open_tab(Some(session.dir.clone())) {
            Ok(_) => {
                if let Some(tab) = self.active_mut() {
                    tab.title = Some(session.title());
                }
                self.lua_set_status(&format!("A shell in {}'s worktree", session.id), false);
            }
            Err(e) => self.lua_set_status(&format!("couldn't open a tab: {e}"), true),
        }
    }

    /// Closes the tab of a session that was merged or discarded, when
    /// all that's left in it are idle shells (in a worktree that's gone).
    /// Never the last tab.
    fn close_session_tab(&mut self, id: &str) {
        let Some(index) = self.tab_for_session(id) else {
            return;
        };
        if self.tabs.len() <= 1 {
            return;
        }
        let panes = self.tabs[index].root.panes();
        let idle = panes.iter().all(|p| {
            self.pane(*p).is_some_and(|pane| {
                let shell = pane.session.pid();
                crate::procs::foreground(shell).is_none_or(|fg| fg == shell)
            })
        });
        if idle {
            for p in panes {
                self.close_pane(p);
            }
        }
    }

    /// m: checks the merge (no changes yet) and asks.
    fn tower_merge(&mut self) {
        let Some(row) = self.tower_selected() else {
            return;
        };
        let (id, running) = (row.id.clone(), row.running);
        // Finished its turn but still open: offer to quit it first.
        let stop = running && row.mark == Mark::Done;
        let Ok(session) = crate::agent::load(&id) else {
            return;
        };
        let running = running && !stop;
        let strategy = crate::agent_merge::Strategy::parse(&self.config.agents.merge)
            .unwrap_or(crate::agent_merge::Strategy::Squash);
        let plan = match crate::agent_merge::plan(&session, running, strategy) {
            Ok(p) => p,
            Err(e) => {
                self.lua_set_status(&e, true);
                return;
            }
        };
        if let Some(why) = &plan.blocked {
            self.lua_set_status(why, true);
            return;
        }
        if !plan.conflicts.is_empty() {
            self.lua_set_status(
                &format!(
                    "{id} conflicts with {} in {}: ask the agent to merge {} and resolve them",
                    plan.onto,
                    plan.conflicts.join(", "),
                    plan.onto
                ),
                true,
            );
            return;
        }
        if plan.is_empty() {
            self.lua_set_status(&format!("{id} hasn't changed anything to merge"), false);
            return;
        }
        let how = match strategy {
            crate::agent_merge::Strategy::Squash => "as one commit",
            crate::agent_merge::Strategy::Merge => "with a merge commit",
            crate::agent_merge::Strategy::FastForward => "by fast-forward",
        };
        let n = plan.commits.len();
        let mut what = format!("{n} commit{}", if n == 1 { "" } else { "s" });
        if plan.uncommitted > 0 {
            what.push_str(&format!(
                ", {} uncommitted file{}",
                plan.uncommitted,
                if plan.uncommitted == 1 { "" } else { "s" }
            ));
        }
        let quit = if stop {
            format!("Quit {} and merge", session.label)
        } else {
            "Merge".to_string()
        };
        let question = format!(
            "{quit} {id} into {} {how} ({what}), then remove its worktree?",
            plan.onto
        );
        if let Some(ui) = &mut self.tower {
            let group = ui
                .rows
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| r.group.clone());
            ui.after_merge = ui
                .rows
                .iter()
                .filter(|r| r.id != id && group.is_some() && r.group == group)
                .map(|r| r.id.clone())
                .collect();
            ui.ask = Some((Ask::Merge(id, stop), question));
        }
        self.lua_clear_status();
    }

    /// d: asks before throwing the work away.
    fn tower_discard(&mut self) {
        let Some(row) = self.tower_selected() else {
            return;
        };
        let stop = row.running && row.mark == Mark::Done;
        if row.running && !stop {
            self.lua_set_status(&format!("{} is still working: quit it first", row.id), true);
            return;
        }
        let lost = match row.changes {
            Some((_, _, files)) if files > 0 || row.commits > 0 => format!(
                "{files} changed file{} and {} commit{} will be lost",
                if files == 1 { "" } else { "s" },
                row.commits,
                if row.commits == 1 { "" } else { "s" }
            ),
            _ => "nothing it changed is kept".into(),
        };
        let id = row.id.clone();
        let question = if stop {
            format!("Quit {id}'s agent and discard it? {lost}.")
        } else {
            format!("Discard {id}? {lost}.")
        };
        if let Some(ui) = &mut self.tower {
            ui.ask = Some((Ask::Discard(id, stop), question));
        }
        self.lua_clear_status();
    }

    fn tower_run(&mut self, ask: Ask) {
        let strategy = crate::agent_merge::Strategy::parse(&self.config.agents.merge)
            .unwrap_or(crate::agent_merge::Strategy::Squash);
        let Some(ui) = &mut self.tower else {
            return;
        };
        if !matches!(ask, Ask::Merge(..)) {
            ui.after_merge.clear();
        }
        ui.job_ids = match &ask {
            Ask::Merge(id, _) | Ask::Discard(id, _) => vec![id.clone()],
            Ask::DiscardAll(ids) => ids.clone(),
        };
        let state = crate::agent::state_dir();
        let (what, handle) = match ask {
            Ask::DiscardAll(ids) => (
                format!("Discarding {}", ids.join(", ")),
                std::thread::spawn(move || {
                    let mut done = Vec::new();
                    let mut failed = Vec::new();
                    for id in &ids {
                        // The user chose to drop them: open ones are quit.
                        let discarded = crate::agent_merge::stop_agent(id)
                            .and_then(|()| crate::agent_merge::discard_in(&state, id));
                        match discarded {
                            Ok(_) => done.push(id.clone()),
                            Err(e) => failed.push(format!("{id}: {e}")),
                        }
                    }
                    if failed.is_empty() {
                        Ok(format!("Discarded {}", done.join(", ")))
                    } else {
                        Err(failed.join("; "))
                    }
                }),
            ),
            Ask::Merge(id, stop) => (
                format!("Merging {id}"),
                std::thread::spawn(move || {
                    if stop {
                        crate::agent_merge::stop_agent(&id)?;
                    }
                    crate::agent_merge::merge_in(&state, &id, strategy, false)
                        .map(|note| format!("{id}: {note}"))
                }),
            ),
            Ask::Discard(id, stop) => (
                format!("Discarding {id}"),
                std::thread::spawn(move || {
                    if stop {
                        crate::agent_merge::stop_agent(&id)?;
                    }
                    crate::agent_merge::discard_in(&state, &id).map(|note| format!("{id}: {note}"))
                }),
            ),
        };
        ui.job = Some((what, handle));
    }

    /// Keys while the Tower is open. Returns false for Ctrl/Alt
    /// combinations, which go to the bindings.
    pub(super) fn tower_key(&mut self, event: &KeyEvent) -> bool {
        use winit::keyboard::{Key, NamedKey};
        if self.mods.control_key() || self.mods.alt_key() {
            return false;
        }
        if event.state != winit::event::ElementState::Pressed {
            return true;
        }
        let key = &event.logical_key;
        let ch = match key {
            Key::Character(c) => c.as_str(),
            _ => "",
        };
        let Some(ui) = &mut self.tower else {
            return false;
        };

        if let Some((ask, _)) = ui.ask.take() {
            if ch == "y" {
                self.tower_run(ask);
            } else {
                ui.after_merge.clear();
            }
            self.request_redraw();
            return true;
        }
        let busy = ui.job.is_some();
        let count = ui.rows.len();
        match (key, ch) {
            (Key::Named(NamedKey::Escape), _) | (_, "q") => self.tower = None,
            (Key::Named(NamedKey::ArrowUp), _) | (_, "k") => {
                ui.selected = ui.selected.saturating_sub(1);
                self.tower_load_log();
            }
            (Key::Named(NamedKey::ArrowDown), _) | (_, "j") => {
                ui.selected = (ui.selected + 1).min(count.saturating_sub(1));
                self.tower_load_log();
            }
            (Key::Named(NamedKey::Home), _) => {
                ui.selected = 0;
                self.tower_load_log();
            }
            (Key::Named(NamedKey::End), _) => {
                ui.selected = count.saturating_sub(1);
                self.tower_load_log();
            }
            (Key::Named(NamedKey::Enter), _) => self.tower_jump(),
            (_, "c") => {
                let session = self
                    .tower_selected()
                    .and_then(|r| crate::agent::load(&r.id).ok());
                if let Some(s) = session {
                    self.open_changes_for(s);
                }
            }
            (_, "m" | "d") if busy => {
                self.lua_set_status("Wait for the current merge or discard to finish", true)
            }
            (_, "m") => self.tower_merge(),
            (_, "d") => self.tower_discard(),
            (_, "n") => {
                self.tower = None;
                self.open_command_palette_with("New agent");
            }
            _ => {}
        }
        self.request_redraw();
        true
    }

    /// The Tower over the tab's area: the list, and beside it (when
    /// there's room) the selected session's flight log.
    pub(super) fn draw_tower(&self, gpu: &Gpu, list: &mut DrawList) {
        let Some(ui) = &self.tower else {
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
        let (ok, bad, warn) = (
            frame::hex_to_rgb(self.palette.ansi[2]),
            frame::hex_to_rgb(self.palette.ansi[1]),
            frame::hex_to_rgb(self.palette.ansi[3]),
        );
        let selected = mix(accent, bg, 0.25);

        // The list takes the whole width when it's narrow.
        let list_cols = if cols >= 100 {
            (cols * 45 / 100).clamp(48, 72)
        } else if cols >= 72 {
            cols / 2
        } else {
            cols
        };
        // A running job, a question, or a message goes across the whole
        // bottom row (the job first); otherwise the list shows its keys.
        let strip = self.tower_strip(cols, fg, bg, accent, warn);
        let left = ui::tower_view::build(
            &ui::tower_view::View {
                rows: &ui.rows,
                selected: ui.selected,
                keys: strip.is_none(),
                loading: ui.refreshed.is_some() && ui.loading.is_some() && ui.rows.is_empty(),
            },
            list_cols,
            rows,
            &ui::tower_view::Colors {
                fg,
                bg,
                dim: mix(fg, bg, 0.55),
                accent,
                ok,
                bad,
                warn,
                selected,
            },
        );
        let left_rect = Rect {
            w: list_cols as f32 * cw,
            ..area
        };
        list.panes.push((left_rect, left, 0.0, 0.0));

        let right_cols = cols.saturating_sub(list_cols + 1);
        let strip_rect = Rect {
            y: area.y + rows.saturating_sub(1) as f32 * ch,
            h: ch,
            ..area
        };
        if right_cols < 30 {
            if let Some(strip) = strip {
                list.panes.push((strip_rect, strip, 0.0, 0.0));
            }
            return;
        }
        let right_rect = Rect {
            x: area.x + (list_cols + 1) as f32 * cw,
            w: right_cols as f32 * cw,
            ..area
        };
        let line = (gpu.window.scale_factor() as f32).round().max(1.0);
        list.overlays.push(Overlay {
            rect: Rect {
                x: (area.x + list_cols as f32 * cw + cw / 2.0).round(),
                w: line,
                h: rows.saturating_sub(strip.is_some() as usize) as f32 * ch,
                ..area
            },
            color: mix(fg, bg, 0.3),
            alpha: 1.0,
        });
        let empty = crate::flight_log::summary(&[]);
        let row = ui.rows.get(ui.selected);
        let log = ui
            .log
            .as_ref()
            .filter(|l| row.is_some_and(|r| r.id == l.id));
        let mut summary = log.map(|l| l.summary).unwrap_or(empty);
        if let Some(r) = row {
            summary.state = match r.mark {
                Mark::Working => State::Working,
                Mark::Waiting => State::Waiting,
                Mark::Done => State::Done,
                Mark::Stopped => summary.state,
            };
        }
        let none = std::collections::HashSet::new();
        let title = log
            .map(|l| l.title.clone())
            .unwrap_or_else(|| "No session selected".into());
        let frame = ui::flight_panel::build(
            &ui::flight_panel::View {
                title: &title,
                branch: log.and_then(|l| l.branch.as_deref()),
                running: row.is_some_and(|r| r.running),
                summary,
                entries: log.map(|l| l.entries.as_slice()).unwrap_or_default(),
                selected: None,
                expanded: &none,
                focused: false,
                root: log.and_then(|l| l.root.as_deref()),
                key: "",
            },
            right_cols,
            rows,
            &ui::flight_panel::Colors {
                fg,
                bg: mix(fg, bg, 0.05),
                dim: mix(fg, bg, 0.55),
                accent,
                ok,
                bad,
                warn,
                selected,
            },
        );
        list.panes.push((right_rect, frame, 0.0, 0.0));
        if let Some(strip) = strip {
            list.panes.push((strip_rect, strip, 0.0, 0.0));
        }
    }

    /// The bottom row's text when it's more than the keys: a running job,
    /// a question, or a message (a merge that can't go ahead, one that
    /// finished -- the panes that usually show those are covered).
    fn tower_strip(
        &self,
        cols: usize,
        fg: [u8; 3],
        bg: [u8; 3],
        accent: [u8; 3],
        warn: [u8; 3],
    ) -> Option<Frame> {
        let ui = self.tower.as_ref()?;
        let mut strip = Frame::blank(cols, 1, fg, bg);
        let fit = |t: String| t.chars().take(cols).collect::<String>();
        match (&ui.job, &ui.ask) {
            (Some((what, _)), _) => {
                strip.put(0, 0, &fit(format!(" {what}…")), accent, bg);
            }
            (None, Some((_, q))) => {
                strip.put(0, 0, &fit(format!(" {q}  y / n")), warn, bg);
            }
            _ if self.lua_status_shown() => self.draw_lua_status(&mut strip),
            _ => return None,
        }
        Some(strip)
    }
}
