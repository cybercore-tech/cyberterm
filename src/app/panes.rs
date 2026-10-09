// src/app/panes.rs
//
// Tabs and splits: creating and closing panes, moving focus, resizing,
// zoom, broadcast, and switching/reordering tabs. The geometry itself lives
// in `crate::layout` (pure and unit-tested); this file connects it to real
// sessions.

use super::*;
use crate::layout::{neighbor, Axis, Direction, Removed, Tab};

/// How far one resize keypress moves a divider (fraction of the split).
const RESIZE_STEP: f32 = 0.05;

impl App {
    pub(super) fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub(super) fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    pub(super) fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    /// Panes of the active tab as currently laid out.
    pub(super) fn visible_rects(&self) -> Vec<(PaneId, Rect)> {
        self.active()
            .map(|tab| {
                tab.root
                    .panes()
                    .into_iter()
                    .filter(|id| !tab.zoomed || *id == tab.focused)
                    .filter_map(|id| self.pane(id).map(|p| (id, p.rect)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The visible pane under a window position.
    pub(super) fn pane_at(&self, x: f32, y: f32) -> Option<PaneId> {
        self.visible_rects()
            .into_iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
            .map(|(id, _)| id)
    }

    /// Spawns a shell in a new pane (not yet placed in any tab).
    pub(super) fn new_session(&mut self, cwd: Option<PathBuf>) -> std::io::Result<PaneId> {
        let Some(gpu) = &self.gpu else {
            return Err(std::io::Error::other("no window yet"));
        };
        let (_, area) = self.areas(gpu);
        let (cols, rows) = gpu.renderer.grid_size(area.w, area.h);
        let (cw, ch) = gpu.renderer.cell_size();
        let size = GridSize { cols, rows };
        let (id, session) = match self.daemon.clone() {
            Some(client) => self.spawn_remote(client, cwd, size, (cw, ch))?,
            None => {
                let id = self.next_pane;
                self.next_pane += 1;
                let session = Session::spawn(
                    id,
                    self.proxy.clone(),
                    SpawnOptions {
                        size,
                        cell_width: cw,
                        cell_height: ch,
                        program: self.config.shell.program.clone(),
                        term: self.config.shell.term.clone(),
                        args: self.config.shell.args.clone(),
                        cwd,
                        term_config: self.term_config(),
                        control_socket: self.shell_socket(),
                    },
                )?;
                (id, session)
            }
        };
        session.term.lock().is_focused = self.window_focused;
        self.panes.push(Pane {
            id,
            session,
            rect: area,
            bell: None,
            bell_unseen: false,
            title: String::new(),
            pending_command: None,
        });
        Ok(id)
    }

    /// Directory for a new pane: wherever the focused pane's shell is.
    fn inherited_cwd(&self) -> Option<PathBuf> {
        self.focused_pane().and_then(|p| p.session.cwd())
    }

    pub(super) fn open_tab(&mut self, cwd: Option<PathBuf>) -> std::io::Result<PaneId> {
        let id = self.new_session(cwd)?;
        let tab_id = self.next_tab;
        self.next_tab += 1;
        self.tabs.push(Tab::new(tab_id, id));
        self.activate_tab(self.tabs.len() - 1);
        self.relayout();
        Ok(id)
    }

    pub(super) fn new_tab(&mut self) {
        let cwd = self.inherited_cwd();
        if let Err(e) = self.open_tab(cwd) {
            eprintln!("cyberterm: couldn't open a tab: {e}");
        }
    }

    pub(super) fn split(&mut self, dir: Direction) {
        let cwd = self.inherited_cwd();
        if let Err(e) = self.split_with(dir, cwd).map(|_| ()) {
            eprintln!("cyberterm: couldn't split: {e}");
        }
    }

    pub(super) fn split_with(
        &mut self,
        dir: Direction,
        cwd: Option<PathBuf>,
    ) -> std::io::Result<PaneId> {
        let Some(target) = self.active().map(|t| t.focused) else {
            return self.open_tab(cwd);
        };
        let id = self.new_session(cwd)?;
        if let Some(tab) = self.active_mut() {
            tab.zoomed = false;
            let before = matches!(dir, Direction::Left | Direction::Up);
            tab.root.split(target, dir.axis(), id, before);
        }
        self.focus_pane(id);
        self.relayout();
        Ok(id)
    }

    /// Closes a pane and its shell.
    pub(super) fn close_pane(&mut self, id: PaneId) {
        // Ending the session hangs up the shell; its exit event then finds
        // no pane and is ignored.
        if let Some(pane) = self.pane(id) {
            pane.session.kill();
        }
        self.remove_pane(id);
    }

    /// Takes a pane out of its tab (closing the tab if it was the last
    /// one) and drops its session. Exits once no tabs remain.
    pub(super) fn remove_pane(&mut self, id: PaneId) {
        let Some(index) = self.panes.iter().position(|p| p.id == id) else {
            return;
        };
        let was_focused = id == self.focused;
        self.panes.remove(index);

        let Some(tab_index) = self.tabs.iter().position(|t| t.root.contains(id)) else {
            return;
        };
        let tab = &mut self.tabs[tab_index];
        // Focus moves to the pane that takes over the space, like tmux.
        let rects = tab.root.layout(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1000.0,
                h: 1000.0,
            },
            0.0,
        );
        let successor = [
            Direction::Left,
            Direction::Up,
            Direction::Right,
            Direction::Down,
        ]
        .into_iter()
        .find_map(|d| neighbor(&rects, id, d));
        match tab.root.remove(id) {
            Removed::Empty => {
                self.tabs.remove(tab_index);
                if self.tabs.is_empty() {
                    self.exit_requested = true;
                    return;
                }
                if self.active_tab >= tab_index && self.active_tab > 0 {
                    self.active_tab -= 1;
                }
                self.activate_tab(self.active_tab.min(self.tabs.len() - 1));
            }
            Removed::Done => {
                if tab.focused == id {
                    tab.focused = successor
                        .filter(|s| tab.root.contains(*s))
                        .unwrap_or_else(|| tab.root.panes()[0]);
                }
                if tab.root.panes().len() == 1 {
                    tab.zoomed = false;
                    tab.broadcast = false;
                }
                if was_focused || tab_index == self.active_tab {
                    self.sync_focus();
                }
            }
            Removed::NotFound => {}
        }
        self.relayout();
    }

    pub(super) fn close_tab(&mut self) {
        let Some(tab) = self.active() else { return };
        for id in tab.root.panes() {
            if let Some(pane) = self.pane(id) {
                pane.session.kill();
            }
            self.panes.retain(|p| p.id != id);
        }
        self.tabs.remove(self.active_tab);
        if self.tabs.is_empty() {
            self.exit_requested = true;
            return;
        }
        self.activate_tab(self.active_tab.min(self.tabs.len() - 1));
        self.relayout();
    }

    /// Focuses a pane, switching to its tab if needed.
    pub(super) fn focus_pane(&mut self, id: PaneId) {
        let Some(tab_index) = self.tabs.iter().position(|t| t.root.contains(id)) else {
            return;
        };
        self.tabs[tab_index].focused = id;
        if tab_index != self.active_tab {
            self.activate_tab(tab_index);
        } else {
            self.sync_focus();
        }
    }

    pub(super) fn focus_direction(&mut self, dir: Direction) {
        let Some(tab) = self.active_mut() else { return };
        // Like tmux: moving focus out of a zoomed pane unzooms.
        let was_zoomed = std::mem::replace(&mut tab.zoomed, false);
        if was_zoomed {
            self.relayout();
        }
        let Some(focused) = self.active().map(|t| t.focused) else {
            return;
        };
        if let Some(next) = neighbor(&self.visible_rects(), focused, dir) {
            self.focus_pane(next);
        }
    }

    pub(super) fn focus_cycle(&mut self, step: isize) {
        let Some(tab) = self.active() else { return };
        let order = tab.root.panes();
        let Some(at) = order.iter().position(|p| *p == tab.focused) else {
            return;
        };
        let next = order[(at as isize + step).rem_euclid(order.len() as isize) as usize];
        if let Some(tab) = self.active_mut() {
            tab.zoomed = false;
        }
        self.focus_pane(next);
        self.relayout();
    }

    pub(super) fn resize_focused(&mut self, dir: Direction) {
        let Some(tab) = self.active_mut() else { return };
        let focused = tab.focused;
        if tab.root.resize(focused, dir, RESIZE_STEP) {
            self.relayout();
        }
    }

    pub(super) fn equalize(&mut self) {
        if let Some(tab) = self.active_mut() {
            tab.root.equalize();
        }
        self.relayout();
    }

    pub(super) fn toggle_zoom(&mut self) {
        if let Some(tab) = self.active_mut() {
            if tab.root.panes().len() > 1 {
                tab.zoomed = !tab.zoomed;
            }
        }
        self.relayout();
    }

    pub(super) fn toggle_broadcast(&mut self) {
        if let Some(tab) = self.active_mut() {
            tab.broadcast = !tab.broadcast;
        }
        self.request_redraw();
    }

    /// Panes that typed input should reach: every pane of the tab while
    /// broadcasting, else just the focused one.
    pub(super) fn input_targets(&self) -> Vec<PaneId> {
        match self.active() {
            Some(tab) if tab.broadcast => tab.root.panes(),
            Some(tab) => vec![tab.focused],
            None => Vec::new(),
        }
    }

    pub(super) fn activate_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.active_tab = index;
        for id in self.tabs[index].root.panes() {
            if let Some(pane) = self.pane_mut(id) {
                pane.bell_unseen = false;
            }
        }
        self.sync_focus();
    }

    pub(super) fn cycle_tab(&mut self, step: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        self.activate_tab((self.active_tab as isize + step).rem_euclid(n) as usize);
    }

    pub(super) fn move_tab(&mut self, step: isize) {
        let n = self.tabs.len() as isize;
        let to = self.active_tab as isize + step;
        if n < 2 || to < 0 || to >= n {
            return;
        }
        self.tabs.swap(self.active_tab, to as usize);
        self.active_tab = to as usize;
        self.layout_dirty = true;
        self.request_redraw();
    }

    /// Makes `self.focused` match the active tab, sends focus in/out
    /// reports to programs that asked for them, and updates the title.
    pub(super) fn sync_focus(&mut self) {
        let Some(new) = self.active().map(|t| t.focused) else {
            return;
        };
        let old = std::mem::replace(&mut self.focused, new);
        if old != new && self.window_focused {
            for (id, report) in [(old, &b"\x1b[O"[..]), (new, &b"\x1b[I"[..])] {
                if let Some(pane) = self.pane(id) {
                    if pane
                        .session
                        .term
                        .lock()
                        .mode()
                        .contains(TermMode::FOCUS_IN_OUT)
                    {
                        pane.session.write(report);
                    }
                }
            }
        }
        if old != new {
            self.mouse.hover = None;
            self.menu = None;
        }
        self.update_window_title();
        self.request_redraw();
    }

    pub(super) fn update_window_title(&self) {
        let title = self
            .focused_pane()
            .map(|p| p.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Cyberterm".to_string());
        if let Some(gpu) = &self.gpu {
            gpu.window.set_title(&title);
        }
    }

    /// What a tab is called in the tab bar.
    pub(super) fn tab_title(&self, tab: &Tab) -> String {
        if let Some(title) = &tab.title {
            return title.clone();
        }
        let Some(pane) = self.pane(tab.focused) else {
            return "shell".to_string();
        };
        if !pane.title.is_empty() {
            return pane.title.clone();
        }
        pane.session
            .cwd()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "shell".to_string())
    }

    pub(super) fn new_window(&self) {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let mut command = Command::new(exe);
        if let Some(cwd) = self.inherited_cwd() {
            command.current_dir(cwd);
        }
        spawn_detached(&mut command);
    }

    /// Divider under a window position (with a few pixels of slack, so
    /// thin dividers are easy to grab).
    pub(super) fn divider_at(&self, x: f32, y: f32) -> Option<crate::layout::Divider> {
        let gpu = self.gpu.as_ref()?;
        let tab = self.active()?;
        if tab.zoomed {
            return None;
        }
        let (_, area) = self.areas(gpu);
        let slack = 3.0;
        tab.root
            .dividers(area, self.gap(gpu))
            .into_iter()
            .find(|d| {
                let r = d.rect;
                x >= r.x - slack
                    && x < r.x + r.w + slack
                    && y >= r.y - slack
                    && y < r.y + r.h + slack
            })
    }

    /// Moves a dragged divider to follow the pointer.
    pub(super) fn drag_divider(&mut self, divider: &crate::layout::Divider, x: f32, y: f32) {
        let Some(gap) = self.gpu.as_ref().map(|g| self.gap(g)) else {
            return;
        };
        let p = divider.parent;
        let ratio = match divider.axis {
            Axis::Horizontal => (x - p.x - gap / 2.0) / (p.w - gap).max(1.0),
            Axis::Vertical => (y - p.y - gap / 2.0) / (p.h - gap).max(1.0),
        };
        if let Some(tab) = self.active_mut() {
            tab.root.set_ratio_at(&divider.path, ratio);
        }
        self.relayout();
    }

    /// Types a pane's queued command once its shell has drawn a prompt.
    pub(super) fn flush_pending_command(&mut self, index: usize) {
        let pane = &mut self.panes[index];
        let Some((_, queued)) = &pane.pending_command else {
            return;
        };
        let ready = pane.session.shell.lock().prompts > 0;
        if ready || queued.elapsed() >= COMMAND_READY_TIMEOUT {
            if let Some((command, _)) = pane.pending_command.take() {
                pane.session.write(format!("{command}\r").into_bytes());
            }
        }
    }

    /// Opens the tabs a layout file describes, after the existing ones,
    /// and switches to the first of them.
    pub(super) fn open_layout(
        &mut self,
        plans: Vec<crate::layout_file::TabPlan>,
    ) -> std::io::Result<()> {
        let first_new = self.tabs.len();
        for plan in plans {
            let mut focus = None;
            let root = self.spawn_plan(&plan.root, &mut focus)?;
            let first = root.panes()[0];
            let mut tab = Tab::new(self.next_tab, first);
            self.next_tab += 1;
            tab.root = root;
            tab.focused = focus.unwrap_or(first);
            tab.title = plan.title;
            self.tabs.push(tab);
        }
        self.activate_tab(first_new);
        self.relayout();
        Ok(())
    }

    fn spawn_plan(
        &mut self,
        plan: &crate::layout_file::Plan,
        focus: &mut Option<PaneId>,
    ) -> std::io::Result<crate::layout::Node> {
        use crate::layout::Node;
        use crate::layout_file::Plan;
        Ok(match plan {
            Plan::Pane {
                cwd,
                command,
                focus: wants_focus,
            } => {
                let cwd = if cwd.is_dir() {
                    Some(cwd.clone())
                } else {
                    eprintln!(
                        "cyberterm: layout directory {} doesn't exist",
                        cwd.display()
                    );
                    None
                };
                let id = self.new_session(cwd)?;
                self.run_in(id, command.as_deref());
                if *wants_focus {
                    *focus = Some(id);
                }
                Node::Leaf(id)
            }
            Plan::Split { axis, ratio, a, b } => Node::Split {
                axis: *axis,
                ratio: *ratio,
                a: Box::new(self.spawn_plan(a, focus)?),
                b: Box::new(self.spawn_plan(b, focus)?),
            },
        })
    }
}
