// src/config.rs
//
// `~/.config/cyberterm/cyber_config.toml`. Every section and field is
// optional (`#[serde(default)]` all the way down), so the two-line config
// older builds wrote (`theme` + `opacity`) still loads unchanged and only
// overrides what it names. The file is re-read whenever its mtime changes
// (`app.rs` polls it once a second), so edits apply without a restart.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const CONFIG_FILE: &str = "cyber_config.toml";

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct CyberConfig {
    pub theme: String,
    pub opacity: f32,
    pub font: FontConfig,
    pub window: WindowConfig,
    pub cursor: CursorConfig,
    pub scrollback: ScrollbackConfig,
    pub mouse: MouseConfig,
    pub clipboard: ClipboardConfig,
    pub bell: BellConfig,
    pub shell: ShellConfig,
    pub keyboard: KeyboardConfig,
    pub tabs: TabsConfig,
    pub splits: SplitsConfig,
    pub control: ControlConfig,
    pub daemon: DaemonConfig,
    pub history: HistoryConfig,
    pub notify: NotifyConfig,
    pub agents: AgentsConfig,
    pub ai: AiConfig,
    pub danger: DangerConfig,
    pub ports: PortsConfig,
    pub rewind: RewindConfig,
    pub lua: LuaConfig,
    pub blocks: BlocksConfig,
    pub links: LinksConfig,
    /// `"ctrl+shift+c" = "copy"` style overrides layered on top of the
    /// built-in bindings (`input::bindings`). Map a combo to `"none"` to
    /// unbind a default and let the keystroke reach the shell instead.
    pub keybindings: BTreeMap<String, String>,
}

