// src/app/input.rs
//
// Keyboard, mouse and context-menu handling for `App`.

use super::*;

impl App {
    // ------------------------------------------------------------------
    // Keyboard
    // ------------------------------------------------------------------

    pub(super) fn on_key(&mut self, event: KeyEvent) {
        let state = match (event.state, event.repeat) {
            (ElementState::Released, _) => KeyState::Release,
            (ElementState::Pressed, true) => KeyState::Repeat,
            (ElementState::Pressed, false) => KeyState::Press,
        };

        if self.danger_confirm_open() {
            self.danger_confirm_key(&event);
            return;
        }

        // An agent's consent prompt takes every key until it's answered.
        if self.consent_pending() {
            self.consent_key(&event);
            return;
        }

        if self.command_palette_open() {
            self.command_palette_key(&event);
            return;
        }

        if self.rewind_open() {
            self.rewind_key(&event);
            return;
        }

        if self.ai_open() {
            self.ai_key(&event);
            return;
        }

        if self.theme_menu.is_open {
            if state != KeyState::Release {
                self.handle_theme_menu_key(&event.logical_key);
                self.request_redraw();
            }
            return;
        }

        if self.history_ui.is_some() {
            self.history_key(&event);
            return;
        }
        if self.find.is_some() && self.find_key(&event) {
            return;
        }

        if self.menu.is_some() && state != KeyState::Release && self.on_menu_key(&event.logical_key)
        {
            self.request_redraw();
            return;
        }

        if state == KeyState::Release && self.suppressed.remove(&event.physical_key) {
            return;
        }

        let base = event.key_without_modifiers();
        let key = &event.logical_key;
        if state != KeyState::Release && self.leader_pending {
            // Waiting for the key after the leader. Modifiers alone (the
            // Shift needed for `%`) don't count.
            if is_modifier(key) {
                return;
            }
            self.leader_pending = false;
            self.request_redraw();
            if !self.bindings.is_leader(key, &base, self.mods) {
                self.suppressed.insert(event.physical_key);
                if let Some(action) = self.bindings.lookup_leader(key, &base, self.mods) {
                    self.perform(action);
                }
                return;
            }
            // Leader twice: send the leader key itself, as tmux does.
        } else if state == KeyState::Press && self.bindings.is_leader(key, &base, self.mods) {
            self.leader_pending = true;
            self.suppressed.insert(event.physical_key);
            self.request_redraw();
            return;
        } else if state != KeyState::Release {
            if state == KeyState::Press {
                if let Some(i) = self.lua_binding(&event.logical_key, &base, self.mods) {
                    self.suppressed.insert(event.physical_key);
                    self.lua_run_binding(i);
                    return;
                }
            }
            if let Some(action) = self.bindings.lookup(&event.logical_key, &base, self.mods) {
                self.suppressed.insert(event.physical_key);
                self.perform(action);
                return;
            }
        }

        let Some(pane) = self.focused_pane() else {
            return;
        };
        let mode = *pane.session.term.lock().mode();
        let input = KeyInput {
            key: &event.logical_key,
            base: &base,
            text: event.text.as_deref(),
            location: event.location,
            state,
        };
        let Some(bytes) = keyboard::encode(&input, self.mods, KeyMode::from_term(mode)) else {
            return;
        };
        let targets = self.input_targets();
        if state == KeyState::Press && self.danger_hold(&targets, &event.logical_key, &bytes) {
            return;
        }
        for id in targets {
            if let Some(pane) = self.pane(id) {
                if state != KeyState::Release {
                    pane.session.term.lock().scroll_display(Scroll::Bottom);
                }
                pane.session.write(bytes.clone());
            }
        }
        if state != KeyState::Release {
            self.blink_visible = true;
            self.blink_last = Instant::now();
            if self.config.mouse.hide_while_typing && !self.mouse.hidden {
                if let Some(gpu) = &self.gpu {
                    gpu.window.set_cursor_visible(false);
                }
                self.mouse.hidden = true;
            }
        }
        self.request_redraw();
    }

