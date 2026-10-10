// Cyberterm - Fully customizable terminal written in rust.
// The ultimate, scriptable, vintage-meets-modern terminal engine.

mod agent;
mod ai;
mod app;
mod blocks;
mod boxdraw;
mod changes;
mod cli;
mod clipboard;
mod config;
mod control;
mod danger;
mod find;
mod flight_log;
mod frame;
mod fuzzy;
mod graphics;
mod history;
mod input;
mod json_viewer;
mod layout;
mod layout_file;
mod mcp;
mod mux;
mod popular_themes;
mod procs;
mod renderer;
mod renderer_images;
mod rewind;
mod session;
mod shell;
mod theme;
mod theme_install;
mod tty_client;
mod ui;

use std::path::PathBuf;

use theme::ThemeRegistry;
use winit::event_loop::EventLoop;

fn main() {
    // Agent hooks run on every tool call: skip the start-up work below.
    let mut args = std::env::args();
    if args.nth(1).as_deref() == Some("+hook") {
        flight_log::run_hook(&args.collect::<Vec<_>>());
        return;
    }
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

    let cyber_config = match config::load_config(&config_root) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("cyberterm: config not loaded, using defaults: {e}");
            config::CyberConfig::default()
        }
    };

    let sys_args: Vec<String> = std::env::args().collect();
    let (startup_layout, startup_attach) =
        match cli::handle_arguments(sys_args, &config_root, cyber_config.clone()) {
            cli::CliAction::ExitCleanly => return,
            cli::CliAction::RunTerminal => (None, None),
            cli::CliAction::RunLayout(path) => {
                (Some(std::fs::canonicalize(&path).unwrap_or(path)), None)
            }
            cli::CliAction::RunAttach(target) => (None, Some(target)),
        };
    let mut cyber_config = cyber_config;
    if startup_attach.is_some() {
        // Attaching only makes sense with the daemon.
        cyber_config.daemon.enabled = true;
    }

    // TERM=alacritty when its terminfo is installed (our parser *is*
    // alacritty's, so that entry describes exactly what we support),
    // otherwise xterm-256color; COLORTERM=truecolor either way.
    alacritty_terminal::tty::setup_env();

    let mut registry = ThemeRegistry::load_from_dir(&themes_dir);
    let shared_active = registry.append_cybercore_themes();
    let shared_revision = cybercore::theme::ThemeCatalog::load()
        .map(|catalog| catalog.revision())
        .unwrap_or_default();
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
    if let Some(index) = initial_theme
        .as_ref()
        .and_then(|t| registry.themes.iter().position(|r| r.name == t.name))
    {
        registry.selected_index = index;
    }

    let event_loop = match EventLoop::<session::UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            eprintln!("CRITICAL: cannot connect to the display: {e}");
            std::process::exit(1);
        }
    };
    let mut app = app::App::new(
        event_loop.create_proxy(),
        app::Startup {
            config_root,
            themes_dir,
            config: cyber_config,
            registry,
            initial_theme,
            shared_theme_revision: shared_revision,
            layout: startup_layout,
            attach: startup_attach,
        },
    );
    let _ = event_loop.run_app(&mut app);
}