impl Default for CyberConfig {
    fn default() -> Self {
        Self {
            theme: "synthwave_84".to_string(),
            opacity: 0.90,
            font: FontConfig::default(),
            window: WindowConfig::default(),
            cursor: CursorConfig::default(),
            scrollback: ScrollbackConfig::default(),
            mouse: MouseConfig::default(),
            clipboard: ClipboardConfig::default(),
            bell: BellConfig::default(),
            shell: ShellConfig::default(),
            keyboard: KeyboardConfig::default(),
            tabs: TabsConfig::default(),
            splits: SplitsConfig::default(),
            control: ControlConfig::default(),
            daemon: DaemonConfig::default(),
            history: HistoryConfig::default(),
            notify: NotifyConfig::default(),
            agents: AgentsConfig::default(),
            ai: AiConfig::default(),
            danger: DangerConfig::default(),
            ports: PortsConfig::default(),
            rewind: RewindConfig::default(),
            lua: LuaConfig::default(),
            blocks: BlocksConfig::default(),
            links: LinksConfig::default(),
            keybindings: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct FontConfig {
    /// Primary monospace family, resolved through fontconfig.
    pub family: String,
    /// Size in points (converted at 96 DPI times the window scale factor,
    /// the same convention Ghostty, Kitty and Alacritty use).
    pub size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    /// Families tried, in order, for glyphs the primary font lacks.
    pub fallback: Vec<String>,
    /// Shape ASCII runs with full OpenType shaping, enabling programming
    /// ligatures in fonts that have them. Off by default: most terminal
    /// users expect `!=` to stay two cells that look like two cells.
    pub ligatures: bool,
    /// Draw bold text in the bright variant of ANSI colors 0-7, the old
    /// xterm convention some color schemes are designed around.
    pub bold_is_bright: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "JetBrainsMono Nerd Font".to_string(),
            size: 9.0,
            line_height: 1.25,
            fallback: vec![
                "Symbols Nerd Font Mono".to_string(),
                "Noto Color Emoji".to_string(),
            ],
            ligatures: false,
            bold_is_bright: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct WindowConfig {
    /// Inner padding around the cell grid, in logical pixels.
    pub padding: f32,
    /// Initial window size in logical pixels.
    pub width: u32,
    pub height: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            padding: 4.0,
            width: 900,
            height: 600,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CursorShapeConfig {
    Block,
    Beam,
    Underline,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct CursorConfig {
    /// Default shape; programs can still change it with DECSCUSR.
    pub style: CursorShapeConfig,
    pub blinking: bool,
    pub blink_interval_ms: u64,
    /// Draw a hollow block while the window doesn't have focus.
    pub unfocused_hollow: bool,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            style: CursorShapeConfig::Block,
            blinking: false,
            blink_interval_ms: 530,
            unfocused_hollow: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct ScrollbackConfig {
    /// Lines of history kept per terminal.
    pub lines: usize,
    /// Lines scrolled per mouse-wheel notch.
    pub multiplier: f32,
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self {
            lines: 10_000,
            multiplier: 3.0,
        }
    }
}

/// Where a finished mouse selection is copied automatically.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase", try_from = "CopyOnSelectRaw")]
pub enum CopyOnSelect {
    Off,
    /// The primary selection (middle-click paste).
    Primary,
    /// The regular clipboard (Ctrl+V / Super+V elsewhere).
    Clipboard,
    Both,
}

/// Older configs wrote `copy_on_select = true/false`.
#[derive(Deserialize)]
#[serde(untagged)]
enum CopyOnSelectRaw {
    Bool(bool),
    Name(String),
}

impl TryFrom<CopyOnSelectRaw> for CopyOnSelect {
    type Error = String;
    fn try_from(raw: CopyOnSelectRaw) -> Result<Self, String> {
        Ok(match raw {
            CopyOnSelectRaw::Bool(true) => Self::Primary,
            CopyOnSelectRaw::Bool(false) => Self::Off,
            CopyOnSelectRaw::Name(n) => match n.as_str() {
                "off" => Self::Off,
                "primary" => Self::Primary,
                "clipboard" => Self::Clipboard,
                "both" => Self::Both,
                other => {
                    return Err(format!(
                        "copy_on_select must be off, primary, clipboard or both, not `{other}`"
                    ))
                }
            },
        })
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct MouseConfig {
    /// Where finished mouse selections are copied automatically.
    pub copy_on_select: CopyOnSelect,
    /// Hide the mouse pointer while typing until it moves again.
    pub hide_while_typing: bool,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            copy_on_select: CopyOnSelect::Primary,
            hide_while_typing: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Osc52Mode {
    Disabled,
    Copy,
    Paste,
    CopyPaste,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct ClipboardConfig {
    /// What programs may do with the clipboard through OSC 52. Reading
    /// (`paste`) lets any program running in the terminal, including one
    /// on a remote host over SSH, read your clipboard, so it is off by
    /// default.
    pub osc52: Osc52Mode,
    /// Replace box-drawing characters with ASCII (`│` -> `|`) when copying.
    pub clean_box_drawing: bool,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            osc52: Osc52Mode::Copy,
            clean_box_drawing: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct BellConfig {
    /// Briefly flash the terminal on BEL.
    pub visual: bool,
    /// Mark the window urgent on BEL while it is unfocused.
    pub urgent: bool,
    /// Optional command run (via `sh -c`) on every BEL.
    pub command: Option<String>,
}

impl Default for BellConfig {
    fn default() -> Self {
        Self {
            visual: true,
            urgent: true,
            command: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
#[serde(default)]
pub struct ShellConfig {
    /// Program to run; defaults to `$SHELL`, then `/bin/bash`.
    pub program: Option<String>,
    pub args: Vec<String>,
    /// `TERM` for the shell. Defaults to `alacritty` when that terminfo
    /// entry is installed (Cyberterm uses alacritty's parser, so it
    /// describes exactly what's supported), else `xterm-256color`. Set
    /// `xterm-256color` if remote hosts you SSH into lack the entry.
    pub term: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct KeyboardConfig {
    /// Let programs opt into the Kitty keyboard protocol (unambiguous
    /// modifiers, key release events). Programs that don't ask for it see
    /// the classic xterm encoding either way.
    pub kitty_protocol: bool,
    /// A tmux-style prefix key (e.g. `"ctrl+b"`). Press it, then a key
    /// from the leader table (`"leader+%" = "split_right"`, ...). Off by
    /// default so no key is taken from the shell unless asked for.
    pub leader: Option<String>,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            kitty_protocol: true,
            leader: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TabBarMode {
    /// Shown while there is more than one tab.
    Auto,
    Always,
    Never,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct TabsConfig {
    pub bar: TabBarMode,
}

impl Default for TabsConfig {
    fn default() -> Self {
        Self {
            bar: TabBarMode::Auto,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct SplitsConfig {
    /// How much to dim panes that don't have focus, 0.0 (off) - 1.0.
    pub inactive_dim: f32,
    /// Splitting a pane that's in an interactive `ssh` session opens the
    /// new pane on the same host (and directory, when the remote shell
    /// reports it).
    pub follow_ssh: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct ControlConfig {
    /// Listen on a user-only Unix socket so `cyberterm +ctl` and scripts
    /// can list, read, type into and arrange panes. Read at startup.
    pub enabled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct DaemonConfig {
    /// Run shells in the session daemon (`cyberterm +daemon`, started
    /// automatically) instead of inside the window: closing or crashing a
    /// window then only detaches it, and its tabs and shells come back on
    /// the next attach. Read at startup.
    pub enabled: bool,
    /// When a window opens and a detached session exists, attach to the
    /// most recent one instead of starting a new session.
    pub reattach: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reattach: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct HistoryConfig {
    /// Save every finished command (with its output, secrets redacted) to
    /// a searchable database. Needs shell integration. Read at startup.
    pub enabled: bool,
    /// Output kept per command; longer output keeps its start and end.
    pub max_output_kb: usize,
    /// Regexes; matching command lines aren't saved. Lines starting with a
    /// space never are.
    pub ignore: Vec<String>,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_output_kb: 256,
            ignore: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct NotifyConfig {
    /// Desktop notification when a command that ran at least this many
    /// seconds finishes while you're not looking at it. 0 turns it off.
    pub long_command_seconds: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            long_command_seconds: 10,
        }
    }
}

/// What an AI agent connected through `cyberterm +mcp` may do without
/// asking.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentPolicy {
    /// Ask in the window each time (with "allow for this pane" to stop
    /// asking).
    Ask,
    Allow,
    Deny,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct AgentsConfig {
    /// Accept requests from AI agents (`cyberterm +mcp`) at all.
    pub enabled: bool,
    /// Reading a pane's text and its recent commands.
    pub read: AgentPolicy,
    /// Typing into a pane and running commands in new panes. `allow`
    /// lets agents act without asking -- only for agents you'd hand your
    /// keyboard to.
    pub write: AgentPolicy,
    /// Record every agent request in `$XDG_STATE_HOME/cyberterm/agents.log`.
    pub audit_log: bool,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            read: AgentPolicy::Ask,
            write: AgentPolicy::Ask,
            audit_log: true,
        }
    }
}

/// Lua scripting (src/app/lua.rs).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct LuaConfig {
    pub enabled: bool,
    /// The script; empty means init.lua next to this config.
    pub script: String,
}

impl Default for LuaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            script: String::new(),
        }
    }
}

/// Rewind: each pane records recent output to step back through.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct RewindConfig {
    pub enabled: bool,
    /// Output kept per pane, in KiB (the oldest is folded away).
    pub buffer_kb: usize,
}

impl Default for RewindConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            buffer_kb: 2048,
        }
    }
}

/// Live ports: chips for the TCP ports a pane's programs listen on.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct PortsConfig {
    pub enabled: bool,
    /// What clicking a chip opens; `{port}` is replaced.
    pub url: String,
}

impl Default for PortsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "http://localhost:{port}".into(),
        }
    }
}

/// Danger mode: panes on production hosts or running as root turn red, and
/// risky commands typed there ask before they run.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct DangerConfig {
    pub enabled: bool,
    /// SSH destinations that count as dangerous (wildcards, any case).
    pub hosts: Vec<String>,
    /// A root shell (sudo -i, su) counts as dangerous.
    pub root: bool,
    /// Working directories that count as dangerous (`~/` allowed).
    pub paths: Vec<String>,
    /// Which commands need a second Enter in a dangerous pane: "dangerous"
    /// (recursive deletes, force-push, DROP TABLE, ...), "caution" (also
    /// sudo, overwrites, kills) or "off".
    pub confirm: String,
    /// How strongly a dangerous pane is tinted red (0 to 0.3).
    pub tint: f32,
}

impl Default for DangerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hosts: vec!["*prod*".into()],
            root: true,
            paths: Vec::new(),
            confirm: "dangerous".into(),
            tint: 0.06,
        }
    }
}

/// Outbound AI (`src/ai.rs`): Explain this error, plain English to a
/// command.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct AiConfig {
    /// "anthropic", "openai" (any OpenAI-compatible API, Ollama included)
    /// or "command"; empty turns AI off.
    pub provider: String,
    /// Empty: claude-opus-5-5 for anthropic; required for openai.
    pub model: String,
    /// Empty: the provider's public API. For Ollama,
    /// "http://localhost:11434/v1".
    pub base_url: String,
    /// The environment variable holding the API key. Empty:
    /// ANTHROPIC_API_KEY / OPENAI_API_KEY. Local servers need none.
    pub api_key_env: String,
    /// For provider = "command": the program and its arguments; the prompt
    /// goes to its stdin, e.g. ["claude", "-p"].
    pub command: Vec<String>,
    /// Anthropic only: how hard the model thinks (low, medium, high).
    pub effort: String,
    pub timeout_seconds: u64,
    /// Most characters of command output sent (the end is kept).
    pub max_context_chars: usize,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_key_env: String::new(),
            command: Vec::new(),
            effort: "low".into(),
            timeout_seconds: 120,
            max_context_chars: 12_000,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct BlocksConfig {
    /// Draw a colored bar beside each command block (green ok, red failed,
    /// yellow running) and its exit code and duration. Needs shell
    /// integration.
    pub decorations: bool,
}

impl Default for BlocksConfig {
    fn default() -> Self {
        Self { decorations: true }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
#[serde(default)]
pub struct LinksConfig {
    /// How to open `path:line` references, with `{file}`, `{line}` and
    /// `{col}` filled in (e.g. `"nvim +{line} {file}"`). Unset: derived
    /// from $VISUAL / $EDITOR.
    pub editor: Option<String>,
    /// Run `editor` in a new pane (terminal editors) instead of as its own
    /// window (GUI editors).
    pub editor_in_pane: Option<bool>,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for SplitsConfig {
    fn default() -> Self {
        Self {
            inactive_dim: 0.25,
            follow_ssh: true,
        }
    }
}

pub fn config_path(config_root: &Path) -> PathBuf {
    config_root.join(CONFIG_FILE)
}

pub fn load_config(config_root: &Path) -> Result<CyberConfig, Box<dyn std::error::Error>> {
    // Must match `save_config`'s filename below -- these disagreed
    // ("config.toml" here vs "cyber_config.toml" there) since day one,
    // silently discarding every setting any `+set-*` CLI command or the
    // in-app theme menu ever saved, since `load_config` always found
    // nothing and fell through to `CyberConfig::default()`.
    let config_path = config_path(config_root);

    if !config_path.exists() {
        return Ok(CyberConfig::default());
    }

    let content = fs::read_to_string(config_path)?;
    Ok(toml::from_str(&content)?)
}

/// Last-modified time of the config file, used to notice edits.
pub fn config_mtime(config_root: &Path) -> Option<SystemTime> {
    fs::metadata(config_path(config_root))
        .and_then(|m| m.modified())
        .ok()
}

/// Writes the fields the CLI and theme menu change (`theme`, `opacity`)
/// back to disk, editing the existing document in place so the user's own
/// comments, ordering and other sections survive.
pub fn save_config(config_root: &Path, config: &CyberConfig) -> Result<(), std::io::Error> {
    let path = config_path(config_root);
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing.parse().map_err(std::io::Error::other)?;
    doc["theme"] = toml_edit::value(config.theme.clone());
    doc["opacity"] = toml_edit::value((config.opacity as f64 * 100.0).round() / 100.0);
    fs::write(path, doc.to_string())
}

/// A fully commented config with every default spelled out, printed by
/// `cyberterm +default-config`.
pub fn default_config_toml() -> &'static str {
    include_str!("../assets/default-config.toml")
}

pub fn initialize_cyberterm_directories() -> Result<PathBuf, std::io::Error> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let config_dir = PathBuf::from(home).join(".config").join("cyberterm");
    let themes_dir = config_dir.join("themes");

    fs::create_dir_all(&themes_dir)?;

    let config_file = config_path(&config_dir);
    if !config_file.exists() {
        let default_cfg = CyberConfig::default();
        let _ = save_config(&config_dir, &default_cfg);
    }

    initialize_builtin_themes(&themes_dir)?;
    Ok(config_dir)
}

fn initialize_builtin_themes(themes_dir: &std::path::Path) -> Result<(), std::io::Error> {
    let themes = get_cyber_theme_payloads();
    for (filename, content) in themes {
        // Real Kitty theme file extension -- these are genuine Kitty-conf-syntax
        // files, so anything pulled straight from kovidgoyal/kitty-themes drops
        // in next to these with zero conversion.
        let theme_path = themes_dir.join(format!("{}.conf", filename));
        if !theme_path.exists() {
            fs::write(theme_path, content.trim_start())?;
        }
    }
    Ok(())
}

/// Real Kitty terminal theme syntax: flat, space-separated `key value` lines,
/// `#`-prefixed comments, no `=`. Verified directly against
/// kovidgoyal/kitty-themes (e.g. Dracula.conf) before converting these.
fn get_cyber_theme_payloads() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "dracula_cyber",
            r#"
# Dracula (Cyberpunk Remix) - Deep violet background, amplified pinks/purples
background #1e1f29
foreground #f8f8f2
cursor #f8f8f2

color0 #1e1f29
color1 #ff5555
color2 #50fa7b
color3 #f1fa8c
color4 #bd93f9
color5 #ff79c6
color6 #8be9fd
color7 #f8f8f2
color8 #6272a4
color9 #ff6e6e
color10 #69ff94
color11 #ffffa5
color12 #d6acff
color13 #ff92df
color14 #a4ffff
color15 #ffffff
"#,
        ),
        (
            "synthwave_84",
            r#"
# Synthwave '84 - The definitive Outrun aesthetic (Deep purple & pure laser grids)
background #261a30
foreground #b3b1b6
cursor #b3b1b6

color0 #261a30
color1 #fe4450
color2 #72f1b8
color3 #fede5d
color4 #03edf6
color5 #f92aad
color6 #03edf6
color7 #b3b1b6
color8 #493b61
color9 #ff6b77
color10 #96f5cd
color11 #ffe484
color12 #5ef2f8
color13 #fb5cb8
color14 #5ef2f8
color15 #ffffff
"#,
        ),
        (
            "cyberpunk_2077",
            r#"
# Cyberpunk 2077 - Radioactive yellow highlights mixed with toxic cyan
background #000000
foreground #bcbcbc
cursor #bcbcbc

color0 #000000
color1 #ff0055
color2 #00ffaa
color3 #f3e600
color4 #00bfff
color5 #de00fe
color6 #00ffff
color7 #bcbcbc
color8 #222222
color9 #ff3377
color10 #33ffbb
color11 #ffff55
color12 #33ccff
color13 #e533ff
color14 #55ffff
color15 #ffffff
"#,
        ),
        (
            "nord_neon",
            r#"
# Nord Neon - Standard arctic cold shattered by electric auroras
background #1a1e24
foreground #e5e9f0
cursor #e5e9f0

color0 #1a1e24
color1 #bf616a
color2 #a3be8c
color3 #ebcb8b
color4 #81a1c1
color5 #b48ead
color6 #88c0d0
color7 #e5e9f0
color8 #4c566a
color9 #ff4a5a
color10 #50fa7b
color11 #f1fa8c
color12 #00bfff
color13 #ff79c6
color14 #00ffff
color15 #ffffff
"#,
        ),
        (
            "gruvbox_cyber",
            r#"
# Gruvbox Cyber - Hardened high-contrast industrial pitch-black and chemical orange
background #111111
foreground #ebdbb2
cursor #ebdbb2

color0 #111111
color1 #fb4934
color2 #b8bb26
color3 #fabd2f
color4 #83a598
color5 #d3869b
color6 #8ec07c
color7 #ebdbb2
color8 #665c54
color9 #ff3333
color10 #00ffaa
color11 #fe4450
color12 #03edf6
color13 #f92aad
color14 #5ef2f8
color15 #ffffff
"#,
        ),
        (
            "darcula_glow",
            r#"
# Darcula Glow - IDE classic backend dialed up into a bright wireframe grid
background #1c1c1c
foreground #a9b7c6
cursor #a9b7c6

color0 #1c1c1c
color1 #ff4d4d
color2 #a5e177
color3 #ffcc00
color4 #9876aa
color5 #cc7832
color6 #299999
color7 #a9b7c6
color8 #606060
color9 #ff6b68
color10 #bdfa8c
color11 #e0db42
color12 #b38fcc
color13 #e08846
color14 #3fb3b3
color15 #ffffff
"#,
        ),
        (
            "monokai_overdrive",
            r#"
# Monokai Overdrive - Liquid chemical dyes on absolute pitch-black
background #0a0a0a
foreground #f8f8f2
cursor #f8f8f2

color0 #0a0a0a
color1 #f92672
color2 #a6e22e
color3 #f4bf75
color4 #66d9ef
color5 #ae81ff
color6 #a1efe4
color7 #f8f8f2
color8 #49483e
color9 #ff0055
color10 #00ff00
color11 #ffea00
color12 #00bfff
color13 #de00fe
color14 #00ffff
color15 #ffffff
"#,
        ),
        (
            "toxic_waste",
            r#"
# Toxic Waste - Acid green dominant terminal with hazardous warning signs
background #0d0f0d
foreground #d0ffd0
cursor #d0ffd0

color0 #0d0f0d
color1 #ff2a2a
color2 #39ff14
color3 #ffff00
color4 #00e5ff
color5 #bd00ff
color6 #00ffaa
color7 #d0ffd0
color8 #253025
color9 #ff5555
color10 #69ff94
color11 #ffffa5
color12 #a4ffff
color13 #ff92df
color14 #a4ffff
color15 #ffffff
"#,
        ),
        (
            "tokyo_grid",
            r#"
# Tokyo Grid - Deep midnight-blue backplane illuminated by neon shinjuku signs
background #16161e
foreground #a9b1d6
cursor #a9b1d6

color0 #16161e
color1 #f7768e
color2 #9ece6a
color3 #e0af68
color4 #7aa2f7
color5 #bb9af7
color6 #7dcfff
color7 #a9b1d6
color8 #414868
color9 #ff4499
color10 #00ffaa
color11 #ffcc00
color12 #03edf6
color13 #f92aad
color14 #00ffff
color15 #ffffff
"#,
        ),
        (
            "vaporwave_85",
            r#"
# Vaporwave '85 - Soft pastel pink and cyan amplified into striking neon gradients
background #180a2b
foreground #e2dbec
cursor #e2dbec

color0 #180a2b
color1 #ff71ce
color2 #01cdfe
color3 #05ffa1
color4 #b967ff
color5 #fffb96
color6 #01cdfe
color7 #e2dbec
color8 #3d1e6d
color9 #ff9de2
color10 #5ef2f8
color11 #9effd7
color12 #d6acff
color13 #ffffcc
color14 #5ef2f8
color15 #ffffff
"#,
        ),
        (
            "solarized_glitch",
            r#"
# Solarized Glitch - Classic solarized structure but fried with toxic high-voltage contrast
background #002b36
foreground #eee8d5
cursor #eee8d5

color0 #002b36
color1 #dc322f
color2 #859900
color3 #b58900
color4 #268bd2
color5 #d33682
color6 #2aa198
color7 #eee8d5
color8 #073642
color9 #ff4444
color10 #00ff00
color11 #f3e600
color12 #00bfff
color13 #ff0055
color14 #00ffff
color15 #ffffff
"#,
        ),
        (
            "oblivion_core",
            r#"
# Oblivion Core - Dark stealth matte black base accented solely by hot magmas
background #050505
foreground #e0e0e0
cursor #e0e0e0

color0 #050505
color1 #ff3700
color2 #ff8800
color3 #ffd000
color4 #990000
color5 #ff0055
color6 #ffaa00
color7 #e0e0e0
color8 #221111
color9 #ff5533
color10 #ffa044
color11 #ffe066
color12 #cc0000
color13 #ff3377
color14 #ffbb33
color15 #ffffff
"#,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_two_line_config_still_loads() {
        let cfg: CyberConfig =
            toml::from_str("theme = \"dracula_cyber\"\nopacity = 0.8\n").unwrap();
        assert_eq!(cfg.theme, "dracula_cyber");
        assert_eq!(cfg.font, FontConfig::default());
        assert_eq!(cfg.scrollback.lines, 10_000);
    }

    #[test]
    fn partial_sections_merge_with_defaults() {
        let cfg: CyberConfig = toml::from_str(
            "[font]\nsize = 12.5\n[cursor]\nstyle = \"beam\"\n[clipboard]\nosc52 = \"copy-paste\"\n[keybindings]\n\"ctrl+shift+c\" = \"none\"\n",
        )
        .unwrap();
        assert_eq!(cfg.font.size, 12.5);
        assert_eq!(cfg.font.family, FontConfig::default().family);
        assert_eq!(cfg.cursor.style, CursorShapeConfig::Beam);
        assert_eq!(cfg.clipboard.osc52, Osc52Mode::CopyPaste);
        assert_eq!(cfg.keybindings["ctrl+shift+c"], "none");
    }

    #[test]
    fn copy_on_select_accepts_old_booleans_and_new_names() {
        let parse = |v: &str| {
            toml::from_str::<CyberConfig>(&format!("[mouse]\ncopy_on_select = {v}\n"))
                .map(|c| c.mouse.copy_on_select)
        };
        assert_eq!(parse("true").unwrap(), CopyOnSelect::Primary);
        assert_eq!(parse("false").unwrap(), CopyOnSelect::Off);
        assert_eq!(parse("\"both\"").unwrap(), CopyOnSelect::Both);
        assert!(parse("\"sometimes\"").is_err());
    }

    #[test]
    fn shipped_default_config_parses_to_the_defaults() {
        let cfg: CyberConfig = toml::from_str(default_config_toml()).unwrap();
        assert_eq!(cfg, CyberConfig::default());
    }

    #[test]
    fn save_preserves_user_comments_and_sections() {
        let dir = std::env::temp_dir().join(format!("cyberterm-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            config_path(&dir),
            "# mine\ntheme = \"old\"\n\n[font]\nsize = 11.0 # keep\n",
        )
        .unwrap();
        let cfg = CyberConfig {
            theme: "new".into(),
            ..CyberConfig::default()
        };
        save_config(&dir, &cfg).unwrap();
        let written = fs::read_to_string(config_path(&dir)).unwrap();
        assert!(written.contains("# mine"));
        assert!(written.contains("theme = \"new\""));
        assert!(written.contains("size = 11.0 # keep"));
        fs::remove_dir_all(dir).unwrap();
    }
}
