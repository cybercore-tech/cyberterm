// src/theme.rs
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub author: String,
    pub category: String,
    pub background: String,
    pub foreground: String,
    pub cursor: String,

    // Alacritty needs u32 internally, but we pull them as Hex strings from JSON
    #[serde(skip)]
    pub colors: [u32; 16],

    // Temporary binding array for Serde during initial parsing
    #[serde(rename = "colors")]
    pub raw_colors: Vec<String>,
}

pub struct ThemeRegistry {
    pub themes: Vec<Theme>,
    pub selected_index: usize,
}

impl ThemeRegistry {
    /// Append shared CYBERGRID themes, adapting semantic colors to ANSI slots.
    /// Existing Kitty and Cyberterm-local themes stay available alongside them.
    pub fn append_cybercore_themes(&mut self) -> Option<String> {
        let catalog = cybercore::theme::ThemeCatalog::load().ok()?;
        let active = catalog.active_id().to_string();
        for (id, entry) in catalog.iter() {
            let palette = entry.document.palette_for(catalog.active_appearance());
            let slots = [
                &palette.bg,
                &palette.red,
                &palette.acid_green,
                &palette.orange,
                &palette.purple,
                &palette.hot_pink,
                &palette.cyan,
                &palette.white,
                &palette.muted,
                &palette.red,
                &palette.acid_green,
                &palette.orange,
                &palette.purple,
                &palette.hot_pink,
                &palette.cyan,
                &palette.white,
            ];
            let raw_colors: Vec<String> = slots.iter().map(|color| format!("#{color}")).collect();
            let mut colors = [0u32; 16];
            for (index, value) in raw_colors.iter().enumerate() {
                colors[index] = u32::from_str_radix(value.trim_start_matches('#'), 16).unwrap_or(0);
            }
            let theme = Theme {
                name: id.to_string(),
                author: entry.document.metadata.author.clone(),
                category: entry.document.metadata.family.clone(),
                background: format!("#{}", palette.bg),
                foreground: format!("#{}", palette.white),
                cursor: format!("#{}", palette.acid_green),
                colors,
                raw_colors,
            };
            if let Some(existing) = self.themes.iter_mut().find(|theme| theme.name == id) {
                *existing = theme;
            } else {
                self.themes.push(theme);
            }
        }
        self.themes.sort_by(|a, b| a.name.cmp(&b.name));
        Some(active)
    }

