// src/app.rs
//
// The winit application: owns the window, the GPU surface and renderer,
// and the panes (one `Session` each). Translates window events into PTY
// input (keys, mouse, paste, focus), terminal events into window effects
// (title, bell, clipboard), and draws every pane each frame.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use alacritty_terminal::event::{Event as TermEvent, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{ClipboardType, Config as TermConfig, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape as TermCursorShape, CursorStyle, Rgb};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
#[cfg(all(unix, not(target_os = "macos")))]
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{CursorIcon, UserAttentionType, Window, WindowId};

use crate::clipboard::{ClipboardManager, Kind as ClipKind};
use crate::config::{self, CopyOnSelect, CursorShapeConfig, CyberConfig, Osc52Mode};
use crate::frame::{self, CursorOptions, Frame, FrameOptions, Palette};
use crate::input::bindings::{Action, Bindings};
use crate::input::keyboard::{self, KeyInput, KeyMode, KeyState};
use crate::input::{links, mouse, paste};
use crate::renderer::{self, FontSpec, PaneView, Rect, TermRenderer};
use crate::session::{GridSize, PaneId, Session, SpawnOptions, UserEvent};
use crate::shell;
use crate::theme::{Theme, ThemeRegistry};
use crate::ui;
use crate::ui::context_menu;

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const BELL_FLASH: Duration = Duration::from_millis(150);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub struct ThemeMenuState {
    pub is_open: bool,
    pub is_creating_mode: bool,
    pub theme_name_input: String,
    pub registry: ThemeRegistry,
}

/// Everything that only exists once the window/GPU surface are up --
/// created in `resumed()`, not before, since wgpu needs a real surface.
struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: TermRenderer,
    premultiply_bg: bool,
    // Rust drops struct fields in declaration order -- `window` must be
    // LAST so it outlives `surface`. wgpu's Surface holds a reference tied
    // to the live window; dropping it after the window is gone is a real,
    // documented crash (glyphon's own hello-world example calls this out),
    // and matched several SIGSEGV coredumps before this ordering was fixed.
    window: Arc<Window>,
}

struct Pane {
    id: PaneId,
    session: Session,
    /// Where the pane's cell grid sits on the surface, in physical pixels.
    rect: Rect,
    bell: Option<Instant>,
}

/// A link under the mouse pointer: viewport row, column range, target.
#[derive(Clone, Debug, PartialEq)]
struct HoverLink {
    row: usize,
    cols: std::ops::Range<usize>,
    uri: String,
}

/// What a context-menu entry does.
#[derive(Clone, Debug)]
enum MenuAction {
    Do(Action),
    OpenLink(String),
    CopyText(String),
}

/// An open right-click menu, anchored at a cell of the focused pane.
struct ContextMenu {
    row: usize,
    col: usize,
    items: Vec<(context_menu::Item, MenuAction)>,
    hover: Option<usize>,
}

#[derive(Default)]
struct MouseState {
    pos: PhysicalPosition<f64>,
    /// Button held while a program has mouse reporting on.
    reported_button: Option<mouse::Button>,
    last_report_cell: Option<(usize, usize)>,
    selecting: bool,
    /// Where a single-click drag starts; the selection only appears once
    /// the pointer actually moves, so a plain click just clears it.
    anchor: Option<(Point, Side, SelectionType)>,
    clicks: u8,
    last_click: Option<(Instant, usize, usize)>,
    hover: Option<HoverLink>,
    scroll_accum: f64,
    hidden: bool,
}

pub struct App {
    proxy: EventLoopProxy<UserEvent>,
    gpu: Option<Gpu>,
    panes: Vec<Pane>,
    focused: PaneId,
    next_pane: PaneId,

    config: CyberConfig,
    config_root: PathBuf,
    config_mtime: Option<SystemTime>,
    themes_dir: PathBuf,
    bindings: Bindings,
    theme_menu: ThemeMenuState,
    palette: Palette,
    /// Current font size in points (config size, changed by zoom keys).
    font_size: f32,

    mods: ModifiersState,
    window_focused: bool,
    mouse: MouseState,
    clipboard: Option<ClipboardManager>,
    blink_visible: bool,
    blink_last: Instant,
    ime_preedit: Option<String>,
    menu: Option<ContextMenu>,
    /// Physical keys consumed by a keybinding, so their release (reported
    /// under the Kitty protocol) doesn't leak to the program either.
    suppressed: HashSet<PhysicalKey>,
    last_poll: Instant,
    shared_theme_revision: u64,
}

