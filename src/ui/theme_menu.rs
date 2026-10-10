// src/ui/theme_menu.rs
//
// The theme picker (Ctrl+Shift+,): type to filter, themes grouped under
// their family, and a live preview -- the whole window switches to the
// highlighted theme, and a sample (prompt, git, ls, a diff, an error)
// shows its colors in use.
use crate::theme::{Theme, ThemeRegistry};

/// One colored run of text within a menu line.
pub struct Span {
    pub text: String,
    pub color: [u8; 3],
}

impl Span {
    fn new(text: impl Into<String>, color: [u8; 3]) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

fn rgb(color: u32) -> [u8; 3] {
    [
        ((color >> 16) & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        (color & 0xFF) as u8,
    ]
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let m = |x: u8, y: u8| (x as f32 * t + y as f32 * (1.0 - t)) as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

fn hex(s: &str) -> [u8; 3] {
    rgb(u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0xc0c8d8))
}

/// Indices of the themes matching `query` (fuzzy, on "name family"), in
/// the registry's grouped order.
pub fn filter(registry: &ThemeRegistry, query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    registry
        .themes
        .iter()
        .enumerate()
        .filter(|(_, t)| q.is_empty() || matches(&q, t))
        .map(|(i, _)| i)
        .collect()
}

/// A theme matches when its family contains the query, or its name does
/// as a compact fuzzy match (letters close together, not scattered across
/// a long name).
fn matches(q: &str, t: &Theme) -> bool {
    if t.category.to_lowercase().contains(q) {
        return true;
    }
    let Some((_, pos)) = crate::fuzzy::fuzzy(q, &t.name) else {
        return false;
    };
    match (pos.first(), pos.last()) {
        (Some(first), Some(last)) => last - first < pos.len() * 2 + 2,
        _ => true,
    }
}

/// What the picker shows: the query, the filtered themes and the cursor
/// (an index into `view`).
pub struct View<'a> {
    pub registry: &'a ThemeRegistry,
    pub view: &'a [usize],
    pub cursor: usize,
    pub query: &'a str,
    pub creating: Option<&'a str>,
}

enum Row {
    Header(String, usize),
    Item(usize),
}

/// The picker as lines of colored spans, `cols` wide and `rows` tall.
pub fn build_lines(v: &View<'_>, cols: usize, rows: usize) -> Vec<Vec<Span>> {
    let theme = v.view.get(v.cursor).and_then(|&i| v.registry.themes.get(i));
    // Colors of the theme being previewed (the whole window shows it).
    let (fg, dim, accent, warn) = match theme {
        // Secondary text is mixed from the foreground and background, so it
        // stays readable whatever the theme's own "dim" color is.
        Some(t) => (
            hex(&t.foreground),
            mix(hex(&t.foreground), hex(&t.background), 0.55),
            rgb(t.colors[6]),
            rgb(t.colors[3]),
        ),
        None => (
            [0xc0, 0xc8, 0xd8],
            [0x60, 0x68, 0x78],
            [0x30, 0xf0, 0xf0],
            [0xf0, 0xd0, 0x30],
        ),
    };

    let left_w = (cols * 11 / 20).clamp(28.min(cols), 64);
    let preview = theme
        .map(|t| preview_lines(t, cols.saturating_sub(left_w + 3)))
        .unwrap_or_default();

    let mut left: Vec<Vec<Span>> = Vec::new();
    left.push(vec![
        Span::new("THEMES", accent),
        Span::new(
            format!("  {} of {}", v.view.len(), v.registry.themes.len()),
            dim,
        ),
    ]);
    match v.creating {
        Some(input) => left.push(vec![
            Span::new("New theme name: ", fg),
            Span::new(format!("{input}▏"), warn),
        ]),
        None => left.push(vec![
            Span::new("› ", accent),
            Span::new(v.query.to_string(), fg),
            Span::new("▏", accent),
        ]),
    }
    left.push(vec![]);

    // Group rows (family headers + themes), then window them around the
    // cursor.
    let mut list: Vec<Row> = Vec::new();
    let mut last: Option<&str> = None;
    for (pos, &i) in v.view.iter().enumerate() {
        let cat = v.registry.themes[i].category.as_str();
        if last != Some(cat) {
            let count = v.view[pos..]
                .iter()
                .take_while(|&&j| v.registry.themes[j].category == cat)
                .count();
            list.push(Row::Header(cat.to_string(), count));
            last = Some(cat);
        }
        list.push(Row::Item(pos));
    }
    let room = rows.saturating_sub(left.len() + 2).max(3);
    let cursor_row = list
        .iter()
        .position(|r| matches!(r, Row::Item(p) if *p == v.cursor))
        .unwrap_or(0);
    let first = cursor_row
        .saturating_sub(room / 2)
        .min(list.len().saturating_sub(room));
    if v.view.is_empty() {
        left.push(vec![Span::new("  No theme matches.", dim)]);
    }
    for row in list.iter().skip(first).take(room) {
        match row {
            Row::Header(cat, n) => left.push(vec![
                Span::new(format!("── {cat} "), dim),
                Span::new(format!("({n})"), dim),
            ]),
            Row::Item(pos) => {
                let t = &v.registry.themes[v.view[*pos]];
                let name: String = t.name.chars().take(left_w.saturating_sub(4)).collect();
                if *pos == v.cursor {
                    left.push(vec![Span::new(format!(" ▸ {name}"), accent)]);
                } else {
                    left.push(vec![Span::new(format!("   {name}"), fg)]);
                }
            }
        }
    }
    while left.len() < rows.saturating_sub(1) {
        left.push(vec![]);
    }
    left.truncate(rows.saturating_sub(1));
    left.push(vec![Span::new(
        "type to filter · ↑↓ PgUp PgDn · Enter apply · Esc cancel · Ctrl+N new · more: cyberterm +themes",
        dim,
    )]);

    // Merge the preview into the right-hand column.
    left.into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            let used: usize = line.iter().map(|s| s.text.chars().count()).sum();
            if let Some(p) = preview
                .get(i.saturating_sub(1))
                .filter(|_| i > 0 && i + 1 < rows)
            {
                if used < left_w + 2 {
                    line.push(Span::new(" ".repeat(left_w + 2 - used), fg));
                    line.push(Span::new("│ ", dim));
                    line.extend(p.iter().map(|s| Span::new(s.text.clone(), s.color)));
                }
            }
            line
        })
        .collect()
}

