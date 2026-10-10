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
use crate::config::TabBarMode;
use crate::config::{self, CopyOnSelect, CursorShapeConfig, CyberConfig, Osc52Mode};
use crate::frame::{self, CursorOptions, Frame, FrameOptions, Palette};
use crate::input::bindings::{Action, Bindings};
use crate::input::keyboard::{self, KeyInput, KeyMode, KeyState};
use crate::input::{links, mouse, paste};
use crate::layout::{Divider, Tab, TabId};
use crate::renderer::{self, FontSpec, Overlay, PaneView, Rect, TermRenderer};
use crate::session::{GridSize, PaneId, Session, SpawnOptions, UserEvent};
use crate::shell;
use crate::theme::{Theme, ThemeRegistry};
use crate::ui;
use crate::ui::context_menu;

mod agents;
mod ai;
mod blocks;
mod control;
mod daemon;
mod danger;
pub use daemon::AttachTarget;
mod input;
mod overlays;
mod panes;
mod ssh;

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const BELL_FLASH: Duration = Duration::from_millis(150);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long a queued command waits for the shell's first prompt before
/// being typed anyway (shells without shell integration never report one).
const COMMAND_READY_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// A bell rang while the pane's tab wasn't showing.
    bell_unseen: bool,
    /// Title set by the program (OSC 0/2).
    title: String,
    /// A command to type once the shell is ready (layouts, `+ctl`), and
    /// when it was queued.
    pending_command: Option<(String, Instant)>,
    /// Newest block mark already considered for a "finished" notification.
    notified_mark: u64,
    /// (block count, newest finish time) last seen, to skip the block scan
    /// when nothing changed.
    block_sig: (usize, Option<u64>),
    /// Why this pane is in danger mode (`ssh prod-db`, `root`), if it is.
    danger: Option<String>,
    /// Set by hand (Ctrl+Shift+D): forced on or off.
    danger_manual: Option<bool>,
}

/// A link under the mouse pointer: viewport row, column range, target.
#[derive(Clone, Debug, PartialEq)]
struct HoverLink {
    row: usize,
    cols: std::ops::Range<usize>,
    uri: String,
    /// Set for `path:line` references (then `uri` is empty).
    file: Option<FileTarget>,
}

/// A file reference resolved to an existing file.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct FileTarget {
    path: PathBuf,
    line: u32,
    col: Option<u32>,
}

/// What a context-menu entry does.
#[derive(Clone, Debug)]
enum MenuAction {
    Do(Action),
    OpenLink(String),
    OpenFile(FileTarget),
    CopyText(String),
    CopyOutput(blocks::BlockRef),
    Rerun(blocks::BlockRef, String),
    RunInSplit(String, Option<PathBuf>),
    Watch(String, Option<PathBuf>),
    Diff(blocks::BlockRef, crate::shell::BlockMeta),
    ViewJson(blocks::BlockRef),
    RevokeAgents(PaneId),
    SplitLocal(crate::layout::Direction),
    Explain(blocks::BlockRef, shell::BlockMeta),
}

/// An open right-click menu, anchored at a cell of the focused pane.
struct ContextMenu {
    row: usize,
    col: usize,
    items: Vec<(context_menu::Item, MenuAction)>,
    hover: Option<usize>,
}

/// Everything one frame draws: panes (and the tab bar) as cell frames
/// with their bell flash and dim amounts, plus plain overlay rectangles.
#[derive(Default)]
struct DrawList {
    panes: Vec<(Rect, Frame, f32, f32)>,
    overlays: Vec<Overlay>,
    /// Index in `panes` of the focused pane.
    focused: Option<usize>,
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
    /// The split divider under the pointer (for the resize cursor).
    over_divider: Option<crate::layout::Axis>,
}

