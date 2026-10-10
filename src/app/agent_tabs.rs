// src/app/agent_tabs.rs
//
// Agent sessions in the window (src/agent.rs has the sessions
// themselves). A pane is an agent pane while its foreground process
// carries CYBERTERM_AGENT_ID, checked once a second from /proc along with
// danger mode -- so it doesn't matter whether the agent was started from
// the palette, `cyberterm +agent`, a layout, or survived in the daemon.
// "New agent: ..." in the command palette starts one for the focused
// pane's directory.

use super::*;
use crate::procs;

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
        if changed {
            self.request_redraw();
        }
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