impl App {
    pub fn new(
        proxy: EventLoopProxy<UserEvent>,
        config_root: PathBuf,
        themes_dir: PathBuf,
        config: CyberConfig,
        registry: ThemeRegistry,
        initial_theme: Option<Theme>,
        shared_theme_revision: u64,
    ) -> Self {
        let (bindings, errors) = Bindings::new(&config.keybindings);
        report_config_errors(&errors);
        let mut app = Self {
            proxy,
            gpu: None,
            panes: Vec::new(),
            focused: 0,
            next_pane: 0,
            font_size: config.font.size,
            config_mtime: config::config_mtime(&config_root),
            config,
            config_root,
            themes_dir,
            bindings,
            theme_menu: ThemeMenuState {
                is_open: false,
                is_creating_mode: false,
                theme_name_input: String::new(),
                registry,
            },
            palette: Palette::default(),
            mods: ModifiersState::empty(),
            window_focused: true,
            mouse: MouseState::default(),
            clipboard: ClipboardManager::try_new(),
            blink_visible: true,
            blink_last: Instant::now(),
            ime_preedit: None,
            menu: None,
            suppressed: HashSet::new(),
            last_poll: Instant::now(),
            shared_theme_revision,
        };
        if let Some(theme) = initial_theme {
            app.apply_theme(&theme);
        }
        app
    }

    fn apply_theme(&mut self, theme: &Theme) {
        self.palette = Palette {
            ansi: theme.colors,
            fg: hex_str_to_u32(&theme.foreground),
            bg: hex_str_to_u32(&theme.background),
            cursor: if theme.cursor.trim().is_empty() {
                hex_str_to_u32(&theme.foreground)
            } else {
                hex_str_to_u32(&theme.cursor)
            },
        };
        self.request_redraw();
    }

    fn request_redraw(&self) {
        if let Some(gpu) = &self.gpu {
            gpu.window.request_redraw();
        }
    }

    fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn focused_pane(&self) -> Option<&Pane> {
        self.pane(self.focused)
    }

    fn term_config(&self) -> TermConfig {
        let c = &self.config;
        TermConfig {
            scrolling_history: c.scrollback.lines,
            default_cursor_style: CursorStyle {
                shape: match c.cursor.style {
                    CursorShapeConfig::Block => TermCursorShape::Block,
                    CursorShapeConfig::Beam => TermCursorShape::Beam,
                    CursorShapeConfig::Underline => TermCursorShape::Underline,
                },
                blinking: c.cursor.blinking,
            },
            kitty_keyboard: c.keyboard.kitty_protocol,
            osc52: match c.clipboard.osc52 {
                Osc52Mode::Disabled => Osc52::Disabled,
                Osc52Mode::Copy => Osc52::OnlyCopy,
                Osc52Mode::Paste => Osc52::OnlyPaste,
                Osc52Mode::CopyPaste => Osc52::CopyPaste,
            },
            ..TermConfig::default()
        }
    }

    fn font_spec(&self, scale: f64) -> FontSpec {
        let f = &self.config.font;
        FontSpec {
            family: f.family.clone(),
            fallback: f.fallback.clone(),
            // Points at 96 DPI, times the monitor's scale factor.
            size_px: (self.font_size * 96.0 / 72.0 * scale as f32).max(4.0),
            line_height: f.line_height.clamp(0.8, 3.0),
            ligatures: f.ligatures,
        }
    }

    /// The rectangle the (single, for now) pane's grid occupies.
    fn content_rect(&self, gpu: &Gpu) -> Rect {
        let pad = (self.config.window.padding.max(0.0) as f64 * gpu.window.scale_factor()) as f32;
        Rect {
            x: pad,
            y: pad,
            w: (gpu.config.width as f32 - 2.0 * pad).max(1.0),
            h: (gpu.config.height as f32 - 2.0 * pad).max(1.0),
        }
    }

    /// Recomputes every pane's rectangle and grid size after a window
    /// resize, font change or padding change.
    fn relayout(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let rect = self.content_rect(gpu);
        let (cols, rows) = gpu.renderer.grid_size(rect.w, rect.h);
        let (cw, ch) = gpu.renderer.cell_size();
        for pane in &mut self.panes {
            pane.rect = rect;
            pane.session.resize(GridSize { cols, rows }, cw, ch);
        }
        self.request_redraw();
    }

