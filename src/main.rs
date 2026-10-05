// Cyberterm - Fully customizable terminal written in rust.
// The ultimate, scriptable, vintage-meets-modern terminal engine.

mod cli;
mod clipboard;
mod config;
mod keys;
mod renderer;
mod tabs;
mod theme;
mod ui;

use alacritty_terminal::event::{
    Event as AlacrittyEvent, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use alacritty_terminal::Term;
use clipboard::ClipboardManager;
use std::path::PathBuf;
use std::sync::Arc;
use theme::{Theme, ThemeRegistry};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
#[cfg(all(unix, not(target_os = "macos")))]
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Window, WindowId};

/// The event type alacritty's PTY/VTE background thread sends back to the
/// winit event loop (new content ready, title changes, the shell exiting,
/// clipboard/color queries, ...).
type TermEvent = AlacrittyEvent;

/// Bridges alacritty's `EventListener` callback (fired from its own PTY
/// reader thread) into a real winit user event, so the GUI thread finds out
/// about new terminal content / title changes / the shell exiting.
#[derive(Clone)]
struct EventProxy(EventLoopProxy<TermEvent>);

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.send_event(event);
    }
}

struct TermSize {
    cols: alacritty_terminal::index::Column,
    rows: alacritty_terminal::index::Line,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows.0 as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows.0 as usize
    }
    fn columns(&self) -> usize {
        self.cols.0
    }
}

pub struct ThemeMenuState {
    pub is_open: bool,
    pub is_creating_mode: bool,
    pub theme_name_input: String,
    pub registry: ThemeRegistry,
}

/// Everything that only exists once the window/GPU surface/shell are up --
/// created in `resumed()`, not before, since wgpu needs a real surface and
/// the PTY needs a real initial cell grid size from the renderer's font
/// metrics.
struct Backend {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: renderer::TermRenderer,
    term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: Notifier,
    cols: usize,
    rows: usize,
    premultiply_bg: bool,
    // Rust drops struct fields in declaration order -- `window` must be
    // LAST so it outlives `surface` and drops after it. wgpu's Surface
    // holds a reference tied to the live window; tearing the window down
    // first and then dropping a Surface pointed at now-destroyed window
    // state is a real, documented crash (glyphon's own hello-world example
    // calls this out explicitly). This was backwards until now, and lines
    // up with several real SIGSEGV coredumps captured during this session.
    window: Arc<Window>,
}

struct CybertermApp {
    backend: Option<Backend>,
    event_proxy: EventLoopProxy<TermEvent>,
    clipboard: Option<ClipboardManager>,
    theme_menu: ThemeMenuState,
    active_palette: [u32; 16],
    active_bg: u32,
    active_fg: u32,
    current_mods: ModifiersState,
    themes_directory: PathBuf,
    config_root: PathBuf,
    cyber_config: config::CyberConfig,
    _tab_manager: tabs::TabContainer,
}

impl CybertermApp {
    fn apply_theme(&mut self, theme: &Theme) {
        self.active_palette = theme.colors;
        self.active_bg = hex_str_to_u32(&theme.background);
        self.active_fg = hex_str_to_u32(&theme.foreground);
    }
}

