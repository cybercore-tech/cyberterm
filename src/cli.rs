// Cyberterm CLI Options
// src/cli.rs
use crate::config::CyberConfig;
use std::fs;
// Remove PathBuf from the destructured import list
use std::path::Path; // Import your updated config layout

pub enum CliAction {
    RunTerminal,
    /// Run, opening this layout file (or project directory) instead of a
    /// single shell.
    RunLayout(std::path::PathBuf),
    /// Run, attached to a daemon session (`+attach [name]`).
    RunAttach(crate::app::AttachTarget),
    ExitCleanly,
}

pub fn handle_arguments(
    args: Vec<String>,
    config_root: &Path,
    mut current_config: CyberConfig,
) -> CliAction {
    if args.len() < 2 {
        return CliAction::RunTerminal;
    }

    let command = &args[1];

    match command.as_str() {
        // =========================================================================
        // MULTI-TIER THEME SCANNER INDEX
        // =========================================================================
        "+list-themes" => {
            let filter = args.get(2).map(|s| s.as_str()).unwrap_or("--all");
            println!("🌐 CYBERTERM THEMES CATALOG [{}]", filter.to_uppercase());
            println!("──────────────────────────────────────────────");
            let themes_dir = config_root.join("themes");
            let mut registry = crate::theme::ThemeRegistry::load_from_dir(themes_dir);
            registry.append_cybercore_themes();
            for theme in registry.themes {
                println!("  {} [{}]", theme.name, theme.category);
            }
            CliAction::ExitCleanly
        }

        "+set-theme" => {
            // Real Kitty-format themes: one flat `<name>.conf` file per
            // theme in the themes dir (the 12 built-ins, plus anything
            // dropped in straight from kovidgoyal/kitty-themes). Matches
            // `theme::ThemeRegistry`'s actual loader, not the old
            // category/folder/variant JSON hierarchy this command used to
            // target (which the registry no longer scans).
            let themes_dir = config_root.join("themes");
            let mut registry = crate::theme::ThemeRegistry::load_from_dir(&themes_dir);
            registry.append_cybercore_themes();

            let print_available = || {
                println!("🎨 Available themes:");
                for theme in &registry.themes {
                    println!("  {}", theme.name);
                }
            };

            match args.get(2) {
                Some(name) if registry.themes.iter().any(|t| &t.name == name) => {
                    println!("🎨 Setting active theme to: {}", name);
                    current_config.theme = name.clone();
                    if crate::config::save_config(config_root, &current_config).is_ok() {
                        println!("✨ Saved theme selection to config.");
                    } else {
                        eprintln!("❌ Error updating configuration.");
                    }
                    if cybercore::theme::ThemeCatalog::load()
                        .is_ok_and(|mut catalog| catalog.select(name).is_ok())
                    {
                        println!("🌐 Saved selection to the shared CYBERGRID catalog.");
                    }
                }
                Some(name) => {
                    eprintln!("❌ Error: No theme named '{}' found.", name);
                    print_available();
                }
                None => {
                    eprintln!("❌ Error: Missing theme name.");
                    eprintln!("   Usage: cyberterm +set-theme <name>");
                    print_available();
                }
            }
            CliAction::ExitCleanly
        }

        // =========================================================================
        // RAW THEME DESKTOP EDITOR HOOKS
        // =========================================================================
        "+edit-theme" => {
            let action = args.get(2).map(|s| s.as_str()).unwrap_or("--view-theme");
            let themes_base_dir = config_root.join("themes");

            match action {
                "--remove-theme" => {
                    if let (Some(cat), Some(folder), Some(variant)) =
                        (args.get(3), args.get(4), args.get(5))
                    {
                        let target_file = themes_base_dir
                            .join(cat)
                            .join(folder)
                            .join(format!("{}.json", variant));

                        if target_file.exists() {
                            if fs::remove_file(&target_file).is_ok() {
                                println!(
                                    "🗑️ Purged dynamic variant asset: {} ➔ {} ➔ {}.json",
                                    cat, folder, variant
                                );

                                if let Some(parent_folder) = target_file.parent() {
                                    if fs::read_dir(parent_folder)
                                        .map(|mut d| d.next().is_none())
                                        .unwrap_or(false)
                                    {
                                        let _ = fs::remove_dir(parent_folder);
                                        println!(
                                            "📁 Removed empty sub-genre folder: {:?}",
                                            parent_folder.file_name().unwrap()
                                        );
                                    }
                                }
                            } else {
                                eprintln!("❌ Error: Operational filesystem lock prevented deleting the target file.");
                            }
                        } else {
                            eprintln!("❌ Error: Target variant file not found.");
                        }
                    } else {
                        eprintln!("❌ Error: Missing hierarchy tokens.");
                        eprintln!("   Usage: cyberterm +edit-theme --remove-theme <category> <folder> <variant_name>");
                    }
                }

                "--view-theme" => {
                    if let (Some(cat), Some(folder), Some(variant)) =
                        (args.get(3), args.get(4), args.get(5))
                    {
                        let target_file = themes_base_dir
                            .join(cat)
                            .join(folder)
                            .join(format!("{}.json", variant));

                        if target_file.exists() {
                            let editor =
                                std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
                            println!("📖 Launching theme matrix using: {}...", editor);
                            if let Err(e) = std::process::Command::new(&editor)
                                .arg(&target_file)
                                .status()
                            {
                                eprintln!("❌ Failed to execute editor '{}': {}. Verify your $EDITOR env variable.", editor, e);
                            }
                        } else {
                            eprintln!("❌ Error: Specified theme variant file does not exist.");
                        }
                    } else {
                        eprintln!("❌ Error: Missing structural path targets.");
                    }
                }

                "--create-theme" => {
                    if let (Some(cat), Some(folder), Some(variant)) =
                        (args.get(3), args.get(4), args.get(5))
                    {
                        let destination_dir = themes_base_dir.join(cat).join(folder);
                        if let Err(e) = fs::create_dir_all(&destination_dir) {
                            eprintln!("❌ Error creating theme folders: {}", e);
                            return CliAction::ExitCleanly;
                        }

                        let target_file = destination_dir.join(format!("{}.json", variant));
                        if !target_file.exists() {
                            let default_json_template = r##"
                            {
                            "background": "#0a0a0a",
                            "foreground": "#ffffff",
                            "normal": {
                            "black":   "#000000",
                            "red":     "#ff5555",
                            "green":   "#50fa7b",
                            "yellow":  "#f1fa8c",
                            "blue":    "#bd93f9",
                            "magenta": "#ff79c6",
                            "cyan":    "#8be9fd",
                            "white":   "#f8f8f2"
                        },
                        "bright": {
                        "black":   "#6272a4",
                        "red":     "#ff6e6e",
                        "green":   "#69ff94",
                        "yellow":  "#ffffa5",
                        "blue":    "#d6acff",
                        "magenta": "#ff92df",
                        "cyan":    "#a4ffff",
                        "white":   "#ffffff"
                        }
                        }
                        "##;
                            if fs::write(&target_file, default_json_template).is_ok() {
                                println!("✨ Initialized fresh color template at target sub-path.");
                                let editor =
                                    std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
                                if let Err(e) = std::process::Command::new(&editor)
                                    .arg(&target_file)
                                    .status()
                                {
                                    eprintln!("❌ Failed to execute editor '{}': {}. Verify your $EDITOR env variable.", editor, e);
                                }
                            }
                        } else {
                            eprintln!("❌ Error: A theme file with that specific variant name already exists.");
                        }
                    } else {
                        eprintln!("❌ Error: Usage: cyberterm +edit-theme --create-theme <category> <folder> <variant_name>");
                    }
                }
                _ => eprintln!(
                    "❌ Unknown sub-command. Options: --create-theme, --view-theme, --remove-theme"
                ),
            }
            CliAction::ExitCleanly
        }

        // =========================================================================
        // TERMINAL BINDING LAYERS
        // =========================================================================
        "+list-termkeys" => {
            println!("🎹 KEYBINDINGS");
            println!("──────────────────────────────────────────────");
            let (bindings, _) = crate::input::bindings::Bindings::new(
                &current_config.keybindings,
                current_config.keyboard.leader.as_deref(),
            );
            for (combo, description) in bindings.describe() {
                println!("  {combo:<28} {description}");
            }
            println!();
            println!(
                "  Rebind or unbind under [keybindings] in {}",
                crate::config::config_path(config_root).display()
            );
            CliAction::ExitCleanly
        }

        "+layout" => {
            let path = args
                .get(2)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(crate::layout_file::DEFAULT_PATH));
            match crate::layout_file::load(&path) {
                Ok(_) => CliAction::RunLayout(path),
                Err(e) => {
                    eprintln!("❌ {e}");
                    std::process::exit(1);
                }
            }
        }

        "+daemon" => {
            let history = current_config.history.enabled.then(|| {
                crate::history::Recorder::start(crate::history::default_path()).map(|r| {
                    (
                        r,
                        crate::history::Policy::from_config(&current_config.history),
                    )
                })
            });
            if let Err(e) = crate::mux::server::run(history.flatten()) {
                eprintln!("❌ {e}");
                std::process::exit(1);
            }
            CliAction::ExitCleanly
        }

        "+attach" => {
            let mut target = crate::app::AttachTarget::default();
            let mut tty = false;
            for arg in &args[2..] {
                match arg.as_str() {
                    "--force" | "-f" => target.force = true,
                    "--tty" | "-t" => tty = true,
                    name => target.name = Some(name.to_string()),
                }
            }
            // Over SSH or on a console there's no display to open a window
            // on, so attach inside this terminal.
            let no_display = std::env::var_os("WAYLAND_DISPLAY").is_none()
                && std::env::var_os("DISPLAY").is_none();
            if tty || no_display {
                if let Err(e) = crate::tty_client::run(target, &current_config) {
                    eprintln!("❌ {e}");
                    std::process::exit(1);
                }
                CliAction::ExitCleanly
            } else {
                CliAction::RunAttach(target)
            }
        }

        "+json" => {
            if let Err(e) = crate::json_viewer::run(args.get(2).map(String::as_str)) {
                eprintln!("❌ {e}");
                std::process::exit(1);
            }
            CliAction::ExitCleanly
        }

        "+history" => {
            run_history(&args[2..]);
            CliAction::ExitCleanly
        }

        "+sessions" => {
            match crate::mux::client::DaemonClient::connect_existing()
                .and_then(|c| c.list_sessions())
            {
                Ok(sessions) if sessions.is_empty() => println!("No sessions."),
                Ok(sessions) => {
                    println!("  SESSION      PANES  STATE      AGE");
                    for s in sessions {
                        let age = match s.age {
                            a if a < 60 => format!("{a}s"),
                            a if a < 3600 => format!("{}m", a / 60),
                            a if a < 86400 => format!("{}h", a / 3600),
                            a => format!("{}d", a / 86400),
                        };
                        let state = if s.attached { "attached" } else { "detached" };
                        println!("  {:<12} {:>5}  {:<10} {}", s.name, s.panes, state, age);
                    }
                }
                Err(_) => println!("No session daemon running."),
            }
            CliAction::ExitCleanly
        }

        "+kill-session" => {
            let Some(name) = args.get(2) else {
                eprintln!("❌ Usage: cyberterm +kill-session <name>");
                std::process::exit(2);
            };
            match crate::mux::client::DaemonClient::connect_existing()
                .and_then(|c| c.kill_session(name))
            {
                Ok(()) => println!("Killed session {name}."),
                Err(e) => {
                    eprintln!("❌ {e}");
                    std::process::exit(1);
                }
            }
            CliAction::ExitCleanly
        }

        "+ctl" => {
            run_ctl(&args[2..]);
            CliAction::ExitCleanly
        }

        "+default-config" => {
            print!("{}", crate::config::default_config_toml());
            CliAction::ExitCleanly
        }

        "+shell-integration" => {
            let shell = args.get(2).cloned().unwrap_or_else(|| {
                std::env::var("SHELL")
                    .ok()
                    .and_then(|s| s.rsplit('/').next().map(str::to_string))
                    .unwrap_or_else(|| "bash".to_string())
            });
            match crate::shell::integration_script(&shell) {
                Some(script) => print!("{script}"),
                None => {
                    eprintln!("❌ No shell integration for '{shell}'. Supported: zsh, bash, fish");
                    eprintln!("   zsh:  eval \"$(cyberterm +shell-integration zsh)\"   (~/.zshrc)");
                    eprintln!(
                        "   bash: eval \"$(cyberterm +shell-integration bash)\"  (~/.bashrc)"
                    );
                    eprintln!(
                        "   fish: cyberterm +shell-integration fish | source     (config.fish)"
                    );
                }
            }
            CliAction::ExitCleanly
        }

        "+help" | "--help" | "-h" => {
            println!("cyberterm {}", env!("CARGO_PKG_VERSION"));
            println!();
            println!("  cyberterm                          launch the terminal");
            println!("  cyberterm +list-themes             list available themes");
            println!("  cyberterm +set-theme <name>        switch theme and save it");
            println!("  cyberterm +set-opacity --custom=N  set background opacity (0-1)");
            println!("  cyberterm +edit-theme ...          create/view/remove JSON themes");
            println!("  cyberterm +list-termkeys           show the effective keybindings");
            println!(
                "  cyberterm +default-config          print a commented config with every default"
            );
            println!(
                "  cyberterm +shell-integration [sh]  print the zsh/bash/fish integration script"
            );
            println!(
                "  cyberterm +ctl <method> [k=v ...]  control a running Cyberterm (see +ctl help)"
            );
            println!("  cyberterm +layout [file|dir]       open a layout (default .cyberterm/layout.toml)");
            println!("  cyberterm +attach [name]           reattach a daemon session (default: most recent)");
            println!("  cyberterm +sessions                list daemon sessions");
            println!("  cyberterm +history [words] [...]   search saved commands and their output (+history --help)");
            println!("  cyberterm +kill-session <name>     end a daemon session and its shells");
            println!("  cyberterm +daemon                  run the session daemon (normally started for you)");
            CliAction::ExitCleanly
        }

        "--version" | "-V" => {
            println!("cyberterm {}", env!("CARGO_PKG_VERSION"));
            CliAction::ExitCleanly
        }

        // =========================================================================
        // COMPOSITOR TRANSIT LAYER (OPACITY MODIFIER)
        // =========================================================================
        "+set-opacity" => {
            if let Some(opacity_arg) = args.get(2) {
                if let Some(clean_val) = opacity_arg.strip_prefix("--custom=") {
                    if let Ok(alpha) = clean_val.parse::<f32>() {
                        let checked_alpha = alpha.clamp(0.0, 1.0);
                        println!(
                            "🔮 Mutating compositor window target opacity constant: {}",
                            checked_alpha
                        );

                        current_config.opacity = checked_alpha;
                        let _ = crate::config::save_config(config_root, &current_config);
                    } else {
                        eprintln!(
                            "❌ Error: Invalid float value passed for target custom opacity."
                        );
                    }
                } else {
                    eprintln!("❌ Error: Opacity argument format must be: --custom=0.85");
                }
            } else {
                eprintln!("❌ Error: Missing opacity tracking target argument value. Format: --custom=0.85");
            }
            CliAction::ExitCleanly
        }

        _ => CliAction::RunTerminal,
    }
}