/// A sample of output in the theme's colors.
fn preview_lines(t: &Theme, width: usize) -> Vec<Vec<Span>> {
    if width < 16 {
        return Vec::new();
    }
    let c = |i: usize| rgb(t.colors[i]);
    let fg = hex(&t.foreground);
    let swatches =
        |range: std::ops::Range<usize>| range.map(|i| Span::new("███", c(i))).collect::<Vec<_>>();
    let mut out = vec![
        vec![
            Span::new(t.name.clone(), c(6)),
            Span::new(format!("  {}", t.category), c(8)),
        ],
        vec![],
        swatches(0..8),
        swatches(8..16),
        vec![],
        vec![
            Span::new("raven", c(5)),
            Span::new("@", fg),
            Span::new("box ", c(4)),
            Span::new("~/api ", c(2)),
            Span::new("❯ ", c(5)),
            Span::new("git status --short", fg),
        ],
        vec![Span::new(" M ", c(1)), Span::new("src/main.rs", fg)],
        vec![Span::new("?? ", c(1)), Span::new("notes.md", fg)],
        vec![Span::new("❯ ", c(5)), Span::new("ls", fg)],
        vec![
            Span::new("Cargo.toml  ", fg),
            Span::new("src/  ", c(4)),
            Span::new("target/  ", c(4)),
            Span::new("run.sh", c(2)),
        ],
        vec![Span::new("❯ ", c(5)), Span::new("git diff", fg)],
        vec![Span::new("@@ -1,2 +1,2 @@", c(6))],
        vec![Span::new("- let port = 3000;", c(1))],
        vec![Span::new("+ let port = 5173;", c(2))],
        vec![
            Span::new("error", c(9)),
            Span::new(": ", fg),
            Span::new("cannot find value `cfg`", fg),
        ],
        vec![
            Span::new("warning", c(11)),
            Span::new(": unused import", fg),
        ],
    ];
    for line in &mut out {
        // Keep each preview line within the column.
        let mut left = width;
        line.retain_mut(|s| {
            if left == 0 {
                return false;
            }
            let n = s.text.chars().count();
            if n > left {
                s.text = s.text.chars().take(left).collect();
            }
            left = left.saturating_sub(n);
            true
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(name: &str, category: &str) -> Theme {
        Theme {
            name: name.into(),
            author: String::new(),
            category: category.into(),
            background: "#000000".into(),
            foreground: "#ffffff".into(),
            cursor: "#ffffff".into(),
            colors: [0x112233; 16],
            raw_colors: vec![],
        }
    }

    fn registry() -> ThemeRegistry {
        let mut r = ThemeRegistry {
            themes: vec![
                theme("Dracula", "iterm2"),
                theme("synthwave_84", "built-in"),
                theme("neosynth-drive", "neosynth"),
                theme("Tokyo Night", "iterm2"),
            ],
            selected_index: 0,
        };
        r.sort();
        r
    }

    #[test]
    fn filters_by_name_and_family() {
        let r = registry();
        assert_eq!(filter(&r, "").len(), 4);
        let hits: Vec<&str> = filter(&r, "tokyo")
            .iter()
            .map(|&i| r.themes[i].name.as_str())
            .collect();
        assert_eq!(hits, vec!["Tokyo Night"]);
        assert_eq!(filter(&r, "iterm").len(), 2);
        // Letters scattered across a long name don't count.
        let mut r2 = registry();
        r2.themes.push(theme("Dystopian_Ash_Sky", "dystopian"));
        let hits: Vec<&str> = filter(&r2, "tokyo")
            .iter()
            .map(|&i| r2.themes[i].name.as_str())
            .collect();
        assert_eq!(hits, vec!["Tokyo Night"]);
    }

    #[test]
    fn groups_under_family_headers_with_counts() {
        let r = registry();
        let view = filter(&r, "");
        let lines = build_lines(
            &View {
                registry: &r,
                view: &view,
                cursor: 0,
                query: "",
                creating: None,
            },
            100,
            20,
        );
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect();
        let all = text.join("\n");
        // built-in first, then the Cybercore families, then collections.
        let b = all.find("── built-in").unwrap();
        let n = all.find("── neosynth").unwrap();
        let i = all.find("── iterm2 (2)").unwrap();
        assert!(b < n && n < i, "{all}");
        assert!(all.contains("▸ synthwave_84"));
    }
}
