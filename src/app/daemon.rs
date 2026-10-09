// src/app/daemon.rs
//
// The window's side of the session daemon: attaching to or creating a
// session at startup, rebuilding its tabs and splits from the saved
// layout, saving the layout as it changes, and detaching on close.

use super::*;
use crate::mux::client::DaemonClient;
use crate::mux::protocol::{ClientMsg, PaneInfo, Size, TermSettings};

/// What `cyberterm +attach` asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachTarget {
    /// `None` = the most recently detached session.
    pub name: Option<String>,
    /// Take the session over from a client that has it attached.
    pub force: bool,
}

impl App {
    pub(super) fn term_settings(&self) -> TermSettings {
        crate::mux::settings_from_config(&self.config)
    }

    /// Connects to (or starts) the daemon and attaches to a session.
    /// Returns true when an existing session's tabs were restored.
    pub(super) fn start_daemon_session(&mut self) -> std::io::Result<bool> {
        let client = DaemonClient::connect_or_start(crate::session::sink_for(self.proxy.clone()))?;
        let target = self.startup_attach.take();
        let explicit = target.is_some();
        let reattach = self.config.daemon.reattach && self.startup_layout.is_none();
        let attached = match &target {
            Some(t) => client.attach(t.name.clone(), t.force),
            None if reattach => client.attach(None, false),
            None => Err(std::io::Error::other("new session")),
        };
        let (name, layout, panes, note) = match attached {
            Ok((name, layout, panes)) => (name, layout, panes, None),
            Err(e) => {
                let (name, layout, panes) = client.new_session(None)?;
                let note = explicit.then(|| format!("cyberterm: couldn't attach: {e}"));
                (name, layout, panes, note)
            }
        };
        self.daemon = Some(client);
        self.link_session_socket(&name);
        self.session_name = Some(name);
        self.push_palette();
        let restored = self.restore(&layout, &panes);
        if let Some(note) = note {
            if !restored {
                let id = self.open_tab(None)?;
                let message = note.replace('\'', "'\\''");
                self.run_in(id, Some(&format!("printf '%s\\n' '{message}'")));
                return Ok(true);
            }
        }
        Ok(restored)
    }

    /// Shells in this session see `CYBERTERM_SOCKET` pointing at a stable
    /// per-session link, re-pointed at whichever window is attached, so
    /// `cyberterm +ctl` inside them keeps working across reattaches.
    fn link_session_socket(&mut self, name: &str) {
        let Some(target) = self.control.as_ref().map(|c| c.path().to_path_buf()) else {
            return;
        };
        let link = crate::control::socket_dir().join(format!("session-{name}.sock"));
        let _ = std::fs::remove_file(&link);
        if std::os::unix::fs::symlink(&target, &link).is_ok() {
            self.session_link = Some(link);
        }
    }

    /// The socket path given to new shells.
    pub(super) fn shell_socket(&self) -> Option<PathBuf> {
        self.session_link
            .clone()
            .or_else(|| self.control.as_ref().map(|c| c.path().to_path_buf()))
    }