    /// Loads every theme this box actually has, in both real shapes that
    /// exist on disk:
    /// 1. Flat `*.conf` files directly in `base_dir` -- real Kitty terminal
    ///    theme syntax (space-separated `key value` lines), which is what
    ///    the 12 built-in curated palettes `config::initialize_builtin_themes`
    ///    now seeds. Because this is Kitty's own real format, any theme
    ///    pulled straight from kovidgoyal/kitty-themes drops in here
    ///    unmodified too.
    /// 2. Nested `category/folder/*.json` files -- the user-created/
    ///    dynamic themes from `+edit-theme`/the in-app "create new" flow.
    pub fn load_from_dir<P: AsRef<Path>>(base_dir: P) -> Self {
        let base_dir = base_dir.as_ref();
        let mut themes = Vec::new();

        if let Ok(entries) = fs::read_dir(base_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "conf") {
                    if let Ok(theme) = Self::parse_theme_file(&path) {
                        themes.push(theme);
                    }
                }
            }
        }

        // Use a recursive closure or manual deep iteration loop to exhaust the new tree layout
        if let Ok(categories) = fs::read_dir(base_dir) {
            for cat_entry in categories.flatten() {
                if cat_entry.path().is_dir() {
                    if let Ok(sub_folders) = fs::read_dir(cat_entry.path()) {
                        for folder_entry in sub_folders.flatten() {
                            if folder_entry.path().is_dir() {
                                if let Ok(files) = fs::read_dir(folder_entry.path()) {
                                    for file_entry in files.flatten() {
                                        let file_path = file_entry.path();
                                        if file_path.is_file()
                                            && file_path.extension().is_some_and(|e| e == "json")
                                        {
                                            if let Ok(theme) = Self::parse_json_theme(&file_path) {
                                                themes.push(theme);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        ThemeRegistry {
            themes,
            selected_index: 0,
        }
    }

    /// Parses a real Kitty terminal theme file: flat, space-separated
    /// `key value` lines (`background #hex`, `foreground #hex`,
    /// `cursor #hex`, `color0 #hex` ... `color15 #hex`), `#`-prefixed
    /// comment lines and blank lines allowed. This is Kitty's own actual
    /// `.conf` syntax (verified against kovidgoyal/kitty-themes), not a
    /// bespoke format -- any file from that repo parses here unmodified.
    /// Unrecognized keys (`selection_background`, `url_color`, tab/border
    /// colors, etc.) are simply ignored rather than rejected, since a real
    /// Kitty theme file commonly carries more keys than this terminal uses.
    fn parse_theme_file(path: &Path) -> Result<Theme, Box<dyn std::error::Error>> {
        let raw = fs::read_to_string(path)?;
        let mut colors = [0u32; 16];
        let mut raw_colors: Vec<String> = vec![String::new(); 16];
        let mut background: Option<String> = None;
        let mut foreground: Option<String> = None;
        let mut cursor: Option<String> = None;

        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();

            if let Some(index_str) = key.strip_prefix("color") {
                let Ok(index) = index_str.parse::<usize>() else {
                    continue;
                };
                if index >= 16 {
                    continue;
                }
                let hex = value.trim_start_matches('#');
                if let Ok(val) = u32::from_str_radix(hex, 16) {
                    colors[index] = val;
                    raw_colors[index] = value.to_string();
                }
                continue;
            }

            match key {
                "background" => background = Some(value.to_string()),
                "foreground" => foreground = Some(value.to_string()),
                "cursor" => cursor = Some(value.to_string()),
                _ => {} // real Kitty files carry many more keys we don't use yet
            }
        }

        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".to_string());

        // Kitty theme files always define background/foreground/cursor
        // explicitly, but fall back to the standard ANSI convention
        // (color0 = background, color7 = default foreground) for any
        // file that omits them.
        let background = background.unwrap_or_else(|| raw_colors[0].clone());
        let foreground = foreground.unwrap_or_else(|| raw_colors[7].clone());
        let cursor = cursor.unwrap_or_else(|| foreground.clone());

        Ok(Theme {
            name,
            author: "built-in".to_string(),
            category: "built-in".to_string(),
            background,
            foreground,
            cursor,
            colors,
            raw_colors,
        })
    }

    /// Internal asset parser mapping hex strings safely over into u32 bits
    fn parse_json_theme(path: &Path) -> Result<Theme, Box<dyn std::error::Error>> {
        let raw_json = fs::read_to_string(path)?;
        let mut theme: Theme = serde_json::from_str(&raw_json)?;

        // Map Hex text array directly onto the internal 16-element u32 buffer array
        let mut color_buffer = [0u32; 16];
        for (i, hex_str) in theme.raw_colors.iter().enumerate().take(16) {
            let clean_hex = hex_str.trim_start_matches('#');
            if let Ok(val) = u32::from_str_radix(clean_hex, 16) {
                color_buffer[i] = val;
            }
        }
        theme.colors = color_buffer;
        Ok(theme)
    }

    /// Creates a fresh, custom cyberpunk-ready theme JSON file inside its explicit folder path.
    pub fn create_new_theme_template<P: AsRef<Path>>(
        dir_path: P,
        raw_name: &str,
    ) -> Result<PathBuf, std::io::Error> {
        let target_dir = dir_path.as_ref();

        // <-- Added: Explicitly provision directory trees safely before saving files to disk
        fs::create_dir_all(target_dir)?;

        // Clean up name string to create a safe file slug (e.g., "Neon Nights" -> "NeonNights")
        let file_slug = raw_name
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>();

        if file_slug.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid theme name",
            ));
        }

        let theme_path = target_dir.join(format!("{}.json", file_slug));

        // Structural JSON layout mapping to match our dynamic loaders
        let template_content = format!(
            "{{\n\
            \x20\x20\"name\": \"{}\",\n\
            \x20\x20\"author\": \"Brett\",\n\
            \x20\x20\"category\": \"sub-cyber\",\n\
            \x20\x20\"background\": \"#0a0a0f\",\n\
            \x20\x20\"foreground\": \"#bcbcbc\",\n\
            \x20\x20\"cursor\": \"#00ffaa\",\n\
            \x20\x20\"colors\": [\n\
            \x20\x20\x20\x20\"#0a0a0f\", \"#ff0055\", \"#00ffaa\", \"#f3e600\",\n\
            \x20\x20\x20\x20\"#00bfff\", \"#de00fe\", \"#00ffff\", \"#bcbcbc\",\n\
            \x20\x20\x20\x20\"#222233\", \"#ff3377\", \"#33ffbb\", \"#ffff55\",\n\
            \x20\x20\x20\x20\"#33ccff\", \"#e533ff\", \"#55ffff\", \"#ffffff\"\n\
            \x20\x20]\n\
            }}",
            raw_name
        );

        fs::write(&theme_path, template_content)?;
        Ok(theme_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_kitty_conf_syntax() {
        let dir = std::env::temp_dir().join(format!("cyberterm_theme_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test_kitty.conf");
        fs::write(
            &path,
            "# a comment line, and a blank line below\n\
             \n\
             background #1e1f29\n\
             foreground #f8f8f2\n\
             cursor #ff79c6\n\
             selection_background #444444\n\
             color0 #1e1f29\n\
             color1 #ff5555\n\
             color7 #f8f8f2\n\
             color15 #ffffff\n",
        )
        .unwrap();

        let theme = ThemeRegistry::parse_theme_file(&path).unwrap();

        assert_eq!(theme.name, "test_kitty");
        assert_eq!(theme.background, "#1e1f29");
        assert_eq!(theme.foreground, "#f8f8f2");
        assert_eq!(theme.cursor, "#ff79c6"); // explicit key, not derived from foreground
        assert_eq!(theme.colors[0], 0x1e1f29);
        assert_eq!(theme.colors[1], 0xff5555);
        assert_eq!(theme.colors[15], 0xffffff);
        assert_eq!(theme.colors[8], 0); // never set, stays zeroed

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn falls_back_to_ansi_convention_when_named_keys_missing() {
        let dir =
            std::env::temp_dir().join(format!("cyberterm_theme_test2_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("no_named_keys.conf");
        fs::write(&path, "color0 #050505\ncolor7 #e0e0e0\n").unwrap();

        let theme = ThemeRegistry::parse_theme_file(&path).unwrap();

        assert_eq!(theme.background, "#050505"); // derived from color0
        assert_eq!(theme.foreground, "#e0e0e0"); // derived from color7
        assert_eq!(theme.cursor, "#e0e0e0"); // falls back to foreground

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_from_dir_finds_flat_conf_files() {
        let dir =
            std::env::temp_dir().join(format!("cyberterm_theme_test3_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("one.conf"),
            "background #000000\nforeground #ffffff\n",
        )
        .unwrap();
        fs::write(dir.join("not_a_theme.txt"), "background #ff00ff\n").unwrap();

        let registry = ThemeRegistry::load_from_dir(&dir);

        assert_eq!(registry.themes.len(), 1);
        assert_eq!(registry.themes[0].name, "one");

        fs::remove_dir_all(&dir).ok();
    }
}