    pub(super) fn perform(&mut self, action: Action) {
        match action {
            Action::Copy => self.copy_selection(ClipKind::Clipboard),
            Action::Paste => self.paste_from(ClipKind::Clipboard),
            Action::PastePrimary => self.paste_from(ClipKind::Primary),
            Action::SelectAll => {
                if let Some(pane) = self.focused_pane() {
                    let mut term = pane.session.term.lock();
                    let top = Point::new(term.topmost_line(), Column(0));
                    let bottom = Point::new(term.bottommost_line(), term.last_column());
                    let mut sel = Selection::new(SelectionType::Simple, top, Side::Left);
                    sel.update(bottom, Side::Right);
                    term.selection = Some(sel);
                }
            }
            Action::ScrollPageUp => self.scroll(Scroll::PageUp),
            Action::ScrollPageDown => self.scroll(Scroll::PageDown),
            Action::ScrollLineUp => self.scroll(Scroll::Delta(1)),
            Action::ScrollLineDown => self.scroll(Scroll::Delta(-1)),
            Action::ScrollToTop => self.scroll(Scroll::Top),
            Action::ScrollToBottom => self.scroll(Scroll::Bottom),
            Action::PreviousPrompt => self.jump_prompt(false),
            Action::NextPrompt => self.jump_prompt(true),
            Action::ClearScrollback => {
                if let Some(pane) = self.focused_pane() {
                    let mut term = pane.session.term.lock();
                    term.selection = None;
                    term.grid_mut().clear_history();
                }
            }
            Action::FontIncrease => self.zoom((self.font_size + 1.0).min(72.0)),
            Action::FontDecrease => self.zoom((self.font_size - 1.0).max(4.0)),
            Action::FontReset => self.zoom(self.config.font.size),
            Action::ThemeMenu => self.open_theme_menu(),
            Action::ReloadConfig => self.reload_config(true),
            Action::Split(dir) => self.split(dir),
            Action::ClosePane => self.close_pane(self.focused),
            Action::Focus(dir) => self.focus_direction(dir),
            Action::FocusNext => self.focus_cycle(1),
            Action::FocusPrevious => self.focus_cycle(-1),
            Action::Resize(dir) => self.resize_focused(dir),
            Action::EqualizeSplits => self.equalize(),
            Action::ToggleZoom => self.toggle_zoom(),
            Action::ToggleBroadcast => self.toggle_broadcast(),
            Action::NewTab => self.new_tab(),
            Action::CloseTab => self.close_tab(),
            Action::NextTab => self.cycle_tab(1),
            Action::PreviousTab => self.cycle_tab(-1),
            Action::GotoTab(n) => self.activate_tab(n as usize - 1),
            Action::LastTab => self.activate_tab(self.tabs.len().saturating_sub(1)),
            Action::MoveTabLeft => self.move_tab(-1),
            Action::MoveTabRight => self.move_tab(1),
            Action::NewWindow => self.new_window(),
            Action::CopyLastOutput => self.copy_last_output(),
            Action::FindInScrollback => self.toggle_find(),
            Action::HistorySearch => self.toggle_history_ui(),
            Action::ShowLastOutput => self.show_last_output(),
            Action::AskAi => self.ai_ask(),
            Action::ExplainError => self.ai_explain_last(),
            Action::ToggleDanger => self.toggle_danger(),
            Action::Rewind => self.toggle_rewind(),
            Action::CommandPalette => self.toggle_command_palette(),
        }
        self.request_redraw();
    }

    pub(super) fn zoom(&mut self, size: f32) {
        if (size - self.font_size).abs() > f32::EPSILON {
            self.font_size = size;
            self.apply_font();
        }
    }

    pub(super) fn scroll(&self, scroll: Scroll) {
        if let Some(pane) = self.focused_pane() {
            pane.session.term.lock().scroll_display(scroll);
        }
    }