const CTL_HELP: &str = "\
cyberterm +ctl <method> [key=value ...] [--socket PATH]

Talks to a running Cyberterm over its control socket: the one this shell
runs in ($CYBERTERM_SOCKET), else the newest one that answers. Values that
parse as JSON are used as JSON (pane=3, paste=true); the rest are strings.

  ping                                    version and pid
  list-tabs                               tabs, their panes and focus
  list-panes                              panes: title, cwd, size, last exit code
  get-text    [pane=N] [lines=N]          screen text, or the last N lines
  send-text   [pane=N] text=... [paste=true]
  split       [pane=N] [direction=right|down|left|up] [cwd=DIR] [command=CMD]
  new-tab     [cwd=DIR] [command=CMD] [title=NAME]
  focus       pane=N
  close       [pane=N]
  zoom        [pane=N] [on=true|false]
  set-title   [tab=N] title=NAME
  resize      [pane=N] direction=... [amount=N]
  load-layout [path=FILE|DIR]             open a layout's tabs in this window
  history     [query=WORDS] [failed=true] [cwd=DIR] [limit=N] [output=true]

Without pane=N, methods act on the focused pane.";

fn run_ctl(args: &[String]) {
    let mut socket = None;
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--socket" {
            socket = iter.next().map(std::path::PathBuf::from);
        } else if let Some(path) = arg.strip_prefix("--socket=") {
            socket = Some(std::path::PathBuf::from(path));
        } else {
            rest.push(arg.clone());
        }
    }
    let Some(method) = rest
        .first()
        .filter(|m| !matches!(m.as_str(), "help" | "--help" | "-h"))
        .map(|m| m.replace('-', "_"))
    else {
        println!("{CTL_HELP}");
        return;
    };
    let params = match crate::control::params_from_args(&rest[1..]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(2);
        }
    };
    let Some(socket) = socket.or_else(crate::control::find_socket) else {
        eprintln!("❌ No running Cyberterm found (is [control] enabled?).");
        std::process::exit(1);
    };
    match crate::control::call(&socket, &method, params) {
        Ok(response) => match (response.result, response.error) {
            (_, Some(error)) => {
                eprintln!("❌ {} ({})", error.message, error.code);
                std::process::exit(1);
            }
            // Text comes out as text, so it pipes into other tools.
            (Some(result), None) if method == "get_text" => {
                println!("{}", result["text"].as_str().unwrap_or_default());
            }
            (Some(result), None) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
            (None, None) => {}
        },
        Err(e) => {
            eprintln!("❌ {}: {e}", socket.display());
            std::process::exit(1);
        }
    }
}

