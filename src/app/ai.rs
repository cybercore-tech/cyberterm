// src/app/ai.rs
//
// The AI panel at the bottom of a pane:
// - Ask (Ctrl+Shift+I): type what you want in plain English, get one
//   command back;
// - Explain (a failed command's menu, or Ctrl+Shift+J for the newest
//   failure): what went wrong and, when there is one, a fix.
// Answers show the command with a risk rating (the model's, raised by
// Cyberterm's own check). Enter types the command at the prompt -- it
// never runs by itself; c copies it.
//
// Requests run on a worker thread and come back as `UserEvent::Ai`, so the
// window never waits on the network.

use std::path::Path;

use super::*;
use crate::ai::{self, Answer, Risk, Task};
use crate::shell::BlockMeta;
use blocks::BlockRef;

enum PanelState {
    Input(String),
    Waiting,
    Done(Answer),
    Failed(String),
}

pub(super) struct AiPanel {
    /// The pane the request is about (and where a command is typed).
    pane: PaneId,
    title: String,
    /// Matches the answer to the request (an older answer is ignored).
    seq: u64,
    state: PanelState,
    scroll: usize,
}

/// Word-wraps text to `width` columns, keeping blank lines.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        let mut len = 0;
        for word in para.split_whitespace() {
            let w = word.chars().count();
            if len > 0 && len + 1 + w > width {
                out.push(std::mem::take(&mut line));
                len = 0;
            }
            // A word longer than the line is broken up.
            let mut word: Vec<char> = word.chars().collect();
            while word.len() > width {
                if len > 0 {
                    out.push(std::mem::take(&mut line));
                    len = 0;
                }
                out.push(word.drain(..width).collect());
            }
            if len > 0 {
                line.push(' ');
                len += 1;
            }
            len += word.len();
            line.extend(word);
        }
        out.push(line);
    }
    out
}

impl App {
    pub(super) fn ai_open(&self) -> bool {
        self.ai_panel.is_some()
    }

    /// Ctrl+Shift+I: ask for a command in plain English.
    pub(super) fn ai_ask(&mut self) {
        let Some(pane) = self.focused_pane().map(|p| p.id) else {
            return;
        };
        self.ai_panel = Some(AiPanel {
            pane,
            title: "Ask for a command".into(),
            seq: 0,
            state: PanelState::Input(String::new()),
            scroll: 0,
        });
        self.request_redraw();
    }

    /// Ctrl+Shift+J: explain the newest failed command in the focused pane.
    pub(super) fn ai_explain_last(&mut self) {
        let failed = self.focused_pane().and_then(|pane| {
            let shell = pane.session.shell.lock();
            let meta = shell
                .blocks
                .iter()
                .rev()
                .find(|b| b.is_command() && b.exit.is_some_and(|e| e != 0))?
                .clone();
            Some((
                BlockRef {
                    pane: pane.id,
                    mark: meta.mark,
                },
                meta,
            ))
        });
        match failed {
            Some((block, meta)) => self.ai_explain(block, &meta),
            None => self.ai_message("No failed command in this pane (needs shell integration)."),
        }
    }

    pub(super) fn ai_explain(&mut self, block: BlockRef, meta: &BlockMeta) {
        let command = meta.command.clone().unwrap_or_default();
        let output = self
            .span_and_output(block)
            .map(|(_, text)| text)
            .unwrap_or_default();
        let context = self.ai_context(block.pane, meta.cwd.clone());
        let short: String = command.chars().take(40).collect();
        let task = Task::Explain {
            command,
            exit: meta.exit,
            output,
            context,
        };
        self.ai_start(block.pane, format!("Explain `{short}`"), task);
    }

    fn ai_message(&mut self, text: &str) {
        let pane = self.focused;
        self.ai_panel = Some(AiPanel {
            pane,
            title: "AI".into(),
            seq: 0,
            state: PanelState::Failed(text.into()),
            scroll: 0,
        });
        self.request_redraw();
    }

    fn ai_context(&self, pane: PaneId, cwd: Option<PathBuf>) -> ai::Context {
        let Some(p) = self.pane(pane) else {
            return ai::Context::default();
        };
        let cwd = cwd.or_else(|| p.session.cwd());
        let recent = {
            let shell = p.session.shell.lock();
            let commands: Vec<String> = shell
                .blocks
                .iter()
                .filter_map(|b| b.command.clone())
                .collect();
            commands[commands.len().saturating_sub(5)..].to_vec()
        };
        ai::Context {
            cwd: cwd.map(|c| c.to_string_lossy().into_owned()),
            shell: blocks::shell_name(self.config.shell.program.as_deref()),
            // Filled in on the worker thread (it runs git).
            git: None,
            recent,
        }
    }