    /// Scrolls so the previous/next shell prompt sits at the top of the
    /// screen. Needs shell integration (`cyberterm +shell-integration`).
    pub(super) fn jump_prompt(&self, forward: bool) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let mut term = pane.session.term.lock();
        let offset = term.grid().display_offset() as i32;
        let top = -offset;
        let prompts = shell::prompt_lines(&term);
        let target = if forward {
            prompts.iter().find(|&&l| l > top).copied()
        } else {
            prompts.iter().rev().find(|&&l| l < top).copied()
        };
        let new_offset = match target {
            Some(line) => (-line).max(0),
            None if forward => 0,
            None => return,
        };
        term.scroll_display(Scroll::Delta(new_offset - offset));
    }

    pub(super) fn copy_selection(&mut self, kind: ClipKind) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let text = pane.session.term.lock().selection_to_string();
        if let (Some(text), Some(clipboard)) = (text, &mut self.clipboard) {
            if !text.is_empty() {
                clipboard.set(kind, &text, self.config.clipboard.clean_box_drawing);
            }
        }
    }

    pub(super) fn paste_from(&mut self, kind: ClipKind) {
        let Some(text) = self.clipboard.as_mut().and_then(|c| c.get(kind)) else {
            return;
        };
        self.paste(&text);
    }

    pub(super) fn paste(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        for id in self.input_targets() {
            let Some(pane) = self.pane(id) else { continue };
            let bracketed = {
                let mut term = pane.session.term.lock();
                term.scroll_display(Scroll::Bottom);
                term.mode().contains(TermMode::BRACKETED_PASTE)
            };
            pane.session.write(paste::paste_bytes(text, bracketed));
        }
    }

    // ------------------------------------------------------------------
    // Mouse
    // ------------------------------------------------------------------

    /// The focused pane's cell under the pointer, clamped to the grid, and
    /// which half of the cell the pointer is in.
    pub(super) fn cell_at_pointer(&self) -> Option<(usize, usize, Side, bool)> {
        let gpu = self.gpu.as_ref()?;
        let pane = self.focused_pane()?;
        let (cw, ch) = gpu.renderer.cell_size();
        let size = pane.session.size();
        let x = self.mouse.pos.x as f32 - pane.rect.x;
        let y = self.mouse.pos.y as f32 - pane.rect.y;
        let inside = x >= 0.0 && y >= 0.0 && x < size.cols as f32 * cw && y < size.rows as f32 * ch;
        let col = ((x / cw).floor().max(0.0) as usize).min(size.cols - 1);
        let row = ((y / ch).floor().max(0.0) as usize).min(size.rows - 1);
        let side = if (x / cw).fract() < 0.5 && x >= 0.0 {
            Side::Left
        } else {
            Side::Right
        };
        Some((row, col, side, inside))
    }

    pub(super) fn reporting_mouse(&self) -> Option<mouse::MouseMode> {
        let pane = self.focused_pane()?;
        let mode = mouse::MouseMode::from_term(*pane.session.term.lock().mode());
        (mode.active() && !self.mods.shift_key()).then_some(mode)
    }

    pub(super) fn report_mouse(
        &mut self,
        button: mouse::Button,
        action: mouse::Action,
        row: usize,
        col: usize,
    ) {
        let Some(mode) = self.reporting_mouse() else {
            return;
        };
        if let Some(bytes) = mouse::encode(button, action, self.mods, col, row, mode) {
            if let Some(pane) = self.focused_pane() {
                pane.session.write(bytes);
            }
        }
        self.mouse.last_report_cell = Some((row, col));
    }

    pub(super) fn on_cursor_moved(&mut self, pos: PhysicalPosition<f64>) {
        self.mouse.pos = pos;
        if self.mouse.hidden {
            if let Some(gpu) = &self.gpu {
                gpu.window.set_cursor_visible(true);
            }
            self.mouse.hidden = false;
        }
        let (x, y) = (pos.x as f32, pos.y as f32);
        if let Some(divider) = self.divider_drag.clone() {
            self.drag_divider(&divider, x, y);
            return;
        }
        if !self.mouse.selecting {
            let over = self.divider_at(x, y).map(|d| d.axis);
            if over != self.mouse.over_divider {
                self.mouse.over_divider = over;
                if let Some(gpu) = &self.gpu {
                    gpu.window.set_cursor(match over {
                        Some(crate::layout::Axis::Horizontal) => CursorIcon::ColResize,
                        Some(crate::layout::Axis::Vertical) => CursorIcon::RowResize,
                        None => CursorIcon::Text,
                    });
                }
            }
            if over.is_some() {
                return;
            }
        }
        let Some((row, col, side, _)) = self.cell_at_pointer() else {
            return;
        };

        if let (Some(menu), Some(layout)) = (&self.menu, self.menu_layout()) {
            let hover = context_menu::item_at(&layout, menu.items.len(), row, col)
                .filter(|&i| menu.items[i].0.enabled);
            if hover != menu.hover {
                if let Some(menu) = &mut self.menu {
                    menu.hover = hover;
                }
                self.request_redraw();
            }
            return;
        }

        if let Some(mode) = self.reporting_mouse() {
            let held = self.mouse.reported_button;
            if mode.reports_motion(held.is_some())
                && self.mouse.last_report_cell != Some((row, col))
            {
                self.report_mouse(
                    held.unwrap_or(mouse::Button::None),
                    mouse::Action::Motion,
                    row,
                    col,
                );
            }
            return;
        }

        if self.mouse.selecting {
            self.extend_selection(row, col, side);
        }
        self.update_hover(row, col);
    }

    pub(super) fn extend_selection(&mut self, row: usize, col: usize, side: Side) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let mut term = pane.session.term.lock();
        // Dragging past the top or bottom edge scrolls the scrollback.
        let y = self.mouse.pos.y as f32;
        if y < pane.rect.y {
            term.scroll_display(Scroll::Delta(1));
        } else if y > pane.rect.y + pane.rect.h {
            term.scroll_display(Scroll::Delta(-1));
        }
        let point = frame::viewport_to_point(row, col, term.grid().display_offset());
        let mut anchor_used = false;
        if let Some((anchor, anchor_side, ty)) = self.mouse.anchor {
            if anchor == point && anchor_side == side {
                return;
            }
            term.selection = Some(Selection::new(ty, anchor, anchor_side));
            anchor_used = true;
        }
        if let Some(sel) = &mut term.selection {
            sel.update(point, side);
        }
        drop(term);
        if anchor_used {
            self.mouse.anchor = None;
        }
        self.request_redraw();
    }

    pub(super) fn update_hover(&mut self, row: usize, col: usize) {
        let hover = self.link_at(row, col);
        if hover != self.mouse.hover {
            if let Some(gpu) = &self.gpu {
                gpu.window.set_cursor(if hover.is_some() {
                    CursorIcon::Pointer
                } else {
                    CursorIcon::Text
                });
            }
            self.mouse.hover = hover;
            self.request_redraw();
        }
    }

    /// An OSC 8 hyperlink or plain-text URL at a viewport cell.
    pub(super) fn link_at(&self, row: usize, col: usize) -> Option<HoverLink> {
        let pane = self.focused_pane()?;
        let term = pane.session.term.lock();
        let offset = term.grid().display_offset();
        let point = frame::viewport_to_point(row, col, offset);
        let line = &term.grid()[point.line];
        let cols = term.columns();

        if let Some(link) = line[Column(col)].hyperlink() {
            if !shell::is_mark(link.uri()) {
                let same = |c: usize| line[Column(c)].hyperlink().is_some_and(|l| l == link);
                let mut start = col;
                while start > 0 && same(start - 1) {
                    start -= 1;
                }
                let mut end = col + 1;
                while end < cols && same(end) {
                    end += 1;
                }
                return Some(HoverLink {
                    row,
                    cols: start..end,
                    uri: link.uri().to_string(),
                    file: None,
                });
            }
        }

        let mut text = String::new();
        let mut char_cols = Vec::new();
        for c in 0..cols {
            let cell = &line[Column(c)];
            if cell
                .flags
                .intersects(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
            {
                continue;
            }
            text.push(cell.c);
            char_cols.push(c);
        }
        let index = char_cols.iter().position(|&c| c >= col)?;
        let span = |range: std::ops::Range<usize>| {
            char_cols[range.start]..char_cols.get(range.end).copied().unwrap_or(cols)
        };
        if let Some((range, uri)) = links::url_at(&text, index) {
            return Some(HoverLink {
                row,
                cols: span(range),
                uri,
                file: None,
            });
        }
        // `path:line` references, resolved against the directory the
        // command ran in, and only when the file exists.
        let (range, found) = links::file_ref_at(&text, index)?;
        let base = crate::blocks::block_at(&*term, point.line.0)
            .and_then(|span| {
                crate::blocks::meta(&pane.session.shell.lock(), span.mark)
                    .and_then(|m| m.cwd.clone())
            })
            .or_else(|| pane.session.cwd());
        drop(term);
        let path = resolve_path(&found.path, base.as_deref())?;
        Some(HoverLink {
            row,
            cols: span(range),
            uri: String::new(),
            file: Some(FileTarget {
                path,
                line: found.line,
                col: found.col,
            }),
        })
    }

    pub(super) fn on_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        let (x, y) = (self.mouse.pos.x as f32, self.mouse.pos.y as f32);
        if self.menu.is_none() {
            if state == ElementState::Pressed {
                if let Some(index) = self.tab_at(x, y) {
                    match button {
                        MouseButton::Left => self.activate_tab(index),
                        MouseButton::Middle => {
                            self.activate_tab(index);
                            self.close_tab();
                        }
                        _ => {}
                    }
                    return;
                }
                if button == MouseButton::Left {
                    if let Some(divider) = self.divider_at(x, y) {
                        self.divider_drag = Some(divider);
                        return;
                    }
                }
                // Clicking a pane focuses it, then the click acts there.
                if let Some(id) = self.pane_at(x, y).filter(|id| *id != self.focused) {
                    self.focus_pane(id);
                }
            } else if button == MouseButton::Left && self.divider_drag.take().is_some() {
                return;
            }
        }

        let Some((row, col, side, inside)) = self.cell_at_pointer() else {
            return;
        };
        let pressed = state == ElementState::Pressed;

        if self.menu.is_some() {
            if pressed {
                self.on_menu_click(button, row, col);
                self.request_redraw();
            }
            return;
        }

        if pressed && button == MouseButton::Left {
            if let Some(port) = self.port_chip_at(row, col) {
                self.open_port(port);
                return;
            }
        }

        let report_button = match button {
            MouseButton::Left => mouse::Button::Left,
            MouseButton::Middle => mouse::Button::Middle,
            MouseButton::Right => mouse::Button::Right,
            _ => return,
        };

        if self.reporting_mouse().is_some() {
            if pressed {
                self.mouse.reported_button = Some(report_button);
                self.report_mouse(report_button, mouse::Action::Press, row, col);
            } else {
                self.mouse.reported_button = None;
                self.report_mouse(report_button, mouse::Action::Release, row, col);
            }
            return;
        }

        match (button, pressed) {
            (MouseButton::Left, true) => {
                if self.mods.control_key() {
                    if let Some(link) = self.mouse.hover.clone() {
                        match link.file {
                            Some(file) => self.open_file(&file),
                            None => open_link(&link.uri),
                        }
                        return;
                    }
                }
                if !inside {
                    return;
                }
                if self.mods.shift_key() && self.extend_existing_selection(row, col, side) {
                    self.mouse.selecting = true;
                    self.mouse.anchor = None;
                    self.request_redraw();
                    return;
                }
                let now = Instant::now();
                self.mouse.clicks = match self.mouse.last_click {
                    Some((at, r, c)) if now - at < DOUBLE_CLICK && (r, c) == (row, col) => {
                        (self.mouse.clicks % 3) + 1
                    }
                    _ => 1,
                };
                self.mouse.last_click = Some((now, row, col));
                let Some(pane) = self.focused_pane() else {
                    return;
                };
                let mut term = pane.session.term.lock();
                let point = frame::viewport_to_point(row, col, term.grid().display_offset());
                let ty = match self.mouse.clicks {
                    1 if self.mods.alt_key() => SelectionType::Block,
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                let anchor = if self.mouse.clicks == 1 {
                    term.selection = None;
                    Some((point, side, ty))
                } else {
                    let mut sel = Selection::new(ty, point, side);
                    sel.update(point, side);
                    term.selection = Some(sel);
                    None
                };
                drop(term);
                self.mouse.anchor = anchor;
                self.mouse.selecting = true;
            }
            (MouseButton::Left, false) => {
                let was_selecting = self.mouse.selecting && self.mouse.anchor.is_none();
                self.mouse.selecting = false;
                self.mouse.anchor = None;
                if was_selecting {
                    self.copy_on_select();
                }
            }
            (MouseButton::Middle, true) => self.paste_from(ClipKind::Primary),
            (MouseButton::Right, true) => self.open_menu(row, col),
            _ => {}
        }
        self.request_redraw();
    }

    pub(super) fn on_wheel(&mut self, delta: MouseScrollDelta) {
        if self.menu.take().is_some() {
            self.request_redraw();
            return;
        }
        let cell_height = self
            .gpu
            .as_ref()
            .map(|g| g.renderer.cell_size().1 as f64)
            .unwrap_or(16.0);
        self.mouse.scroll_accum += match delta {
            MouseScrollDelta::LineDelta(_, y) => {
                y as f64 * self.config.scrollback.multiplier as f64
            }
            MouseScrollDelta::PixelDelta(p) => p.y / cell_height,
        };
        let lines = self.mouse.scroll_accum.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.mouse.scroll_accum -= lines as f64;

        let Some((row, col, _, _)) = self.cell_at_pointer() else {
            return;
        };
        if self.reporting_mouse().is_some() {
            let button = if lines > 0 {
                mouse::Button::WheelUp
            } else {
                mouse::Button::WheelDown
            };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(button, mouse::Action::Press, row, col);
            }
            return;
        }

        let Some(pane) = self.focused_pane() else {
            return;
        };
        let mut term = pane.session.term.lock();
        let mode = *term.mode();
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !self.mods.shift_key()
        {
            // Full-screen programs without mouse support (less, man) get
            // arrow keys instead, as in xterm's alternateScroll.
            drop(term);
            let arrow: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let bytes: Vec<u8> = arrow.repeat(lines.unsigned_abs() as usize);
            pane.session.write(bytes);
        } else {
            term.scroll_display(Scroll::Delta(lines));
            drop(term);
            self.request_redraw();
        }
    }

    /// Copies a just-finished mouse selection where `copy_on_select` says.
    pub(super) fn copy_on_select(&mut self) {
        match self.config.mouse.copy_on_select {
            CopyOnSelect::Off => {}
            CopyOnSelect::Primary => self.copy_selection(ClipKind::Primary),
            CopyOnSelect::Clipboard => self.copy_selection(ClipKind::Clipboard),
            CopyOnSelect::Both => {
                self.copy_selection(ClipKind::Primary);
                self.copy_selection(ClipKind::Clipboard);
            }
        }
    }

    /// Shift+click: move the end of the current selection to the pointer.
    pub(super) fn extend_existing_selection(&mut self, row: usize, col: usize, side: Side) -> bool {
        let Some(pane) = self.focused_pane() else {
            return false;
        };
        let mut term = pane.session.term.lock();
        let point = frame::viewport_to_point(row, col, term.grid().display_offset());
        match &mut term.selection {
            Some(sel) => {
                sel.update(point, side);
                true
            }
            None => false,
        }
    }

    // ------------------------------------------------------------------
    // Context menu
    // ------------------------------------------------------------------

    pub(super) fn open_menu(&mut self, row: usize, col: usize) {
        let has_selection = self
            .focused_pane()
            .and_then(|p| p.session.term.lock().selection_to_string())
            .is_some_and(|t| !t.is_empty());
        let has_clipboard = self
            .clipboard
            .as_mut()
            .and_then(|c| c.get(ClipKind::Clipboard))
            .is_some_and(|t| !t.is_empty());

        let b_hint_explain = self.bindings.hint(Action::ExplainError);
        let focused_ports = self
            .focused_pane()
            .map(|p| p.ports.clone())
            .unwrap_or_default();
        let in_ssh = self.config.splits.follow_ssh && self.ssh_follow_command().is_some();
        let agent_pane = Some(self.focused).filter(|&p| self.pane_has_agent(p));
        let mut items = Vec::new();
        let mut add = |label: &str, hint: String, enabled: bool, action: MenuAction| {
            items.push((
                context_menu::Item {
                    label: label.to_string(),
                    hint,
                    enabled,
                },
                action,
            ));
        };
        if let Some(file) = self.mouse.hover.clone().and_then(|l| l.file) {
            let name = file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            add(
                &format!("Open {name}:{}", file.line),
                "Ctrl+Click".into(),
                true,
                MenuAction::OpenFile(file.clone()),
            );
            add(
                "Copy Path",
                String::new(),
                true,
                MenuAction::CopyText(file.path.to_string_lossy().into_owned()),
            );
        } else if let Some(link) = self.mouse.hover.clone() {
            add(
                "Open Link",
                "Ctrl+Click".into(),
                links::openable(&link.uri),
                MenuAction::OpenLink(link.uri.clone()),
            );
            add(
                "Copy Link",
                String::new(),
                true,
                MenuAction::CopyText(link.uri),
            );
        }
        if let Some((block, meta)) = self.block_under(row) {
            let command = meta.command.clone().unwrap_or_default();
            let short: String = command.chars().take(24).collect();
            let has_previous = self.previous_output(block, &meta).is_some();
            add(
                &format!("Copy Output of `{short}`"),
                String::new(),
                true,
                MenuAction::CopyOutput(block),
            );
            add(
                "Copy Command",
                String::new(),
                true,
                MenuAction::CopyText(command.clone()),
            );
            add(
                "Rerun",
                String::new(),
                !meta.running(),
                MenuAction::Rerun(block, command.clone()),
            );
            add(
                "Rerun in New Pane",
                String::new(),
                true,
                MenuAction::RunInSplit(command.clone(), meta.cwd.clone()),
            );
            add(
                "Watch in New Pane (every 2s)",
                String::new(),
                true,
                MenuAction::Watch(command.clone(), meta.cwd.clone()),
            );
            if self.block_output_is_json(block) {
                add(
                    "View as JSON",
                    String::new(),
                    true,
                    MenuAction::ViewJson(block),
                );
            }
            if meta.exit.is_some_and(|e| e != 0) {
                add(
                    "Explain with AI",
                    b_hint_explain.clone(),
                    true,
                    MenuAction::Explain(block, meta.clone()),
                );
            }
            add(
                "Diff with Previous Run",
                String::new(),
                has_previous && !meta.running(),
                MenuAction::Diff(block, meta.clone()),
            );
        }
        for port in focused_ports.iter().take(4) {
            add(
                &format!("Open localhost:{port}"),
                String::new(),
                true,
                MenuAction::OpenPort(*port),
            );
        }
        add(
            "Command Palette…",
            self.bindings.hint(Action::CommandPalette),
            true,
            MenuAction::CommandPalette,
        );
        add(
            "Rewind…",
            self.bindings.hint(Action::Rewind),
            true,
            MenuAction::Rewind,
        );
        if in_ssh {
            add(
                "Split Right (Local Shell)",
                String::new(),
                true,
                MenuAction::SplitLocal(crate::layout::Direction::Right),
            );
            add(
                "Split Down (Local Shell)",
                String::new(),
                true,
                MenuAction::SplitLocal(crate::layout::Direction::Down),
            );
        }
        if let Some(pane) = agent_pane {
            add(
                "Revoke Agent Access",
                String::new(),
                true,
                MenuAction::RevokeAgents(pane),
            );
        }
        let b = &self.bindings;
        add(
            "Copy",
            b.hint(Action::Copy),
            has_selection,
            MenuAction::Do(Action::Copy),
        );
        add(
            "Paste",
            b.hint(Action::Paste),
            has_clipboard,
            MenuAction::Do(Action::Paste),
        );
        add(
            "Select All",
            b.hint(Action::SelectAll),
            true,
            MenuAction::Do(Action::SelectAll),
        );
        add(
            "Clear Scrollback",
            b.hint(Action::ClearScrollback),
            true,
            MenuAction::Do(Action::ClearScrollback),
        );
        add(
            "Themes…",
            b.hint(Action::ThemeMenu),
            true,
            MenuAction::Do(Action::ThemeMenu),
        );
        add(
            "Reload Config",
            b.hint(Action::ReloadConfig),
            true,
            MenuAction::Do(Action::ReloadConfig),
        );

        self.menu = Some(ContextMenu {
            row,
            col,
            items,
            hover: None,
        });
        self.request_redraw();
    }

    pub(super) fn menu_layout(&self) -> Option<context_menu::Layout> {
        let menu = self.menu.as_ref()?;
        let size = self.focused_pane()?.session.size();
        let items: Vec<_> = menu.items.iter().map(|(item, _)| item.clone()).collect();
        Some(context_menu::layout(
            &items, menu.row, menu.col, size.rows, size.cols,
        ))
    }

    pub(super) fn on_menu_click(&mut self, button: MouseButton, row: usize, col: usize) {
        let Some(layout) = self.menu_layout() else {
            return;
        };
        let count = self.menu.as_ref().map_or(0, |m| m.items.len());
        match (button, context_menu::item_at(&layout, count, row, col)) {
            (MouseButton::Left, Some(index)) => self.activate_menu_item(index),
            (MouseButton::Left, None) if context_menu::contains(&layout, row, col) => {}
            (MouseButton::Right, _) => self.open_menu(row, col),
            _ => self.menu = None,
        }
    }

    pub(super) fn activate_menu_item(&mut self, index: usize) {
        let Some(menu) = self.menu.take() else { return };
        let Some((item, action)) = menu.items.get(index).cloned() else {
            return;
        };
        if !item.enabled {
            self.menu = Some(menu);
            return;
        }
        match action {
            MenuAction::Do(action) => self.perform(action),
            MenuAction::OpenLink(uri) => open_link(&uri),
            MenuAction::CopyText(text) => self.copy_text(&text),
            MenuAction::OpenFile(file) => self.open_file(&file),
            MenuAction::CopyOutput(block) => self.copy_block_output(block),
            MenuAction::Rerun(block, command) => self.rerun(block, &command),
            MenuAction::RunInSplit(command, cwd) => {
                self.run_in_split(crate::layout::Direction::Right, &command, cwd)
            }
            MenuAction::Watch(command, cwd) => self.watch(&command, cwd),
            MenuAction::Diff(block, meta) => self.diff_with_previous(block, &meta),
            MenuAction::ViewJson(block) => self.view_json(block),
            MenuAction::RevokeAgents(pane) => self.revoke_agents(pane),
            MenuAction::SplitLocal(dir) => self.split_local(dir),
            MenuAction::OpenPort(port) => self.open_port(port),
            MenuAction::Rewind => self.toggle_rewind(),
            MenuAction::CommandPalette => self.toggle_command_palette(),
            MenuAction::Explain(block, meta) => self.ai_explain(block, &meta),
        }
    }

    /// Keyboard navigation while the menu is open. Returns false for keys
    /// that should close the menu and then be handled normally.
    pub(super) fn on_menu_key(&mut self, key: &Key) -> bool {
        let Some(menu) = &mut self.menu else {
            return false;
        };
        let enabled: Vec<usize> = (0..menu.items.len())
            .filter(|&i| menu.items[i].0.enabled)
            .collect();
        let position = menu
            .hover
            .and_then(|h| enabled.iter().position(|&i| i == h));
        match key {
            Key::Named(NamedKey::Escape) => self.menu = None,
            Key::Named(NamedKey::ArrowDown) if !enabled.is_empty() => {
                menu.hover = Some(enabled[position.map_or(0, |p| (p + 1) % enabled.len())]);
            }
            Key::Named(NamedKey::ArrowUp) if !enabled.is_empty() => {
                let last = enabled.len() - 1;
                menu.hover = Some(enabled[position.map_or(last, |p| (p + last) % enabled.len())]);
            }
            Key::Named(NamedKey::Enter) => match menu.hover {
                Some(index) => self.activate_menu_item(index),
                None => self.menu = None,
            },
            _ => {
                self.menu = None;
                return false;
            }
        }
        true
    }
}

fn is_modifier(key: &Key) -> bool {
    matches!(
        key,
        Key::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Super
                | NamedKey::Meta
                | NamedKey::Hyper
        )
    )
}

/// An existing file for a reference: absolute, `~/`, or relative to `base`.
fn resolve_path(path: &str, base: Option<&std::path::Path>) -> Option<PathBuf> {
    let p = match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => PathBuf::from(path),
    };
    let full = if p.is_absolute() { p } else { base?.join(p) };
    full.is_file().then_some(full)
}