    fn spawn_pane(&mut self) -> std::io::Result<()> {
        let Some(gpu) = &self.gpu else {
            return Ok(());
        };
        let rect = self.content_rect(gpu);
        let (cols, rows) = gpu.renderer.grid_size(rect.w, rect.h);
        let (cw, ch) = gpu.renderer.cell_size();
        let id = self.next_pane;
        self.next_pane += 1;
        let session = Session::spawn(
            id,
            self.proxy.clone(),
            SpawnOptions {
                size: GridSize { cols, rows },
                cell_width: cw,
                cell_height: ch,
                program: self.config.shell.program.clone(),
                term: self.config.shell.term.clone(),
                args: self.config.shell.args.clone(),
                cwd: None,
                term_config: self.term_config(),
            },
        )?;
        session.term.lock().is_focused = self.window_focused;
        self.panes.push(Pane {
            id,
            session,
            rect,
            bell: None,
        });
        self.focused = id;
        Ok(())
    }

    fn apply_font(&mut self) {
        let Some(scale) = self.gpu.as_ref().map(|g| g.window.scale_factor()) else {
            return;
        };
        let spec = self.font_spec(scale);
        if let Some(gpu) = &mut self.gpu {
            gpu.renderer.set_font(spec);
        }
        self.relayout();
    }

    // ------------------------------------------------------------------
    // Config
    // ------------------------------------------------------------------

    fn reload_config(&mut self, force: bool) {
        let mtime = config::config_mtime(&self.config_root);
        if !force && mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        match config::load_config(&self.config_root) {
            Ok(new) => self.apply_config(new),
            Err(e) => eprintln!("cyberterm: config not applied: {e}"),
        }
    }

    fn apply_config(&mut self, new: CyberConfig) {
        let old = std::mem::replace(&mut self.config, new);
        let (bindings, errors) = Bindings::new(&self.config.keybindings);
        report_config_errors(&errors);
        self.bindings = bindings;

        if self.config.theme != old.theme {
            if let Some(theme) = self
                .theme_menu
                .registry
                .themes
                .iter()
                .find(|t| t.name == self.config.theme)
                .cloned()
            {
                self.apply_theme(&theme);
            }
        }
        let term_config = self.term_config();
        for pane in &self.panes {
            pane.session.term.lock().set_options(term_config.clone());
        }
        if self.config.font != old.font {
            self.font_size = self.config.font.size;
            self.apply_font();
        } else if self.config.window.padding != old.window.padding {
            self.relayout();
        }
        self.request_redraw();
    }

    fn poll_shared_theme(&mut self) {
        let Ok(catalog) = cybercore::theme::ThemeCatalog::load() else {
            return;
        };
        let revision = catalog.revision();
        if revision == self.shared_theme_revision {
            return;
        }
        self.shared_theme_revision = revision;
        let registry = &mut self.theme_menu.registry;
        if let Some(active) = registry.append_cybercore_themes() {
            if let Some(index) = registry.themes.iter().position(|t| t.name == active) {
                registry.selected_index = index;
                let theme = registry.themes[index].clone();
                self.apply_theme(&theme);
            }
        }
    }

    // ------------------------------------------------------------------
    // Keyboard
    // ------------------------------------------------------------------