    fn ai_start(&mut self, pane: PaneId, title: String, mut task: Task) {
        self.ai_seq += 1;
        let seq = self.ai_seq;
        self.ai_panel = Some(AiPanel {
            pane,
            title,
            seq,
            state: PanelState::Waiting,
            scroll: 0,
        });
        let cfg = self.config.ai.clone();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let context = match &mut task {
                Task::Explain { context, .. } | Task::Suggest { context, .. } => context,
            };
            if let Some(cwd) = context.cwd.clone() {
                context.git = ai::git_state(Path::new(&cwd));
            }
            let result = ai::ask(&cfg, &task);
            let _ = proxy.send_event(UserEvent::Ai(seq, result));
        });
        self.request_redraw();
    }

    pub(super) fn on_ai_answer(&mut self, seq: u64, result: Result<Answer, String>) {
        let Some(panel) = self.ai_panel.as_mut().filter(|p| p.seq == seq) else {
            return;
        };
        panel.state = match result {
            Ok(answer) => PanelState::Done(answer),
            Err(e) => PanelState::Failed(e),
        };
        panel.scroll = 0;
        self.request_redraw();
    }

    pub(super) fn ai_key(&mut self, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.control_key();
        let Some(panel) = self.ai_panel.as_mut() else {
            return;
        };
        let key = &event.logical_key;
        if matches!(key, Key::Named(NamedKey::Escape)) {
            self.ai_panel = None;
            self.request_redraw();
            return;
        }
        match &mut panel.state {
            PanelState::Input(text) => match key {
                Key::Named(NamedKey::Enter) => {
                    let request = text.trim().to_string();
                    if request.is_empty() {
                        return;
                    }
                    let pane = panel.pane;
                    let context = self.ai_context(pane, None);
                    let short: String = request.chars().take(40).collect();
                    self.ai_start(
                        pane,
                        format!("Ask: {short}"),
                        Task::Suggest { request, context },
                    );
                }
                Key::Named(NamedKey::Backspace) => {
                    if ctrl {
                        let trimmed = text.trim_end();
                        let cut = trimmed.rfind(' ').map_or(0, |i| i + 1);
                        text.truncate(cut);
                    } else {
                        text.pop();
                    }
                }
                _ if ctrl => {}
                _ => {
                    if let Some(t) = event
                        .text
                        .as_deref()
                        .filter(|t| !t.chars().any(char::is_control))
                    {
                        text.push_str(t);
                    }
                }
            },
            PanelState::Waiting => {}
            PanelState::Failed(_) => {
                if matches!(key, Key::Named(NamedKey::Enter)) {
                    self.ai_panel = None;
                }
            }
            PanelState::Done(answer) => match key {
                Key::Named(NamedKey::ArrowDown) => panel.scroll += 1,
                Key::Named(NamedKey::ArrowUp) => panel.scroll = panel.scroll.saturating_sub(1),
                Key::Named(NamedKey::Enter) => {
                    let command = answer.command.clone();
                    let pane = panel.pane;
                    self.ai_panel = None;
                    if let Some(command) = command {
                        self.type_at_prompt(pane, &command);
                    }
                }
                Key::Character(c) if c.as_str() == "c" && !ctrl => {
                    if let Some(command) = answer.command.clone() {
                        self.copy_text(&command);
                    }
                }
                _ => {}
            },
        }
        self.request_redraw();
    }

    /// Puts a command on the pane's prompt as a paste, without Enter.
    fn type_at_prompt(&mut self, pane: PaneId, command: &str) {
        let Some(p) = self.pane(pane) else { return };
        let bracketed = {
            let mut term = p.session.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };
        // A newline inside a non-bracketed paste would run it; flatten.
        let text = if bracketed {
            command.to_string()
        } else {
            command.replace(['\r', '\n'], " ")
        };
        p.session.write(paste::paste_bytes(&text, bracketed));
        self.focus_pane(pane);
    }

    /// The pane the panel belongs on: its own pane when visible, else the
    /// focused one.
    pub(super) fn ai_panel_pane(&self) -> Option<PaneId> {
        let panel = self.ai_panel.as_ref()?;
        let visible = self.visible_rects().iter().any(|(id, _)| *id == panel.pane);
        Some(if visible { panel.pane } else { self.focused })
    }

    pub(super) fn draw_ai(&self, frame: &mut Frame) {
        let Some(panel) = &self.ai_panel else { return };
        let fg = frame::hex_to_rgb(self.palette.fg);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let accent = frame::hex_to_rgb(self.palette.ansi[4]);
        let command_color = frame::hex_to_rgb(self.palette.ansi[6]);
        let dim = frame::hex_to_rgb(self.palette.ansi[8]);
        let colors = [
            frame::hex_to_rgb(self.palette.ansi[2]),
            frame::hex_to_rgb(self.palette.ansi[3]),
            frame::hex_to_rgb(self.palette.ansi[1]),
        ];
        let cols = frame.cols;
        let width = cols.saturating_sub(4);

        // (text, fg) rows below the header; the last is the key hint.
        let mut body: Vec<(String, [u8; 3])> = Vec::new();
        let keys;
        match &panel.state {
            PanelState::Input(text) => {
                body.push((format!("› {text}▏"), fg));
                body.push((
                    "e.g. \"find files over 100MB here\", \"undo my last commit but keep the changes\"".into(),
                    dim,
                ));
                keys = "Enter ask · Esc cancel";
            }
            PanelState::Waiting => {
                body.push(("Thinking…".into(), dim));
                keys = "Esc cancel";
            }
            PanelState::Failed(e) => {
                for line in wrap(e, width) {
                    body.push((line, colors[2]));
                }
                keys = "Esc close";
            }
            PanelState::Done(answer) => {
                let mut text: Vec<(String, [u8; 3])> = wrap(&answer.explanation, width)
                    .into_iter()
                    .map(|l| (l, fg))
                    .collect();
                // Keep the command and its risk on screen; scroll the
                // explanation above them.
                let room = (frame.rows * 3 / 5).saturating_sub(6).max(3);
                let first = panel.scroll.min(text.len().saturating_sub(room));
                let more = text.len() > first + room;
                text = text.into_iter().skip(first).take(room).collect();
                if more {
                    text.push(("  ↓ more".into(), dim));
                }
                body.extend(text);
                if let Some(command) = &answer.command {
                    body.push((String::new(), fg));
                    for line in wrap(&format!("$ {command}"), width) {
                        body.push((line, command_color));
                    }
                    let risk = answer.risk;
                    let mut line = format!("risk: {}", risk.label());
                    if !answer.warnings.is_empty() {
                        line.push_str(&format!(" — {}", answer.warnings.join(", ")));
                    }
                    body.push((line, colors[risk as usize]));
                    keys = if risk == Risk::Dangerous {
                        "Enter type it at the prompt (read it first!) · c copy · ↑↓ scroll · Esc close"
                    } else {
                        "Enter type it at the prompt · c copy · ↑↓ scroll · Esc close"
                    };
                } else {
                    keys = "↑↓ scroll · Esc close";
                }
            }
        }

        let height = 1 + body.len() + 1;
        if frame.rows < height + 1 {
            return;
        }
        let top = frame.rows - height;
        frame.fill(top, 0, bg, accent);
        let col = frame.put(top, 1, "◆ AI · ", bg, accent);
        frame.put(top, col, &panel.title, bg, accent);
        let who = ai::describe(&self.config.ai);
        let who_len = who.chars().count() + 1;
        if who_len + col + panel.title.chars().count() + 2 < cols {
            frame.put(top, cols - who_len, &who, bg, accent);
        }
        for (i, (text, color)) in body.iter().enumerate() {
            frame.fill(top + 1 + i, 0, fg, bg);
            frame.put(top + 1 + i, 2, text, *color, bg);
        }
        frame.fill(frame.rows - 1, 0, fg, bg);
        frame.put(frame.rows - 1, 1, keys, fg, bg);
        frame.cursor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::wrap;

    #[test]
    fn wraps_on_words_and_breaks_long_ones() {
        assert_eq!(wrap("aaa bbb ccc", 8), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("x\n\ny", 8), vec!["x", "", "y"]);
        assert_eq!(wrap("abcdefghijkl", 8), vec!["abcdefgh", "ijkl"]);
    }
}