impl ApplicationHandler<TermEvent> for CybertermApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.backend.is_some() {
            return;
        }

        let window_attributes = Window::default_attributes()
            .with_title("Cyberterm")
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 600.0))
            .with_transparent(true);
        // Real Wayland app_id / X11 WM_CLASS -- without this, window
        // managers/compositors (Hyprland windowrules, taskbars, alt-tab)
        // have no stable identifier to match cyberterm's window against, so
        // e.g. a `windowrulev2 = opacity 0.9 0.9,class:^(cyberterm)$` rule
        // (the same mechanism Ghostty's own blur/opacity rule uses) can't
        // target it at all.
        #[cfg(all(unix, not(target_os = "macos")))]
        let window_attributes = window_attributes.with_name("cyberterm", "cyberterm");

        let window = match event_loop.create_window(window_attributes) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("CRITICAL: Failed to create window: {}", e);
                return;
            }
        };

        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter =
            futures::executor::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
            .unwrap();

        let (device, queue) = futures::executor::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("Cyberterm GPU Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .unwrap();

        let surface_caps = surface.get_capabilities(&adapter);
        // Deliberately NOT an sRGB format: our own cell-background quad
        // shader (renderer.rs) writes plain, already-gamma-encoded 0-1
        // color values straight through with no linearization. On an sRGB
        // surface, wgpu re-applies the sRGB encoding curve on write,
        // double-encoding every color and washing dark/mid-tone hex colors
        // out toward a lighter, desaturated version of themselves -- e.g. a
        // theme's near-black `#261a30` background rendering as a pale
        // lavender-gray instead. A plain Unorm format writes exactly the
        // bytes we computed from the theme's hex colors, as intended.
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);

        // Prefer a compositing mode that actually blends this window's
        // alpha channel with the desktop behind it (for the opacity/blur
        // look Ghostty has on Omarchy) -- falls back to fully opaque on any
        // adapter/compositor combo that doesn't support it.
        let alpha_mode = surface_caps
            .alpha_modes
            .iter()
            .copied()
            .find(|m| *m == wgpu::CompositeAlphaMode::PostMultiplied)
            .or_else(|| {
                surface_caps
                    .alpha_modes
                    .iter()
                    .copied()
                    .find(|m| *m == wgpu::CompositeAlphaMode::PreMultiplied)
            })
            .unwrap_or(surface_caps.alpha_modes[0]);
        let premultiply_bg = alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied;

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let renderer = renderer::TermRenderer::new(&device, &queue, surface_format);
        let (cell_width, cell_height) = renderer.cell_size();
        let (cols, rows) = renderer.grid_size(config.width, config.height);

        let event_proxy = EventProxy(self.event_proxy.clone());

        let term_size = TermSize {
            cols: alacritty_terminal::index::Column(cols),
            rows: alacritty_terminal::index::Line(rows as i32),
        };
        let term = Arc::new(FairMutex::new(Term::new(
            alacritty_terminal::term::Config::default(),
            &term_size,
            event_proxy.clone(),
        )));

        let shell_program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let pty_options = tty::Options {
            shell: Some(tty::Shell::new(shell_program, Vec::new())),
            working_directory: None,
            drain_on_exit: true,
            env: Default::default(),
        };
        let window_size = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: cell_width as u16,
            cell_height: cell_height as u16,
        };

        let pty = match tty::new(&pty_options, window_size, 0) {
            Ok(pty) => pty,
            Err(e) => {
                eprintln!("CRITICAL: Failed to spawn shell: {}", e);
                return;
            }
        };

        let pty_event_loop = PtyEventLoop::new(term.clone(), event_proxy, pty, true, false)
            .expect("failed to start PTY event loop");
        let notifier = Notifier(pty_event_loop.channel());
        let _pty_io_thread = pty_event_loop.spawn();

        self.backend = Some(Backend {
            window,
            surface,
            device,
            queue,
            config,
            renderer,
            term,
            notifier,
            cols,
            rows,
            premultiply_bg,
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: TermEvent) {
        let Some(backend) = &mut self.backend else {
            return;
        };

        match event {
            AlacrittyEvent::Wakeup => backend.window.request_redraw(),
            AlacrittyEvent::Title(title) => backend.window.set_title(&title),
            AlacrittyEvent::ResetTitle => backend.window.set_title("Cyberterm"),
            AlacrittyEvent::PtyWrite(text) => backend.notifier.notify(text.into_bytes()),
            AlacrittyEvent::ChildExit(_) | AlacrittyEvent::Exit => event_loop.exit(),
            AlacrittyEvent::Bell => {}
            AlacrittyEvent::MouseCursorDirty | AlacrittyEvent::CursorBlinkingChange => {}
            AlacrittyEvent::ClipboardStore(_, text) => {
                if let Some(clipboard) = &mut self.clipboard {
                    clipboard.copy_clean_string(&text);
                }
            }
            AlacrittyEvent::ClipboardLoad(_, format) => {
                let contents = String::new(); // no clipboard read wired up yet; respond empty
                backend.notifier.notify(format(&contents).into_bytes());
            }
            AlacrittyEvent::ColorRequest(index, format) => {
                let rgb = resolve_request_color(
                    index,
                    &self.active_palette,
                    self.active_fg,
                    self.active_bg,
                );
                backend.notifier.notify(
                    format(alacritty_terminal::vte::ansi::Rgb {
                        r: rgb[0],
                        g: rgb[1],
                        b: rgb[2],
                    })
                    .into_bytes(),
                );
            }
            AlacrittyEvent::TextAreaSizeRequest(format) => {
                let (cell_width, cell_height) = backend.renderer.cell_size();
                let window_size = WindowSize {
                    num_lines: backend.rows as u16,
                    num_cols: backend.cols as u16,
                    cell_width: cell_width as u16,
                    cell_height: cell_height as u16,
                };
                backend.notifier.notify(format(window_size).into_bytes());
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Taken out (rather than borrowed) for the duration of this call: the
        // KeyboardInput arm below needs to call `self.handle_theme_menu_key`,
        // which requires a full `&mut self` and can't coexist with a live
        // `&mut self.backend` borrow.
        let Some(mut backend) = self.backend.take() else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::ModifiersChanged(mods) => {
                self.current_mods = mods.state();
            }

            WindowEvent::Resized(new_size) => {
                if new_size.width > 0 && new_size.height > 0 {
                    backend.config.width = new_size.width;
                    backend.config.height = new_size.height;
                    backend.surface.configure(&backend.device, &backend.config);

                    let (cols, rows) = backend.renderer.grid_size(new_size.width, new_size.height);
                    backend.cols = cols;
                    backend.rows = rows;

                    let term_size = TermSize {
                        cols: alacritty_terminal::index::Column(cols),
                        rows: alacritty_terminal::index::Line(rows as i32),
                    };
                    backend.term.lock().resize(term_size);

                    let (cell_width, cell_height) = backend.renderer.cell_size();
                    backend.notifier.on_resize(WindowSize {
                        num_lines: rows as u16,
                        num_cols: cols as u16,
                        cell_width: cell_width as u16,
                        cell_height: cell_height as u16,
                    });

                    backend.window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                let frame = match backend.surface.get_current_texture() {
                    Ok(texture) => texture,
                    Err(wgpu::SurfaceError::Timeout) => return,
                    Err(wgpu::SurfaceError::Outdated) => {
                        backend.surface.configure(&backend.device, &backend.config);
                        return;
                    }
                    Err(e) => {
                        eprintln!("Pipeline Draw Warning: {:?}", e);
                        return;
                    }
                };

                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder =
                    backend
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("Primary Frame Encoder"),
                        });

                let bg = hex_to_rgb(self.active_bg);
                let opacity = self.cyber_config.opacity.clamp(0.0, 1.0) as f64;
                let (cr, cg, cb) = if backend.premultiply_bg {
                    (
                        bg[0] as f64 / 255.0 * opacity,
                        bg[1] as f64 / 255.0 * opacity,
                        bg[2] as f64 / 255.0 * opacity,
                    )
                } else {
                    (
                        bg[0] as f64 / 255.0,
                        bg[1] as f64 / 255.0,
                        bg[2] as f64 / 255.0,
                    )
                };
                let clear_color = wgpu::Color {
                    r: cr,
                    g: cg,
                    b: cb,
                    a: opacity,
                };

                let cells = if self.theme_menu.is_open {
                    menu_to_cells(
                        &ui::theme_menu::build_lines(
                            &self.theme_menu.registry,
                            self.theme_menu.is_creating_mode,
                            &self.theme_menu.theme_name_input,
                        ),
                        backend.cols,
                        backend.rows,
                        self.active_bg,
                    )
                } else {
                    let term = backend.term.lock();
                    term_to_cells(&term, &self.active_palette, self.active_fg, self.active_bg)
                };

                backend.renderer.render(
                    &backend.device,
                    &backend.queue,
                    &mut encoder,
                    renderer::FrameTarget {
                        view: &view,
                        width_px: backend.config.width,
                        height_px: backend.config.height,
                        clear_color,
                        background_alpha: opacity as f32,
                        premultiply: backend.premultiply_bg,
                    },
                    &cells,
                );

                backend.queue.submit(std::iter::once(encoder.finish()));
                frame.present();
            }

            WindowEvent::KeyboardInput {
                event: key_event, ..
            } if key_event.state == ElementState::Pressed => {
                if self.theme_menu.is_open {
                    self.handle_theme_menu_key(&key_event.logical_key);
                } else if key_event.physical_key == PhysicalKey::Code(KeyCode::KeyT)
                    && self.current_mods.control_key()
                    && self.current_mods.shift_key()
                {
                    // Physical key, not logical: `logical_key` reflects what
                    // Shift actually produces (uppercase "T"), so comparing
                    // it against a lowercase "t" string never matched --
                    // the keystroke fell through and was encoded as a raw
                    // Ctrl+T byte sent to the shell instead (which fzf's
                    // shell integration binds to its file-widget, opening
                    // what looked like an unrelated filesystem scan).
                    self.theme_menu.is_open = true;
                } else if let Some(bytes) =
                    keys::encode_key(&key_event.logical_key, self.current_mods)
                {
                    backend.notifier.notify(bytes);
                }

                backend.window.request_redraw();
            }
            _ => {}
        }

        self.backend = Some(backend);
    }
}

