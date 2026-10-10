// src/app/command_palette.rs
//
// The command palette (Ctrl+Shift+P): one fuzzy-searchable list of
// everything you can do -- every action (with its key), Lua commands,
// open panes and tabs, themes, and recent commands from history. Enter
// runs the selected item (a history command is typed at the prompt;
// Ctrl+Enter also runs it). Items picked recently come first.

use super::*;
use crate::fuzzy::fuzzy;
use crate::input::bindings::ACTIONS;

/// Items kept from the history.
const HISTORY_ITEMS: usize = 200;
/// Recent picks remembered for ordering.
const RECENT: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Target {
    Action(Action),
    Lua(String),
    Pane(PaneId),
    Tab(usize),
    Theme(String),
    History(String),
    Agent(String),
}

#[derive(Clone, Debug)]
struct Item {
    kind: &'static str,
    label: String,
    hint: String,
    target: Target,
}

pub(super) struct CommandPalette {
    query: String,
    items: Vec<Item>,
    /// Indices into `items` with their matched char positions, best first.
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    scroll: usize,
}

impl App {
    pub(super) fn command_palette_open(&self) -> bool {
        self.command_palette.is_some()
    }

    pub(super) fn toggle_command_palette(&mut self) {
        if self.command_palette.take().is_some() {
            self.request_redraw();
            return;
        }
        let items = self.palette_items();
        let mut p = CommandPalette {
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
        };
        self.palette_filter(&mut p);
        self.command_palette = Some(p);
        self.request_redraw();
    }

    fn palette_items(&self) -> Vec<Item> {
        let mut items = Vec::new();
        for (action, _name, description) in ACTIONS {
            if *action == Action::CommandPalette {
                continue;
            }
            items.push(Item {
                kind: "action",
                label: (*description).to_string(),
                hint: self.bindings.hint(*action),
                target: Target::Action(*action),
            });
        }
        for l in crate::agent::launchers(&self.config.agents) {
            items.push(Item {
                kind: "agent",
                label: format!("New agent: {}", l.label),
                hint: String::new(),
                target: Target::Agent(l.name),
            });
        }
        for name in self.lua_command_names() {
            items.push(Item {
                kind: "lua",
                label: format!("Lua: {name}"),
                hint: String::new(),
                target: Target::Lua(name),
            });
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let short = |p: PathBuf| {
            let s = p.to_string_lossy().into_owned();
            match s.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => s,
            }
        };
        for (ti, tab) in self.tabs.iter().enumerate() {
            items.push(Item {
                kind: "tab",
                label: format!("Tab {}: {}", ti + 1, self.tab_title(tab)),
                hint: String::new(),
                target: Target::Tab(ti),
            });
            for id in tab.root.panes() {
                let Some(pane) = self.pane(id) else { continue };
                let mut label = format!("Pane {id}");
                if !pane.title.is_empty() {
                    label.push_str(&format!(": {}", pane.title));
                }
                if let Some(cwd) = pane.session.cwd() {
                    label.push_str(&format!(" · {}", short(cwd)));
                }
                items.push(Item {
                    kind: "pane",
                    label,
                    hint: format!("tab {}", ti + 1),
                    target: Target::Pane(id),
                });
            }
        }
        for theme in &self.theme_menu.registry.themes {
            items.push(Item {
                kind: "theme",
                label: format!("Theme: {}", theme.name),
                hint: if theme.name == self.config.theme {
                    "current".into()
                } else {
                    String::new()
                },
                target: Target::Theme(theme.name.clone()),
            });
        }
        if let Ok(store) = crate::history::Store::open(&crate::history::default_path()) {
            let query = crate::history::Query {
                limit: HISTORY_ITEMS * 3,
                ..Default::default()
            };
            let mut seen = std::collections::HashSet::new();
            for e in store.search(&query).unwrap_or_default() {
                let command = e.command.trim().to_string();
                if command.is_empty() || !seen.insert(command.clone()) {
                    continue;
                }
                items.push(Item {
                    kind: "history",
                    label: command.replace('\n', " ⏎ "),
                    hint: match e.exit {
                        Some(0) => "✓".into(),
                        Some(code) => format!("✗ {code}"),
                        None => String::new(),
                    },
                    target: Target::History(command),
                });
                if seen.len() >= HISTORY_ITEMS {
                    break;
                }
            }
        }
        items
    }

