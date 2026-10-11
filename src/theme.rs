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
            let normal = [
                &palette.bg,
                &palette.red,
                &palette.acid_green,
                &palette.orange,
                &palette.purple,
                &palette.hot_pink,
                &palette.cyan,
                &palette.white,
            ];
            // The catalog defines eight colors; derive the bright eight so
            // programs that use them for emphasis keep their contrast
            // (lighter on dark themes, deeper on light ones).
            let dark = luminance(&palette.bg) < 0.5;
            let mut raw_colors: Vec<String> = normal.iter().map(|c| format!("#{c}")).collect();
            raw_colors.push(format!("#{}", palette.muted));
            for c in &normal[1..] {
                raw_colors.push(format!("#{}", brighten(c, dark)));
            }
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
        self.sort();
        Some(active)
    }

    /// The theme to start with: the shared Cybercore one when it's active,
    /// else the configured one, else the first.
    pub fn initial(&self, shared_active: Option<&str>, configured: &str) -> Option<Theme> {
        self.themes
            .iter()
            .find(|t| Some(t.name.as_str()) == shared_active)
            .or_else(|| self.themes.iter().find(|t| t.name == configured))
            .or_else(|| self.themes.first())
            .cloned()
    }

    /// Groups themes by family (built-in first, the Cybercore families next,
    /// then installed collections) and sorts by name within each.
    pub fn sort(&mut self) {
        let selected = self.themes.get(self.selected_index).map(|t| t.name.clone());
        self.themes.sort_by(|a, b| {
            (
                category_rank(&a.category),
                a.category.to_lowercase(),
                a.name.to_lowercase(),
            )
                .cmp(&(
                    category_rank(&b.category),
                    b.category.to_lowercase(),
                    b.name.to_lowercase(),
                ))
        });
        if let Some(name) = selected {
            if let Some(i) = self.themes.iter().position(|t| t.name == name) {
                self.selected_index = i;
            }
        }
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
        collect_themes(base_dir, base_dir, 0, &mut themes);
        let mut registry = ThemeRegistry {
            themes,
            selected_index: 0,
        };
        registry.sort();
        registry
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

/// Walks the themes folder: `*.conf` (Kitty format) and `*.json` themes at
/// any depth up to four. A `.conf` file's category is the folder it's in
/// below the themes folder (`iterm2`, `kitty`), or `built-in` at the top.
fn collect_themes(base: &Path, dir: &Path, depth: usize, out: &mut Vec<Theme>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_themes(base, &path, depth + 1, out);
            continue;
        }
        match path.extension().and_then(|e| e.to_str()) {
            Some("conf") => {
                if let Ok(mut theme) = ThemeRegistry::parse_theme_file(&path) {
                    if depth > 0 {
                        if let Some(top) = path
                            .strip_prefix(base)
                            .ok()
                            .and_then(|rel| rel.components().next())
                        {
                            theme.category = top.as_os_str().to_string_lossy().into_owned();
                            theme.author = theme.category.clone();
                        }
                    }
                    out.push(theme);
                }
            }
            Some("json") => {
                if path.file_name().is_some_and(|n| n == "theme_template.json") {
                    continue;
                }
                if let Ok(theme) = ThemeRegistry::parse_json_theme(&path) {
                    out.push(theme);
                }
            }
            _ => {}
        }
    }
}

/// Sort order of theme families: built-in, then the Cybercore families,
/// then everything else.
fn category_rank(category: &str) -> u8 {
    match category {
        "built-in" => 0,
        "default" | "cyberdyne" | "cyberpunk" | "dystopian" | "neosynth" | "synthwave"
        | "omarchy-live" => 1,
        "popular" => 2,
        _ => 3,
    }
}

fn rgb(hex: &str) -> [f32; 3] {
    let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    [
        (v >> 16) as f32,
        ((v >> 8) & 0xff) as f32,
        (v & 0xff) as f32,
    ]
}

/// Relative brightness, 0 (black) to 1 (white).
pub(crate) fn luminance(hex: &str) -> f32 {
    let [r, g, b] = rgb(hex);
    (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0
}

/// A bright variant: mixed toward white on dark themes, toward black
/// (deeper) on light ones.
fn brighten(hex: &str, dark: bool) -> String {
    let c = rgb(hex);
    let mix = |v: f32| {
        if dark {
            v + (255.0 - v) * 0.3
        } else {
            v * 0.78
        }
    };
    format!(
        "{:02x}{:02x}{:02x}",
        mix(c[0]) as u8,
        mix(c[1]) as u8,
        mix(c[2]) as u8
    )
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

    #[test]
    fn nested_conf_files_take_their_folder_as_category() {
        let dir = std::env::temp_dir().join(format!("ct-theme-cat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("iterm2")).unwrap();
        std::fs::create_dir_all(dir.join("popular")).unwrap();
        let conf = "background #000000\nforeground #ffffff\ncolor1 #ff0000\n";
        std::fs::write(dir.join("top.conf"), conf).unwrap();
        std::fs::write(dir.join("iterm2/Zed.conf"), conf).unwrap();
        std::fs::write(dir.join("popular/Dracula.conf"), conf).unwrap();
        let r = ThemeRegistry::load_from_dir(&dir);
        let got: Vec<(&str, &str)> = r
            .themes
            .iter()
            .map(|t| (t.category.as_str(), t.name.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("built-in", "top"),
                ("popular", "Dracula"),
                ("iterm2", "Zed")
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bright_variants_lighten_dark_themes_and_deepen_light_ones() {
        assert!(luminance("000000") < 0.01 && luminance("ffffff") > 0.99);
        let dark = brighten("ff0040", true);
        assert_eq!(&dark[..2], "ff");
        assert!(u8::from_str_radix(&dark[4..6], 16).unwrap() > 0x40);
        let light = brighten("80c0ff", false);
        assert!(u8::from_str_radix(&light[..2], 16).unwrap() < 0x80);
    }
}