pub struct App {
    proxy: EventLoopProxy<UserEvent>,
    /// Declared before `gpu` so it's dropped before the window: on
    /// Wayland it shares the window's display connection.
    clipboard: Option<ClipboardManager>,
    gpu: Option<Gpu>,
    panes: Vec<Pane>,
    tabs: Vec<Tab>,
    active_tab: usize,
    next_tab: TabId,
    /// The active tab's focused pane (kept in sync by `sync_focus`).
    focused: PaneId,
    next_pane: PaneId,
    /// The leader key was pressed; the next key picks a leader binding.
    leader_pending: bool,
    /// A split divider being dragged with the mouse.
    divider_drag: Option<Divider>,
    /// Set when the last tab closes; the event loop exits on its next turn.
    exit_requested: bool,
    /// The control socket (`cyberterm +ctl`, scripts, agents).
    control: Option<crate::control::Server>,
    /// A layout file to open instead of a single shell (`+layout`).
    startup_layout: Option<PathBuf>,
    /// `+attach`: which daemon session to attach to.
    startup_attach: Option<daemon::AttachTarget>,
    /// The session daemon connection, when `[daemon] enabled`.
    daemon: Option<std::sync::Arc<crate::mux::client::DaemonClient>>,
    session_name: Option<String>,
    /// `session-<name>.sock`, pointing at this window's control socket.
    session_link: Option<PathBuf>,
    /// Tabs/splits changed since they were last saved with the session.
    layout_dirty: bool,
    /// The find bar (Ctrl+Shift+F) and history browser (Ctrl+Shift+H).
    find: Option<overlays::FindState>,
    history_ui: Option<overlays::HistoryUi>,
    /// AI agents' pending consent prompts, grants and badges.
    agents: agents::AgentState,
    /// The AI panel (Ask / Explain), and the last request number.
    ai_panel: Option<ai::AiPanel>,
    ai_seq: u64,
    /// An Enter held back in a dangerous pane, waiting for a second one.
    danger_confirm: Option<danger::DangerConfirm>,
    /// Saves finished commands (`[history]`).
    history: Option<crate::history::Recorder>,
    history_policy: crate::history::Policy,

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

/// Everything `main` prepares before the window exists.
pub struct Startup {
    pub config_root: PathBuf,
    pub themes_dir: PathBuf,
    pub config: CyberConfig,
    pub registry: ThemeRegistry,
    pub initial_theme: Option<Theme>,
    pub shared_theme_revision: u64,
    /// A layout file to open instead of a single shell (`+layout`).
    pub layout: Option<PathBuf>,
    /// `+attach [name]`.
    pub attach: Option<daemon::AttachTarget>,
}

impl App {
    pub fn new(proxy: EventLoopProxy<UserEvent>, startup: Startup) -> Self {
        let Startup {
            config_root,
            themes_dir,
            config,
            registry,
            initial_theme,
            shared_theme_revision,
            layout: startup_layout,
            attach: startup_attach,
        } = startup;
        let (bindings, errors) =
            Bindings::new(&config.keybindings, config.keyboard.leader.as_deref());
        report_config_errors(&errors);
        rewind::apply_rewind_config(&config.rewind);
        let mut app = Self {
            proxy,
            gpu: None,
            panes: Vec::new(),
            tabs: Vec::new(),
            active_tab: 0,
            next_tab: 0,
            focused: 0,
            next_pane: 0,
            leader_pending: false,
            divider_drag: None,
            exit_requested: false,
            control: None,
            startup_layout,
            startup_attach,
            daemon: None,
            session_name: None,
            session_link: None,
            layout_dirty: false,
            history: None,
            find: None,
            history_ui: None,
            agents: agents::AgentState::default(),
            ai_panel: None,
            ai_seq: 0,
            danger_confirm: None,
            history_policy: crate::history::Policy::from_config(
                &crate::config::HistoryConfig::default(),
            ),
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
            clipboard: Some(ClipboardManager::new()),
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
        if app.config.history.enabled {
            app.history = crate::history::Recorder::start(crate::history::default_path());
            app.history_policy = crate::history::Policy::from_config(&app.config.history);
        }
        if app.config.control.enabled {
            match crate::control::Server::start(app.proxy.clone()) {
                Ok(server) => app.control = Some(server),
                Err(e) => eprintln!("cyberterm: control socket unavailable: {e}"),
            }
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
        self.push_palette();
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

    fn padding(&self, gpu: &Gpu) -> f32 {
        (self.config.window.padding.max(0.0) as f64 * gpu.window.scale_factor()) as f32
    }

    /// Space between split panes: room for a divider line plus padding on
    /// both sides of it.
    fn gap(&self, gpu: &Gpu) -> f32 {
        (self.padding(gpu) * 2.0 + 1.0).round()
    }

    fn tab_bar_visible(&self) -> bool {
        match self.config.tabs.bar {
            TabBarMode::Always => true,
            TabBarMode::Never => false,
            TabBarMode::Auto => self.tabs.len() > 1,
        }
    }

    /// The tab bar's rectangle (if shown) and the area panes share.
    fn areas(&self, gpu: &Gpu) -> (Option<Rect>, Rect) {
        let pad = self.padding(gpu);
        let (w, h) = (gpu.config.width as f32, gpu.config.height as f32);
        let bar_h = gpu.renderer.cell_size().1;
        let bar = self.tab_bar_visible().then_some(Rect {
            x: pad,
            y: pad,
            w: (w - 2.0 * pad).max(1.0),
            h: bar_h,
        });
        let top = match bar {
            Some(b) => b.y + b.h + pad,
            None => pad,
        };
        let content = Rect {
            x: pad,
            y: top,
            w: (w - 2.0 * pad).max(1.0),
            h: (h - top - pad).max(1.0),
        };
        (bar, content)
    }

    /// Recomputes every pane's rectangle and grid size after a window
    /// resize, split change, font change or padding change. Panes in
    /// background tabs are resized too, so their programs always see the
    /// size they'll be shown at.
    fn relayout(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let (_, area) = self.areas(gpu);
        let gap = self.gap(gpu);
        let (cw, ch) = gpu.renderer.cell_size();
        let mut sizes = Vec::new();
        for tab in &self.tabs {
            for (id, rect) in tab.visible(area, gap) {
                let (cols, rows) = gpu.renderer.grid_size(rect.w, rect.h);
                sizes.push((id, rect, GridSize { cols, rows }));
            }
        }
        for (id, rect, size) in sizes {
            if let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) {
                pane.rect = rect;
                pane.session.resize(size, cw, ch);
            }
        }
        self.layout_dirty = true;
        self.request_redraw();
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
        rewind::apply_rewind_config(&self.config.rewind);
        let (bindings, errors) = Bindings::new(
            &self.config.keybindings,
            self.config.keyboard.leader.as_deref(),
        );
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
    // Terminal events
    // ------------------------------------------------------------------

    fn on_term_event(&mut self, event_loop: &ActiveEventLoop, id: PaneId, event: TermEvent) {
        let Some(index) = self.panes.iter().position(|p| p.id == id) else {
            return;
        };
        // A daemon pane's replica parses the same stream as the daemon's own
        // terminal, which already answered these queries.
        if self.panes[index].session.is_remote()
            && matches!(
                event,
                TermEvent::PtyWrite(_)
                    | TermEvent::ColorRequest(..)
                    | TermEvent::TextAreaSizeRequest(_)
                    | TermEvent::ClipboardLoad(..)
            )
        {
            return;
        }
        match event {
            TermEvent::Wakeup => {
                self.flush_pending_command(index);
                let sig = {
                    let shell = self.panes[index].session.shell.lock();
                    (
                        shell.blocks.len(),
                        shell.blocks.back().and_then(|b| b.finished_ms),
                    )
                };
                if sig != self.panes[index].block_sig {
                    self.panes[index].block_sig = sig;
                    self.blocks_changed(index);
                }
                self.request_redraw();
            }
            TermEvent::Title(title) => {
                self.panes[index].title = title;
                if id == self.focused {
                    self.update_window_title();
                }
                self.request_redraw();
            }
            TermEvent::ResetTitle => {
                self.panes[index].title.clear();
                if id == self.focused {
                    self.update_window_title();
                }
                self.request_redraw();
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
                self.remove_pane(id);
                if self.exit_requested {
                    event_loop.exit();
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
        let id = self.panes[index].id;
        if !self.active().is_some_and(|t| t.root.contains(id)) {
            self.panes[index].bell_unseen = true;
        }
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

    fn tab_labels(&self) -> Vec<ui::tab_bar::TabLabel> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| ui::tab_bar::TabLabel {
                title: self.tab_title(t),
                active: i == self.active_tab,
                bell: t
                    .root
                    .panes()
                    .iter()
                    .any(|id| self.pane(*id).is_some_and(|p| p.bell_unseen)),
                zoomed: t.zoomed,
                broadcast: t.broadcast,
                danger: self.tab_in_danger(t),
            })
            .collect()
    }

    /// The tab under a window position, if it's on the tab bar.
    fn tab_at(&self, x: f32, y: f32) -> Option<usize> {
        let gpu = self.gpu.as_ref()?;
        let (Some(bar), _) = self.areas(gpu) else {
            return None;
        };
        if !(x >= bar.x && x < bar.x + bar.w && y >= bar.y && y < bar.y + bar.h) {
            return None;
        }
        let cw = gpu.renderer.cell_size().0;
        let cols = (bar.w / cw).floor().max(1.0) as usize;
        let colors = ui::tab_bar::Colors {
            fg: [0; 3],
            bg: [0; 3],
            dim: [0; 3],
            accent: [0; 3],
            alert: [0; 3],
        };
        let built = ui::tab_bar::build(&self.tab_labels(), cols, None, &colors);
        ui::tab_bar::tab_at(&built, ((x - bar.x) / cw) as usize)
    }

    fn build_frames(&self) -> DrawList {
        let now = Instant::now();
        let mut list = DrawList::default();
        let Some(gpu) = &self.gpu else {
            return list;
        };
        let accent = frame::hex_to_rgb(self.palette.cursor);
        let dim_color = frame::hex_to_rgb(self.palette.ansi[8]);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let fg = frame::hex_to_rgb(self.palette.fg);
        let tab = self.active();
        let multiple = tab.is_some_and(|t| t.root.panes().len() > 1 && !t.zoomed);

        for (id, rect) in self.visible_rects() {
            let Some(pane) = self.pane(id) else { continue };
            let focused = pane.id == self.focused;
            let hover: Vec<(usize, std::ops::Range<usize>)> = self
                .mouse
                .hover
                .iter()
                .filter(|_| focused)
                .map(|h| (h.row, h.cols.clone()))
                .collect();
            let size = pane.session.size();
            let history_frame = self.draw_rewind(pane.id, size.cols, size.rows).or_else(|| {
                focused
                    .then(|| self.draw_history(size.cols, size.rows))
                    .flatten()
            });
            let mut frame = if let Some(f) = history_frame {
                f
            } else if focused && self.theme_menu.is_open {
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
                Frame::from_spans(&lines, size.cols, size.rows, bg)
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
                        fg,
                        bg,
                        dim: dim_color,
                        accent,
                    };
                    context_menu::draw(&mut frame, &layout, &items, menu.hover, &colors);
                }
                list.focused = Some(list.panes.len());
            }
            let flash = pane
                .bell
                .map(|at| now.saturating_duration_since(at))
                .filter(|d| *d < BELL_FLASH)
                .map(|d| 1.0 - d.as_secs_f32() / BELL_FLASH.as_secs_f32())
                .unwrap_or(0.0);
            let dim = if multiple && !focused {
                self.config.splits.inactive_dim.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let overlay_open = focused && (self.theme_menu.is_open || self.history_ui.is_some());
            if !overlay_open {
                self.decorate_blocks(pane, &mut frame, rect, &mut list.overlays);
                self.danger_overlays(pane, rect, &mut list.overlays);
                if focused {
                    self.draw_find(&mut frame);
                }
            }
            let used = self.draw_danger_badge(pane, &mut frame);
            self.draw_agent_badge(pane.id, &mut frame, used);
            if self.ai_panel_pane() == Some(pane.id) {
                self.draw_ai(&mut frame);
            }
            if focused {
                self.draw_danger_confirm(&mut frame);
                self.draw_consent(&mut frame);
            }
            list.panes.push((rect, frame, flash, dim));
        }

        // Dividers: a one-pixel line in the middle of each gap, in the
        // accent color while broadcasting.
        if let Some(tab) = tab.filter(|t| !t.zoomed) {
            let (_, area) = self.areas(gpu);
            let line = (gpu.window.scale_factor() as f32).round().max(1.0);
            let color = if tab.broadcast { accent } else { dim_color };
            for d in tab.root.dividers(area, self.gap(gpu)) {
                let r = d.rect;
                let rect = match d.axis {
                    crate::layout::Axis::Horizontal => Rect {
                        x: (r.x + (r.w - line) / 2.0).round(),
                        w: line,
                        ..r
                    },
                    crate::layout::Axis::Vertical => Rect {
                        y: (r.y + (r.h - line) / 2.0).round(),
                        h: line,
                        ..r
                    },
                };
                list.overlays.push(Overlay {
                    rect,
                    color,
                    alpha: 1.0,
                });
            }
        }

        let status = self.leader_pending.then_some("LEADER");
        let (bar_rect, area) = self.areas(gpu);
        let (cw, _) = gpu.renderer.cell_size();
        if let Some(bar_rect) = bar_rect {
            let labels = self.tab_labels();
            let cols = (bar_rect.w / cw).floor().max(1.0) as usize;
            let colors = ui::tab_bar::Colors {
                fg,
                bg,
                dim: fg,
                accent,
                alert: frame::hex_to_rgb(self.palette.ansi[1]),
            };
            let bar = ui::tab_bar::build(&labels, cols, status, &colors);
            list.panes.push((bar_rect, bar.frame, 0.0, 0.0));
        } else if let Some(status) = status {
            // No tab bar: show the pending leader as a chip in the corner.
            let text = format!(" {status} ");
            let cols = text.chars().count();
            let colors_bg = frame::hex_to_rgb(self.palette.ansi[1]);
            let chip = Frame::from_spans(&[vec![(text, bg)]], cols, 1, colors_bg);
            let rect = Rect {
                x: area.x + area.w - cols as f32 * cw,
                y: area.y,
                w: cols as f32 * cw,
                h: gpu.renderer.cell_size().1,
            };
            list.panes.push((rect, chip, 0.0, 0.0));
        }
        list
    }

    fn draw(&mut self) {
        self.expire_consents();
        self.refresh_overlays();
        let list = self.build_frames();
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
        let bg = list
            .focused
            .and_then(|i| list.panes.get(i))
            .map(|(_, f, _, _)| f.bg)
            .unwrap_or_else(|| frame::hex_to_rgb(self.palette.bg));
        let opacity = self.config.opacity.clamp(0.0, 1.0) as f64;
        let mul = if gpu.premultiply_bg { opacity } else { 1.0 };
        let clear_color = wgpu::Color {
            r: bg[0] as f64 / 255.0 * mul,
            g: bg[1] as f64 / 255.0 * mul,
            b: bg[2] as f64 / 255.0 * mul,
            a: opacity,
        };

        let views: Vec<PaneView<'_>> = list
            .panes
            .iter()
            .map(|(rect, frame, flash, dim)| PaneView {
                rect: *rect,
                frame,
                flash: *flash,
                dim: *dim,
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
            &list.overlays,
        );
        gpu.queue.submit(std::iter::once(encoder.finish()));
        gpu.window.pre_present_notify();
        surface_texture.present();

        // Tell the input method where the cursor is, so its candidate
        // window pops up next to the text being composed.
        if let Some((rect, frame, _, _)) = list.focused.and_then(|i| list.panes.get(i)) {
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
        if let Some(clipboard) = &mut self.clipboard {
            clipboard.attach(&window);
        }
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

impl Drop for App {
    fn drop(&mut self) {
        self.unlink_session_socket();
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
        if self.config.daemon.enabled {
            match self.start_daemon_session() {
                Ok(true) => return,
                Ok(false) => {}
                Err(e) => {
                    eprintln!("cyberterm: session daemon unavailable, shells will live in this window: {e}");
                }
            }
        }
        if let Some(path) = self.startup_layout.take() {
            match crate::layout_file::load(&path) {
                Ok(plans) => match self.open_layout(plans) {
                    Ok(()) => return,
                    Err(e) => eprintln!("cyberterm: layout failed: {e}"),
                },
                Err(e) => {
                    // Say so where the user will see it: in the shell.
                    eprintln!("cyberterm: layout not loaded: {e}");
                    if let Ok(id) = self.open_tab(None) {
                        let message =
                            format!("cyberterm: layout not loaded: {e}").replace('\'', "'\\''");
                        self.run_in(id, Some(&format!("printf '%s\\n' '{message}'")));
                    }
                    return;
                }
            }
        }
        if let Err(e) = self.open_tab(None) {
            eprintln!("CRITICAL: Failed to spawn shell: {e}");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Term(id, event) => self.on_term_event(event_loop, id, event),
            UserEvent::DaemonLost(reason) => self.daemon_lost(&reason),
            UserEvent::Ai(seq, result) => self.on_ai_answer(seq, result),
            UserEvent::Control(call) => {
                self.on_control_call(call);
                if self.exit_requested {
                    event_loop.exit();
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.detach();
                event_loop.exit();
            }
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
                    self.leader_pending = false;
                    self.divider_drag = None;
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
        if self.exit_requested {
            event_loop.exit();
            return;
        }
        if self.layout_dirty {
            self.save_layout();
        }
        let now = Instant::now();
        if now.duration_since(self.last_poll) >= POLL_INTERVAL {
            self.last_poll = now;
            self.poll_shared_theme();
            self.reload_config(false);
            self.refresh_danger();
        }
        let mut next = self.last_poll + POLL_INTERVAL;

        for index in 0..self.panes.len() {
            self.flush_pending_command(index);
        }
        if let Some(queued) = self
            .panes
            .iter()
            .filter_map(|p| p.pending_command.as_ref())
            .map(|(_, at)| *at)
            .min()
        {
            next = next.min(queued + COMMAND_READY_TIMEOUT);
        }

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