    /// Re-ranks for the current query: best score first; with no query,
    /// recent picks first, then the natural order (history last).
    fn palette_filter(&self, p: &mut CommandPalette) {
        let recent_rank = |item: &Item| {
            self.palette_recent
                .iter()
                .rev()
                .position(|r| *r == item.target)
        };
        let mut scored: Vec<(i64, usize, Vec<usize>)> = p
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let (mut score, pos) = fuzzy(&p.query, &item.label)?;
                // History lines and the (possibly 1,500) themes match almost
                // anything loosely: only keep fairly compact matches there.
                // The match is checked past the "Theme: " prefix.
                if matches!(item.kind, "history" | "theme") && pos.len() > 1 {
                    let span = pos[pos.len() - 1] - pos[0] + 1;
                    let limit = if item.kind == "theme" {
                        pos.len() * 2 + 2
                    } else {
                        (pos.len() * 3).max(8)
                    };
                    if span > limit {
                        return None;
                    }
                }
                if let Some(r) = recent_rank(item) {
                    score += 1000 - r as i64 * 10;
                }
                // History is plentiful; keep it below exact-ish matches.
                if item.kind == "history" {
                    score -= 15;
                }
                Some((score, i, pos))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        p.matches = scored.into_iter().map(|(_, i, pos)| (i, pos)).collect();
        p.selected = 0;
        p.scroll = 0;
    }