    /// Recreates tabs from a saved layout, keeping only panes that still
    /// exist; panes the layout doesn't mention get a tab each.
    fn restore(&mut self, layout: &serde_json::Value, panes: &[PaneInfo]) -> bool {
        let Some(client) = self.daemon.clone() else {
            return false;
        };
        for info in panes {
            let size = GridSize {
                cols: info.size.cols.max(2) as usize,
                rows: info.size.rows.max(1) as usize,
            };
            if let Some(session) = Session::remote(info.id, client.clone(), size) {
                self.panes.push(Pane {
                    id: info.id,
                    session,
                    rect: Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 1.0,
                        h: 1.0,
                    },
                    bell: None,
                    bell_unseen: false,
                    title: info.title.clone(),
                    pending_command: None,
                    // Don't notify about commands that finished while
                    // detached.
                    notified_mark: info.shell.blocks.iter().map(|b| b.mark).max().unwrap_or(0),
                    block_sig: (0, None),
                });
            }
        }
        let ids: Vec<PaneId> = self.panes.iter().map(|p| p.id).collect();
        let (tabs, active) = crate::mux::layout_doc::decode(layout, &ids, &mut self.next_tab);
        self.tabs = tabs;
        self.active_tab = active;
        if self.tabs.is_empty() {
            return false;
        }
        self.activate_tab(self.active_tab.min(self.tabs.len() - 1));
        self.relayout();
        true
    }

    /// Starts a shell in the daemon and wraps its replica as a session.
    pub(super) fn spawn_remote(
        &mut self,
        client: std::sync::Arc<DaemonClient>,
        cwd: Option<PathBuf>,
        size: GridSize,
        cell: (f32, f32),
    ) -> std::io::Result<(PaneId, Session)> {
        let mut env = std::collections::HashMap::new();
        env.insert("TERM_PROGRAM".to_string(), "cyberterm".to_string());
        env.insert(
            "TERM_PROGRAM_VERSION".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        );
        if let Some(socket) = self.shell_socket() {
            env.insert(
                "CYBERTERM_SOCKET".to_string(),
                socket.to_string_lossy().into_owned(),
            );
        }
        if let Some(term) = self
            .config
            .shell
            .term
            .clone()
            .filter(|t| !t.trim().is_empty())
        {
            env.insert("TERM".to_string(), term);
        }
        let wire_size = Size {
            cols: size.cols as u16,
            rows: size.rows as u16,
            cell_width: cell.0 as u16,
            cell_height: cell.1 as u16,
        };
        let settings = self.term_settings();
        let (program, args) = (
            self.config.shell.program.clone(),
            self.config.shell.args.clone(),
        );
        let info = client.spawn(|req| ClientMsg::Spawn {
            req,
            size: wire_size,
            settings,
            program,
            args,
            cwd,
            env,
        })?;
        let session = Session::remote(info.id, client, size)
            .ok_or_else(|| std::io::Error::other("the daemon's pane vanished"))?;
        Ok((info.id, session))
    }

    /// Stores the current tabs and splits with the session.
    pub(super) fn save_layout(&mut self) {
        self.layout_dirty = false;
        let Some(client) = &self.daemon else { return };
        let layout = crate::mux::layout_doc::encode(&self.tabs, self.active_tab);
        let _ = client.send(&ClientMsg::SaveLayout { layout });
    }

    /// Colors the daemon reports when programs query them.
    pub(super) fn push_palette(&self) {
        if let Some(client) = &self.daemon {
            let p = self.palette;
            let _ = client.send(&ClientMsg::SetPalette {
                ansi: p.ansi,
                fg: p.fg,
                bg: p.bg,
                cursor: p.cursor,
            });
        }
    }

    /// Window closing with a daemon: save the layout and detach, leaving
    /// every shell running.
    pub(super) fn detach(&mut self) {
        if self.daemon.is_none() {
            return;
        }
        self.save_layout();
        if let Some(client) = &self.daemon {
            let _ = client.send(&ClientMsg::Detach);
        }
        self.unlink_session_socket();
    }

    /// Removes `session-<name>.sock` if it still points at this window
    /// (another window may have re-linked it since).
    pub(super) fn unlink_session_socket(&mut self) {
        if let Some(link) = self.session_link.take() {
            let ours = self.control.as_ref().map(|c| c.path().to_path_buf());
            if std::fs::read_link(&link).ok() == ours {
                let _ = std::fs::remove_file(link);
            }
        }
    }

    /// The daemon went away, or detached this window because the session
    /// was attached elsewhere: say so in every pane and stop sending to it.
    pub(super) fn daemon_lost(&mut self, reason: &str) {
        let Some(client) = self.daemon.take() else {
            return;
        };
        // Panes still hold the client; close it so the daemon doesn't wait
        // on this window any more.
        client.close();
        let notice = format!("\r\n\x1b[0;1;31m[cyberterm: {reason}]\x1b[0m\r\n");
        let notice = notice.as_bytes();
        for pane in &self.panes {
            let mut term = pane.session.term.lock();
            let mut parser: alacritty_terminal::vte::ansi::Processor<
                alacritty_terminal::vte::ansi::StdSyncHandler,
            > = alacritty_terminal::vte::ansi::Processor::new();
            parser.advance(&mut *term, notice);
        }
        self.request_redraw();
    }
}