impl CybertermApp {
    fn handle_theme_menu_key(&mut self, key: &Key) {
        if self.theme_menu.is_creating_mode {
            match key {
                Key::Named(NamedKey::Enter) => {
                    if !self.theme_menu.theme_name_input.trim().is_empty() {
                        if let Ok(new_file) = ThemeRegistry::create_new_theme_template(
                            &self.themes_directory,
                            &self.theme_menu.theme_name_input,
                        ) {
                            let editor =
                                std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
                            let _ = std::process::Command::new(editor).arg(new_file).status();
                            self.theme_menu.registry =
                                ThemeRegistry::load_from_dir(&self.themes_directory);
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

        match key {
            Key::Named(NamedKey::ArrowDown) => {
                if !self.theme_menu.registry.themes.is_empty() {
                    self.theme_menu.registry.selected_index =
                        (self.theme_menu.registry.selected_index + 1)
                            % self.theme_menu.registry.themes.len();
                    let theme = self.theme_menu.registry.themes
                        [self.theme_menu.registry.selected_index]
                        .clone();
                    self.apply_theme(&theme);
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                if !self.theme_menu.registry.themes.is_empty() {
                    let len = self.theme_menu.registry.themes.len();
                    self.theme_menu.registry.selected_index =
                        (self.theme_menu.registry.selected_index + len - 1) % len;
                    let theme = self.theme_menu.registry.themes
                        [self.theme_menu.registry.selected_index]
                        .clone();
                    self.apply_theme(&theme);
                }
            }
            Key::Named(NamedKey::Enter) => {
                if let Some(theme) = self
                    .theme_menu
                    .registry
                    .themes
                    .get(self.theme_menu.registry.selected_index)
                {
                    self.cyber_config.theme = theme.name.clone();
                    let _ = config::save_config(&self.config_root, &self.cyber_config);
                    if cybercore::theme::ThemeCatalog::load()
                        .is_ok_and(|mut catalog| catalog.select(&theme.name).is_ok())
                    {
                        // Shared selection now drives Cyberterm and the other Cybercore apps.
                    }
                }
                self.theme_menu.is_open = false;
            }
            Key::Named(NamedKey::Escape) => self.theme_menu.is_open = false,
            Key::Character(c) if c.as_str() == "n" || c.as_str() == "N" => {
                self.theme_menu.is_creating_mode = true;
            }
            Key::Character(c) if c.as_str() == "q" || c.as_str() == "Q" => {
                self.theme_menu.is_open = false;
            }
            _ => {}
        }
    }
}

fn hex_str_to_u32(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0)
}

fn hex_to_rgb(color: u32) -> [u8; 3] {
    [
        ((color >> 16) & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        (color & 0xFF) as u8,
    ]
}

/// The real xterm 256-color formula: a 6x6x6 color cube (16-231) plus a
/// 24-step grayscale ramp (232-255) -- this is what makes `ls --color`/`bat`
/// output that uses indices beyond the base 16 render correctly.
fn indexed_256_to_rgb(idx: u8) -> [u8; 3] {
    if idx >= 232 {
        let level = 8 + (idx - 232) * 10;
        [level, level, level]
    } else {
        let i = idx - 16;
        let r = i / 36;
        let g = (i / 6) % 6;
        let b = i % 6;
        let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        [scale(r), scale(g), scale(b)]
    }
}

fn dim(rgb: [u8; 3]) -> [u8; 3] {
    rgb.map(|c| (c as f32 * 0.7) as u8)
}

fn resolve_named(
    named: NamedColor,
    palette: &[u32; 16],
    default_fg: u32,
    default_bg: u32,
) -> [u8; 3] {
    use NamedColor::*;
    match named {
        Foreground | BrightForeground => hex_to_rgb(default_fg),
        DimForeground => dim(hex_to_rgb(default_fg)),
        Background => hex_to_rgb(default_bg),
        Cursor => hex_to_rgb(default_fg),
        Black => hex_to_rgb(palette[0]),
        Red => hex_to_rgb(palette[1]),
        Green => hex_to_rgb(palette[2]),
        Yellow => hex_to_rgb(palette[3]),
        Blue => hex_to_rgb(palette[4]),
        Magenta => hex_to_rgb(palette[5]),
        Cyan => hex_to_rgb(palette[6]),
        White => hex_to_rgb(palette[7]),
        BrightBlack => hex_to_rgb(palette[8]),
        BrightRed => hex_to_rgb(palette[9]),
        BrightGreen => hex_to_rgb(palette[10]),
        BrightYellow => hex_to_rgb(palette[11]),
        BrightBlue => hex_to_rgb(palette[12]),
        BrightMagenta => hex_to_rgb(palette[13]),
        BrightCyan => hex_to_rgb(palette[14]),
        BrightWhite => hex_to_rgb(palette[15]),
        DimBlack => dim(hex_to_rgb(palette[0])),
        DimRed => dim(hex_to_rgb(palette[1])),
        DimGreen => dim(hex_to_rgb(palette[2])),
        DimYellow => dim(hex_to_rgb(palette[3])),
        DimBlue => dim(hex_to_rgb(palette[4])),
        DimMagenta => dim(hex_to_rgb(palette[5])),
        DimCyan => dim(hex_to_rgb(palette[6])),
        DimWhite => dim(hex_to_rgb(palette[7])),
    }
}

fn resolve_color(
    color: AnsiColor,
    palette: &[u32; 16],
    default_fg: u32,
    default_bg: u32,
) -> [u8; 3] {
    match color {
        AnsiColor::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        AnsiColor::Indexed(idx) => {
            if idx < 16 {
                hex_to_rgb(palette[idx as usize])
            } else {
                indexed_256_to_rgb(idx)
            }
        }
        AnsiColor::Named(named) => resolve_named(named, palette, default_fg, default_bg),
    }
}

/// Same resolution used for OSC 4/10/11/12 color queries (`ColorRequest`) --
/// `index` follows the same convention as `Indexed`, plus alacritty's
/// special-cased 256/257/258 for fg/bg/cursor.
fn resolve_request_color(
    index: usize,
    palette: &[u32; 16],
    default_fg: u32,
    default_bg: u32,
) -> [u8; 3] {
    match index {
        256 => hex_to_rgb(default_fg),
        257 => hex_to_rgb(default_bg),
        258 => hex_to_rgb(default_fg),
        i if i < 16 => hex_to_rgb(palette[i]),
        i if i <= 255 => indexed_256_to_rgb(i as u8),
        _ => hex_to_rgb(default_fg),
    }
}

fn term_to_cells(
    term: &Term<EventProxy>,
    palette: &[u32; 16],
    default_fg: u32,
    default_bg: u32,
) -> Vec<Vec<renderer::RenderCell>> {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let bg_rgb = hex_to_rgb(default_bg);
    let fg_rgb = hex_to_rgb(default_fg);

    let mut cells = vec![
        vec![
            renderer::RenderCell {
                ch: ' ',
                fg: fg_rgb,
                bg: bg_rgb
            };
            cols
        ];
        rows
    ];

    let display_offset = grid.display_offset() as i32;
    for indexed in grid.display_iter() {
        let row = indexed.point.line.0 + display_offset;
        let col = indexed.point.column.0;
        if row < 0 || row as usize >= rows || col >= cols {
            continue;
        }

        let cell = indexed.cell;
        let mut fg = resolve_color(cell.fg, palette, default_fg, default_bg);
        let mut bg = resolve_color(cell.bg, palette, default_fg, default_bg);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        cells[row as usize][col] = renderer::RenderCell { ch: cell.c, fg, bg };
    }

    if term.mode().contains(TermMode::SHOW_CURSOR) {
        let cursor_point = grid.cursor.point;
        let crow = cursor_point.line.0 + display_offset;
        let ccol = cursor_point.column.0;
        if crow >= 0 && (crow as usize) < rows && ccol < cols {
            let c = &mut cells[crow as usize][ccol];
            std::mem::swap(&mut c.fg, &mut c.bg);
        }
    }

    cells
}

/// Lays the theme-picker HUD's colored line spans out onto a plain cell
/// grid, reusing the same `RenderCell`-based renderer path as the terminal
/// grid itself instead of a separate rendering codepath.
fn menu_to_cells(
    lines: &[Vec<ui::theme_menu::Span>],
    cols: usize,
    rows: usize,
    default_bg: u32,
) -> Vec<Vec<renderer::RenderCell>> {
    let bg = hex_to_rgb(default_bg);
    let mut cells = vec![
        vec![
            renderer::RenderCell {
                ch: ' ',
                fg: [0xff, 0xff, 0xff],
                bg
            };
            cols
        ];
        rows
    ];

    for (row_idx, line) in lines.iter().enumerate().take(rows) {
        let mut col = 0usize;
        for span in line {
            for ch in span.text.chars() {
                if col >= cols {
                    break;
                }
                cells[row_idx][col] = renderer::RenderCell {
                    ch,
                    fg: span.color,
                    bg,
                };
                col += 1;
            }
        }
    }

    cells
}

fn main() {
    let themes_dir = match config::initialize_cyberterm_directories() {
        Ok(base_path) => base_path.join("themes"),
        Err(e) => {
            eprintln!(
                "Initialization Warning: Could not verify data paths ({})",
                e
            );
            PathBuf::from("themes")
        }
    };

    let config_root = themes_dir.parent().unwrap_or(&themes_dir).to_path_buf();

    let cyber_config =
        config::load_config(&config_root).unwrap_or_else(|_| config::CyberConfig::default());

    let sys_args: Vec<String> = std::env::args().collect();
    if matches!(
        cli::handle_arguments(sys_args, &config_root, cyber_config.clone()),
        cli::CliAction::ExitCleanly
    ) {
        return;
    }

    let event_loop = EventLoop::<TermEvent>::with_user_event().build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut registry = ThemeRegistry::load_from_dir(&themes_dir);
    let shared_active = registry.append_cybercore_themes();
    let initial_theme = registry
        .themes
        .iter()
        .find(|t| Some(t.name.as_str()) == shared_active.as_deref())
        .or_else(|| {
            registry
                .themes
                .iter()
                .find(|t| t.name == cyber_config.theme)
        })
        .or_else(|| registry.themes.first())
        .cloned();

    let event_proxy = event_loop.create_proxy();

    let mut app = CybertermApp {
        backend: None,
        event_proxy,
        clipboard: ClipboardManager::try_new(),
        theme_menu: ThemeMenuState {
            is_open: false,
            is_creating_mode: false,
            theme_name_input: String::new(),
            registry,
        },
        active_palette: [0; 16],
        active_bg: 0x000000,
        active_fg: 0xffffff,
        current_mods: ModifiersState::empty(),
        themes_directory: themes_dir,
        config_root,
        cyber_config,
        _tab_manager: tabs::TabContainer::new(),
    };

    if let Some(theme) = initial_theme {
        app.apply_theme(&theme);
    }

    let _ = event_loop.run_app(&mut app);
}