    fn on_key(&mut self, event: KeyEvent) {
        let state = match (event.state, event.repeat) {
            (ElementState::Released, _) => KeyState::Release,
            (ElementState::Pressed, true) => KeyState::Repeat,
            (ElementState::Pressed, false) => KeyState::Press,
        };

        if self.theme_menu.is_open {
            if state != KeyState::Release {
                self.handle_theme_menu_key(&event.logical_key);
                self.request_redraw();
            }
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
        if state != KeyState::Release {
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
        if state != KeyState::Release {
            pane.session.term.lock().scroll_display(Scroll::Bottom);
        }
        pane.session.write(bytes);
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

    fn perform(&mut self, action: Action) {
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
            Action::ThemeMenu => self.theme_menu.is_open = true,
            Action::ReloadConfig => self.reload_config(true),
        }
        self.request_redraw();
    }

    fn zoom(&mut self, size: f32) {
        if (size - self.font_size).abs() > f32::EPSILON {
            self.font_size = size;
            self.apply_font();
        }
    }

    fn scroll(&self, scroll: Scroll) {
        if let Some(pane) = self.focused_pane() {
            pane.session.term.lock().scroll_display(scroll);
        }
    }

    /// Scrolls so the previous/next shell prompt sits at the top of the
    /// screen. Needs shell integration (`cyberterm +shell-integration`).
    fn jump_prompt(&self, forward: bool) {
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

    fn copy_selection(&mut self, kind: ClipKind) {
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

    fn paste_from(&mut self, kind: ClipKind) {
        let Some(text) = self.clipboard.as_mut().and_then(|c| c.get(kind)) else {
            return;
        };
        self.paste(&text);
    }

    fn paste(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let bracketed = {
            let mut term = pane.session.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };
        pane.session.write(paste::paste_bytes(text, bracketed));
    }

    // ------------------------------------------------------------------
    // Mouse
    // ------------------------------------------------------------------

    /// The focused pane's cell under the pointer, clamped to the grid, and
    /// which half of the cell the pointer is in.
    fn cell_at_pointer(&self) -> Option<(usize, usize, Side, bool)> {
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

    fn reporting_mouse(&self) -> Option<mouse::MouseMode> {
        let pane = self.focused_pane()?;
        let mode = mouse::MouseMode::from_term(*pane.session.term.lock().mode());
        (mode.active() && !self.mods.shift_key()).then_some(mode)
    }

    fn report_mouse(
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

    fn on_cursor_moved(&mut self, pos: PhysicalPosition<f64>) {
        self.mouse.pos = pos;
        if self.mouse.hidden {
            if let Some(gpu) = &self.gpu {
                gpu.window.set_cursor_visible(true);
            }
            self.mouse.hidden = false;
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

    fn extend_selection(&mut self, row: usize, col: usize, side: Side) {
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

    fn update_hover(&mut self, row: usize, col: usize) {
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
    fn link_at(&self, row: usize, col: usize) -> Option<HoverLink> {
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
        let (range, uri) = links::url_at(&text, index)?;
        let start = char_cols[range.start];
        let end = char_cols.get(range.end).copied().unwrap_or(cols);
        Some(HoverLink {
            row,
            cols: start..end,
            uri,
        })
    }

    fn on_mouse_input(&mut self, state: ElementState, button: MouseButton) {
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
                    if let Some(link) = &self.mouse.hover {
                        open_link(&link.uri);
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

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
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
    fn copy_on_select(&mut self) {
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
    fn extend_existing_selection(&mut self, row: usize, col: usize, side: Side) -> bool {
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

    fn open_menu(&mut self, row: usize, col: usize) {
        let has_selection = self
            .focused_pane()
            .and_then(|p| p.session.term.lock().selection_to_string())
            .is_some_and(|t| !t.is_empty());
        let has_clipboard = self
            .clipboard
            .as_mut()
            .and_then(|c| c.get(ClipKind::Clipboard))
            .is_some_and(|t| !t.is_empty());

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
        if let Some(link) = self.mouse.hover.clone() {
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

    fn menu_layout(&self) -> Option<context_menu::Layout> {
        let menu = self.menu.as_ref()?;
        let size = self.focused_pane()?.session.size();
        let items: Vec<_> = menu.items.iter().map(|(item, _)| item.clone()).collect();
        Some(context_menu::layout(
            &items, menu.row, menu.col, size.rows, size.cols,
        ))
    }

    fn on_menu_click(&mut self, button: MouseButton, row: usize, col: usize) {
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

    fn activate_menu_item(&mut self, index: usize) {
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
            MenuAction::CopyText(text) => {
                if let Some(clipboard) = &mut self.clipboard {
                    clipboard.set(ClipKind::Clipboard, &text, false);
                }
            }
        }
    }

    /// Keyboard navigation while the menu is open. Returns false for keys
    /// that should close the menu and then be handled normally.
    fn on_menu_key(&mut self, key: &Key) -> bool {
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

    // ------------------------------------------------------------------
    // Terminal events
    // ------------------------------------------------------------------

    fn on_term_event(&mut self, event_loop: &ActiveEventLoop, id: PaneId, event: TermEvent) {
        let Some(index) = self.panes.iter().position(|p| p.id == id) else {
            return;
        };
        match event {
            TermEvent::Wakeup => self.request_redraw(),
            TermEvent::Title(title) => {
                if id == self.focused {
                    if let Some(gpu) = &self.gpu {
                        gpu.window.set_title(&title);
                    }
                }
            }
            TermEvent::ResetTitle => {
                if let Some(gpu) = &self.gpu {
                    gpu.window.set_title("Cyberterm");
                }
            }
            TermEvent::PtyWrite(text) => self.panes[index].session.write(text.into_bytes()),
            TermEvent::ClipboardStore(ty, text) => {
                if let Some(clipboard) = &mut self.clipboard {
                    clipboard.set(clip_kind(ty), &text, false);
                }
            }
            TermEvent::ClipboardLoad(ty, format) => {
                // Only reached when [clipboard] osc52 allows paste.
                let text = self
                    .clipboard
                    .as_mut()
                    .and_then(|c| c.get(clip_kind(ty)))
                    .unwrap_or_default();
                self.panes[index].session.write(format(&text).into_bytes());
            }
            TermEvent::ColorRequest(color_index, format) => {
                let [r, g, b] = frame::query_color(&self.palette, color_index);
                self.panes[index]
                    .session
                    .write(format(Rgb { r, g, b }).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                if let Some(gpu) = &self.gpu {
                    let (cw, ch) = gpu.renderer.cell_size();
                    let size = self.panes[index].session.size();
                    let window_size = WindowSize {
                        num_lines: size.rows as u16,
                        num_cols: size.cols as u16,
                        cell_width: cw as u16,
                        cell_height: ch as u16,
                    };
                    self.panes[index]
                        .session
                        .write(format(window_size).into_bytes());
                }
            }
            TermEvent::Bell => self.ring_bell(index),
            TermEvent::ChildExit(_) | TermEvent::Exit => {
                self.panes.remove(index);
                if self.panes.is_empty() {
                    event_loop.exit();
                } else if id == self.focused {
                    self.focused = self.panes[0].id;
                    self.request_redraw();
                }
            }
            TermEvent::CursorBlinkingChange => {
                self.blink_visible = true;
                self.blink_last = Instant::now();
                self.request_redraw();
            }
            TermEvent::MouseCursorDirty => {}
        }
    }

    fn ring_bell(&mut self, index: usize) {
        let bell = self.config.bell.clone();
        if bell.visual {
            self.panes[index].bell = Some(Instant::now());
            self.request_redraw();
        }
        if bell.urgent && !self.window_focused {
            if let Some(gpu) = &self.gpu {
                gpu.window
                    .request_user_attention(Some(UserAttentionType::Informational));
            }
        }
        if let Some(command) = bell.command.filter(|c| !c.trim().is_empty()) {
            spawn_detached(Command::new("sh").arg("-c").arg(command));
        }
    }

    // ------------------------------------------------------------------
    // Drawing
    // ------------------------------------------------------------------

    fn build_frames(&self) -> Vec<(Rect, Frame, f32)> {
        let now = Instant::now();
        let mut frames = Vec::new();
        for pane in &self.panes {
            let focused = pane.id == self.focused;
            let hover: Vec<(usize, std::ops::Range<usize>)> = self
                .mouse
                .hover
                .iter()
                .filter(|_| focused)
                .map(|h| (h.row, h.cols.clone()))
                .collect();
            let mut frame = if focused && self.theme_menu.is_open {
                let size = pane.session.size();
                let lines: Vec<Vec<(String, [u8; 3])>> = ui::theme_menu::build_lines(
                    &self.theme_menu.registry,
                    self.theme_menu.is_creating_mode,
                    &self.theme_menu.theme_name_input,
                    size.rows,
                )
                .into_iter()
                .map(|line| line.into_iter().map(|s| (s.text, s.color)).collect())
                .collect();
                Frame::from_spans(
                    &lines,
                    size.cols,
                    size.rows,
                    frame::hex_to_rgb(self.palette.bg),
                )
            } else {
                let term = pane.session.term.lock();
                frame::build(
                    &term,
                    &FrameOptions {
                        palette: &self.palette,
                        bold_is_bright: self.config.font.bold_is_bright,
                        cursor: CursorOptions {
                            visible: self.blink_visible,
                            focused: self.window_focused && focused,
                            unfocused_hollow: self.config.cursor.unfocused_hollow,
                        },
                        link_hover: &hover,
                    },
                )
            };
            if focused {
                if let Some(preedit) = &self.ime_preedit {
                    overlay_preedit(&mut frame, preedit);
                }
                if let (Some(menu), Some(layout)) = (&self.menu, self.menu_layout()) {
                    let items: Vec<_> = menu.items.iter().map(|(item, _)| item.clone()).collect();
                    let colors = context_menu::Colors {
                        fg: frame::hex_to_rgb(self.palette.fg),
                        bg: frame::hex_to_rgb(self.palette.bg),
                        dim: frame::hex_to_rgb(self.palette.ansi[8]),
                        accent: frame::hex_to_rgb(self.palette.cursor),
                    };
                    context_menu::draw(&mut frame, &layout, &items, menu.hover, &colors);
                }
            }
            let flash = pane
                .bell
                .map(|at| now.saturating_duration_since(at))
                .filter(|d| *d < BELL_FLASH)
                .map(|d| 1.0 - d.as_secs_f32() / BELL_FLASH.as_secs_f32())
                .unwrap_or(0.0);
            frames.push((pane.rect, frame, flash));
        }
        frames
    }

    fn draw(&mut self) {
        let frames = self.build_frames();
        let Some(gpu) = &mut self.gpu else { return };

        let surface_texture = match gpu.surface.get_current_texture() {
            Ok(texture) => texture,
            Err(wgpu::SurfaceError::Timeout) => return,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                gpu.window.request_redraw();
                return;
            }
            Err(e) => {
                eprintln!("cyberterm: surface error: {e:?}");
                return;
            }
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cyberterm frame encoder"),
            });

        // The clear color is the focused pane's background (which a program
        // may have changed with OSC 11), so padding matches the cells.
        let bg = frames
            .first()
            .map(|(_, f, _)| f.bg)
            .unwrap_or_else(|| frame::hex_to_rgb(self.palette.bg));
        let opacity = self.config.opacity.clamp(0.0, 1.0) as f64;
        let mul = if gpu.premultiply_bg { opacity } else { 1.0 };
        let clear_color = wgpu::Color {
            r: bg[0] as f64 / 255.0 * mul,
            g: bg[1] as f64 / 255.0 * mul,
            b: bg[2] as f64 / 255.0 * mul,
            a: opacity,
        };

        let views: Vec<PaneView<'_>> = frames
            .iter()
            .map(|(rect, frame, flash)| PaneView {
                rect: *rect,
                frame,
                flash: *flash,
            })
            .collect();
        gpu.renderer.render(
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            renderer::FrameTarget {
                view: &view,
                width_px: gpu.config.width,
                height_px: gpu.config.height,
                clear_color,
                background_alpha: opacity as f32,
                premultiply: gpu.premultiply_bg,
            },
            &views,
        );
        gpu.queue.submit(std::iter::once(encoder.finish()));
        gpu.window.pre_present_notify();
        surface_texture.present();

        // Tell the input method where the cursor is, so its candidate
        // window pops up next to the text being composed.
        if let Some((rect, frame, _)) = frames.first() {
            if let Some(cursor) = frame.cursor {
                let (cw, ch) = gpu.renderer.cell_size();
                gpu.window.set_ime_cursor_area(
                    PhysicalPosition::new(
                        rect.x + cursor.col as f32 * cw,
                        rect.y + cursor.row as f32 * ch,
                    ),
                    PhysicalSize::new(cw, ch),
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // Theme menu
    // ------------------------------------------------------------------

    fn handle_theme_menu_key(&mut self, key: &Key) {
        if self.theme_menu.is_creating_mode {
            match key {
                Key::Named(NamedKey::Enter) => {
                    if !self.theme_menu.theme_name_input.trim().is_empty() {
                        if let Ok(new_file) = ThemeRegistry::create_new_theme_template(
                            &self.themes_dir,
                            &self.theme_menu.theme_name_input,
                        ) {
                            let editor =
                                std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
                            let _ = Command::new(editor).arg(new_file).status();
                            self.theme_menu.registry =
                                ThemeRegistry::load_from_dir(&self.themes_dir);
                        }
                    }
                    self.theme_menu.theme_name_input.clear();
                    self.theme_menu.is_creating_mode = false;
                }
                Key::Named(NamedKey::Escape) => {
                    self.theme_menu.is_creating_mode = false;
                    self.theme_menu.theme_name_input.clear();
                }
                Key::Named(NamedKey::Backspace) => {
                    self.theme_menu.theme_name_input.pop();
                }
                Key::Character(c) if self.theme_menu.theme_name_input.len() < 20 => {
                    self.theme_menu.theme_name_input.push_str(c);
                }
                _ => {}
            }
            return;
        }

        let len = self.theme_menu.registry.themes.len();
        match key {
            Key::Named(NamedKey::ArrowDown | NamedKey::ArrowUp) if len > 0 => {
                let registry = &mut self.theme_menu.registry;
                registry.selected_index = if *key == Key::Named(NamedKey::ArrowDown) {
                    (registry.selected_index + 1) % len
                } else {
                    (registry.selected_index + len - 1) % len
                };
                let theme = registry.themes[registry.selected_index].clone();
                self.apply_theme(&theme);
            }
            Key::Named(NamedKey::Enter) => {
                if let Some(theme) = self
                    .theme_menu
                    .registry
                    .themes
                    .get(self.theme_menu.registry.selected_index)
                {
                    self.config.theme = theme.name.clone();
                    let _ = config::save_config(&self.config_root, &self.config);
                    self.config_mtime = config::config_mtime(&self.config_root);
                    // Shared selection drives Cyberterm and the other
                    // Cybercore apps; local Kitty themes aren't in it.
                    let _ = cybercore::theme::ThemeCatalog::load()
                        .map(|mut catalog| catalog.select(&theme.name).is_ok());
                }
                self.theme_menu.is_open = false;
            }
            Key::Named(NamedKey::Escape) => self.theme_menu.is_open = false,
            Key::Character(c) if c.eq_ignore_ascii_case("n") => {
                self.theme_menu.is_creating_mode = true;
            }
            Key::Character(c) if c.eq_ignore_ascii_case("q") => {
                self.theme_menu.is_open = false;
            }
            _ => {}
        }
    }

    fn init_gpu(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let window_attributes = Window::default_attributes()
            .with_title("Cyberterm")
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.window.width.max(100) as f64,
                self.config.window.height.max(80) as f64,
            ))
            .with_transparent(true);
        // Real Wayland app_id / X11 WM_CLASS -- without this, compositors
        // (Hyprland windowrules, taskbars, alt-tab) have no stable
        // identifier to match cyberterm's window against.
        #[cfg(all(unix, not(target_os = "macos")))]
        let window_attributes = window_attributes.with_name("cyberterm", "cyberterm");

        let window = Arc::new(
            event_loop
                .create_window(window_attributes)
                .map_err(|e| format!("failed to create window: {e}"))?,
        );
        window.set_ime_allowed(true);
        window.set_cursor(CursorIcon::Text);

        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("failed to create surface: {e}"))?;
        let adapter =
            futures::executor::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
            .ok_or("no compatible GPU adapter")?;
        let (device, queue) = futures::executor::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("Cyberterm GPU Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|e| format!("failed to open GPU device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        // Deliberately NOT an sRGB format: the quad shader writes plain,
        // already-gamma-encoded color values. On an sRGB surface wgpu would
        // re-encode them, washing dark theme backgrounds out toward pale
        // gray (a near-black `#261a30` rendered as lavender-gray).
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Prefer a compositing mode that blends this window's alpha with
        // the desktop (the opacity/blur look); falls back to opaque.
        let alpha_mode = [
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::PreMultiplied,
        ]
        .into_iter()
        .find(|m| caps.alpha_modes.contains(m))
        .unwrap_or(caps.alpha_modes[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let renderer = TermRenderer::new(
            &device,
            &queue,
            format,
            self.font_spec(window.scale_factor()),
        );
        self.gpu = Some(Gpu {
            surface,
            device,
            queue,
            config,
            renderer,
            premultiply_bg: alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied,
            window,
        });
        Ok(())
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        if let Err(e) = self.init_gpu(event_loop) {
            eprintln!("CRITICAL: {e}");
            event_loop.exit();
            return;
        }
        if let Err(e) = self.spawn_pane() {
            eprintln!("CRITICAL: Failed to spawn shell: {e}");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Term(id, event) => self.on_term_event(event_loop, id, event),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(mods) => {
                self.mods = mods.state();
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    if size.width > 0 && size.height > 0 {
                        gpu.config.width = size.width;
                        gpu.config.height = size.height;
                        gpu.surface.configure(&gpu.device, &gpu.config);
                        self.relayout();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => self.apply_font(),
            WindowEvent::Focused(focused) => {
                self.window_focused = focused;
                for pane in &self.panes {
                    let mut term = pane.session.term.lock();
                    term.is_focused = focused;
                    let report = term.mode().contains(TermMode::FOCUS_IN_OUT);
                    drop(term);
                    if report && pane.id == self.focused {
                        pane.session.write(if focused {
                            &b"\x1b[I"[..]
                        } else {
                            &b"\x1b[O"[..]
                        });
                    }
                }
                if !focused {
                    self.suppressed.clear();
                    self.menu = None;
                }
                self.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => self.on_key(event),
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, _) => {
                    self.ime_preedit = (!text.is_empty()).then_some(text);
                    self.request_redraw();
                }
                Ime::Commit(text) => {
                    self.ime_preedit = None;
                    if let Some(pane) = self.focused_pane() {
                        pane.session.term.lock().scroll_display(Scroll::Bottom);
                        pane.session.write(text.into_bytes());
                    }
                    self.request_redraw();
                }
                Ime::Enabled | Ime::Disabled => {
                    self.ime_preedit = None;
                }
            },
            WindowEvent::CursorMoved { position, .. } => self.on_cursor_moved(position),
            WindowEvent::CursorLeft { .. } => {
                if self.mouse.hover.take().is_some() {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => self.on_mouse_input(state, button),
            WindowEvent::MouseWheel { delta, .. } => self.on_wheel(delta),
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if now.duration_since(self.last_poll) >= POLL_INTERVAL {
            self.last_poll = now;
            self.poll_shared_theme();
            self.reload_config(false);
        }
        let mut next = self.last_poll + POLL_INTERVAL;

        // Cursor blink: only while focused, and only when the effective
        // cursor style (config or a program's DECSCUSR) asks for it.
        let blinking = self.window_focused
            && self
                .focused_pane()
                .is_some_and(|p| p.session.term.lock().cursor_style().blinking);
        if blinking {
            let interval = Duration::from_millis(self.config.cursor.blink_interval_ms.max(100));
            if now.duration_since(self.blink_last) >= interval {
                self.blink_visible = !self.blink_visible;
                self.blink_last = now;
                self.request_redraw();
            }
            next = next.min(self.blink_last + interval);
        } else if !self.blink_visible {
            self.blink_visible = true;
            self.request_redraw();
        }

        // Keep animating while a bell flash is fading.
        if self
            .panes
            .iter()
            .any(|p| p.bell.is_some_and(|at| now.duration_since(at) < BELL_FLASH))
        {
            self.request_redraw();
            next = next.min(now + Duration::from_millis(16));
        }

        event_loop.set_control_flow(ControlFlow::WaitUntil(next));
    }
}

fn clip_kind(ty: ClipboardType) -> ClipKind {
    match ty {
        ClipboardType::Clipboard => ClipKind::Clipboard,
        ClipboardType::Selection => ClipKind::Primary,
    }
}

/// Draws in-progress IME composition text at the cursor, underlined.
fn overlay_preedit(frame: &mut Frame, text: &str) {
    let Some(cursor) = frame.cursor else { return };
    let row = cursor.row;
    let mut col = cursor.col;
    for ch in text.chars() {
        if col >= frame.cols {
            break;
        }
        let index = row * frame.cols + col;
        let fg = frame.cells[index].fg;
        let cell = &mut frame.cells[index];
        cell.ch = ch;
        cell.zerowidth = None;
        cell.flags = alacritty_terminal::term::cell::Flags::UNDERLINE;
        cell.fg = fg;
        col += 1;
    }
    if let Some(c) = &mut frame.cursor {
        c.col = col.min(frame.cols - 1);
    }
}

fn open_link(uri: &str) {
    if !links::openable(uri) {
        eprintln!("cyberterm: not opening link with unsupported scheme: {uri}");
        return;
    }
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    spawn_detached(Command::new(opener).arg(uri));
}

/// Runs a helper process without blocking the UI, reaping it from a
/// background thread so it doesn't linger as a zombie.
fn spawn_detached(command: &mut Command) {
    match command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("cyberterm: failed to run {:?}: {e}", command.get_program()),
    }
}

fn report_config_errors(errors: &[String]) {
    for e in errors {
        eprintln!("cyberterm: keybinding ignored: {e}");
    }
}

fn hex_str_to_u32(s: &str) -> u32 {
    u32::from_str_radix(s.trim().trim_start_matches('#'), 16).unwrap_or(0)
}