    pub(super) fn command_palette_key(&mut self, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.control_key();
        let Some(mut p) = self.command_palette.take() else {
            return;
        };
        let n = p.matches.len();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.request_redraw();
                return;
            }
            Key::Named(NamedKey::ArrowDown) if n > 0 => p.selected = (p.selected + 1) % n,
            Key::Named(NamedKey::ArrowUp) if n > 0 => p.selected = (p.selected + n - 1) % n,
            Key::Named(NamedKey::PageDown) if n > 0 => p.selected = (p.selected + 10).min(n - 1),
            Key::Named(NamedKey::PageUp) => p.selected = p.selected.saturating_sub(10),
            Key::Named(NamedKey::Enter) => {
                let target = p
                    .matches
                    .get(p.selected)
                    .map(|(i, _)| p.items[*i].target.clone());
                if let Some(target) = target {
                    self.palette_run(target, ctrl);
                }
                self.request_redraw();
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                if ctrl {
                    let trimmed = p.query.trim_end();
                    let cut = trimmed.rfind(' ').map_or(0, |i| i + 1);
                    p.query.truncate(cut);
                } else {
                    p.query.pop();
                }
                self.palette_filter(&mut p);
            }
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("n") && n > 0 => {
                p.selected = (p.selected + 1) % n
            }
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("p") && n > 0 => {
                p.selected = (p.selected + n - 1) % n
            }
            _ if ctrl => {}
            _ => {
                if let Some(t) = event
                    .text
                    .as_deref()
                    .filter(|t| !t.chars().any(char::is_control))
                {
                    p.query.push_str(t);
                    self.palette_filter(&mut p);
                }
            }
        }
        self.command_palette = Some(p);
        self.request_redraw();
    }

    fn palette_run(&mut self, target: Target, ctrl: bool) {
        self.palette_recent.retain(|t| *t != target);
        self.palette_recent.push(target.clone());
        if self.palette_recent.len() > RECENT {
            self.palette_recent.remove(0);
        }
        match target {
            Target::Action(action) => self.perform(action),
            Target::Lua(name) => {
                if let Err(e) = self.lua_command(&name, serde_json::Value::Null) {
                    eprintln!("cyberterm: Lua command {name}: {e}");
                }
            }
            Target::Pane(id) => self.focus_pane(id),
            Target::Agent(name) => self.start_agent(&name),
            Target::Tab(index) => self.activate_tab(index),
            Target::Theme(name) => {
                if let Some(theme) = self
                    .theme_menu
                    .registry
                    .themes
                    .iter()
                    .find(|t| t.name == name)
                    .cloned()
                {
                    self.select_theme(&theme);
                }
            }
            Target::History(command) => {
                self.paste(&command);
                if ctrl {
                    if let Some(pane) = self.focused_pane() {
                        pane.session.write(&b"\r"[..]);
                    }
                }
            }
        }
    }

    /// The palette as a box over the top of the focused pane.
    pub(super) fn draw_command_palette(&self, frame: &mut Frame) {
        let Some(p) = &self.command_palette else {
            return;
        };
        let fg = frame::hex_to_rgb(self.palette.fg);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let accent = frame::hex_to_rgb(self.palette.cursor);
        let dim = frame::hex_to_rgb(self.palette.ansi[8]);
        let border = frame::hex_to_rgb(self.palette.ansi[4]);
        let panel = frame::hex_to_rgb(self.palette.ansi[0]);
        let panel = if panel == bg { bg } else { panel };

        let width = frame.cols.saturating_sub(4).min(96);
        if width < 24 || frame.rows < 8 {
            return;
        }
        let list_rows = frame.rows.saturating_sub(7).clamp(3, 14);
        let left = (frame.cols - width) / 2;
        let top = 1;
        let inner = width - 2;

        let hline = |frame: &mut Frame, row: usize, l: char, m: char, r: char| {
            let line: String = std::iter::once(l)
                .chain(std::iter::repeat_n(m, inner))
                .chain(std::iter::once(r))
                .collect();
            frame.put(row, left, &line, border, panel);
        };
        let blank = |frame: &mut Frame, row: usize| {
            frame.put(row, left, "│", border, panel);
            frame.put(row, left + 1, &" ".repeat(inner), fg, panel);
            frame.put(row, left + width - 1, "│", border, panel);
        };

        hline(frame, top, '╭', '─', '╮');
        // Query line.
        blank(frame, top + 1);
        let col = frame.put(top + 1, left + 2, "› ", accent, panel);
        let shown: String = p.query.chars().take(inner.saturating_sub(16)).collect();
        let col = frame.put(top + 1, col, &shown, fg, panel);
        frame.put(top + 1, col, "▏", accent, panel);
        let count = format!("{}/{}", p.matches.len(), p.items.len());
        frame.put(top + 1, left + width - 2 - count.len(), &count, dim, panel);
        hline(frame, top + 2, '├', '─', '┤');

        // The list, scrolled to keep the selection visible.
        let first = if p.selected >= list_rows {
            p.selected + 1 - list_rows
        } else {
            0
        };
        for r in 0..list_rows {
            let row = top + 3 + r;
            blank(frame, row);
            let Some((index, positions)) = p.matches.get(first + r) else {
                if r == 0 {
                    frame.put(row, left + 2, "Nothing matches.", dim, panel);
                }
                continue;
            };
            let item = &p.items[*index];
            let selected = first + r == p.selected;
            let (rfg, rbg) = if selected { (bg, accent) } else { (fg, panel) };
            if selected {
                frame.put(row, left + 1, &" ".repeat(inner), rfg, rbg);
            }
            // Right side: hint and kind.
            let right = format!("{}  {:>7}", item.hint, item.kind);
            let right_w = right.chars().count();
            let label_room = inner.saturating_sub(right_w + 4);
            let mut col = left + 2;
            for (ci, ch) in item.label.chars().enumerate().take(label_room) {
                let hit = positions.contains(&ci);
                let cfg = match (selected, hit) {
                    (true, _) => rfg,
                    (false, true) => accent,
                    (false, false) => fg,
                };
                col = frame.put(row, col, &ch.to_string(), cfg, rbg);
            }
            if item.label.chars().count() > label_room {
                frame.put(row, col.saturating_sub(1), "…", rfg, rbg);
            }
            frame.put(
                row,
                left + width - 2 - right_w,
                &right,
                if selected { rfg } else { dim },
                rbg,
            );
        }
        let foot = top + 3 + list_rows;
        hline(frame, foot, '├', '─', '┤');
        blank(frame, foot + 1);
        frame.put(
            foot + 1,
            left + 2,
            "↑↓ select · Enter run · Ctrl+Enter run a history command · Esc close",
            dim,
            panel,
        );
        hline(frame, foot + 2, '╰', '─', '╯');
        frame.cursor = None;
    }
}