const HISTORY_HELP: &str = "\
cyberterm +history [words ...] [--failed] [--here] [-n N]
cyberterm +history --output <id>

Searches commands saved by shell integration (newest first). Words match
the command line or its output; all must match.

  --failed     only commands that exited non-zero
  --here       only commands run in the current directory
  -n N         show N results (default 20)
  --output ID  print the saved output of one command";

fn run_history(args: &[String]) {
    let mut query = crate::history::Query {
        limit: 20,
        ..crate::history::Query::default()
    };
    let mut words = Vec::new();
    let mut output_id = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HISTORY_HELP}");
                return;
            }
            "--failed" => query.failed_only = true,
            "--here" => {
                query.cwd = std::env::current_dir()
                    .ok()
                    .map(|d| d.to_string_lossy().into_owned())
            }
            "-n" => query.limit = iter.next().and_then(|n| n.parse().ok()).unwrap_or(20),
            "--output" => output_id = iter.next().and_then(|n| n.parse::<i64>().ok()),
            word => words.push(word.to_string()),
        }
    }
    if !words.is_empty() {
        query.text = Some(words.join(" "));
    }
    let path = crate::history::default_path();
    let store = match crate::history::Store::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    if let Some(id) = output_id {
        match store.output(id) {
            Ok(Some(text)) => println!("{text}"),
            _ => {
                eprintln!("❌ No saved command {id}.");
                std::process::exit(1);
            }
        }
        return;
    }
    let entries = match store.search(&query) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    };
    if entries.is_empty() {
        println!("No matching commands.");
        return;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    // Oldest of the page first, so the newest ends up next to the prompt.
    for e in entries.iter().rev() {
        let when = crate::history::format_time(e.started_ms);
        let status = match e.exit {
            Some(0) => "✓".to_string(),
            Some(code) => format!("✗{code}"),
            None => "?".to_string(),
        };
        let duration = crate::blocks::format_duration(e.finished_ms.saturating_sub(e.started_ms));
        let cwd = e
            .cwd
            .clone()
            .map(|c| match c.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => c,
            })
            .unwrap_or_default();
        println!(
            "{:>6}  {when}  {status:<4} {duration:>6}  {cwd}  $ {}",
            e.id, e.command
        );
        if let Some(snippet) = e.snippet.as_deref().filter(|s| !s.is_empty()) {
            let line = snippet.replace('\n', " ⏎ ");
            println!("{:>8}{}", "", line.chars().take(110).collect::<String>());
        }
    }
}
