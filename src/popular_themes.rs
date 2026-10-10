// src/popular_themes.rs
//
// Popular themes shipped in the binary (about 14 KB): Kitty-format files
// from iTerm2-Color-Schemes (MIT, themes/popular/LICENSE). Written into
// the themes folder's `popular/` on first run; `cyberterm +themes` adds
// the full collections.

pub const LICENSE: &str = include_str!("../themes/popular/LICENSE");

pub const THEMES: &[(&str, &str)] = &[
    ("Ayu", include_str!("../themes/popular/Ayu.conf")),
    (
        "Ayu Light",
        include_str!("../themes/popular/Ayu Light.conf"),
    ),
    (
        "Catppuccin Frappe",
        include_str!("../themes/popular/Catppuccin Frappe.conf"),
    ),
    (
        "Catppuccin Latte",
        include_str!("../themes/popular/Catppuccin Latte.conf"),
    ),
    (
        "Catppuccin Macchiato",
        include_str!("../themes/popular/Catppuccin Macchiato.conf"),
    ),
    (
        "Catppuccin Mocha",
        include_str!("../themes/popular/Catppuccin Mocha.conf"),
    ),
    ("Dracula", include_str!("../themes/popular/Dracula.conf")),
    (
        "Everforest Dark",
        include_str!("../themes/popular/Everforest Dark.conf"),
    ),
    (
        "Everforest Light",
        include_str!("../themes/popular/Everforest Light.conf"),
    ),
    (
        "GitHub Dark",
        include_str!("../themes/popular/GitHub Dark.conf"),
    ),
    (
        "GitHub Light",
        include_str!("../themes/popular/GitHub Light.conf"),
    ),
    (
        "Gruvbox Dark",
        include_str!("../themes/popular/Gruvbox Dark.conf"),
    ),
    (
        "Gruvbox Light",
        include_str!("../themes/popular/Gruvbox Light.conf"),
    ),
    (
        "Gruvbox Material",
        include_str!("../themes/popular/Gruvbox Material.conf"),
    ),
    (
        "Kanagawa Dragon",
        include_str!("../themes/popular/Kanagawa Dragon.conf"),
    ),
    (
        "Kanagawa Wave",
        include_str!("../themes/popular/Kanagawa Wave.conf"),
    ),
    (
        "Material Ocean",
        include_str!("../themes/popular/Material Ocean.conf"),
    ),
    (
        "Monokai Pro",
        include_str!("../themes/popular/Monokai Pro.conf"),
    ),
    ("Moonfly", include_str!("../themes/popular/Moonfly.conf")),
    (
        "Night Owl",
        include_str!("../themes/popular/Night Owl.conf"),
    ),
    ("Nightfox", include_str!("../themes/popular/Nightfox.conf")),
    ("Nord", include_str!("../themes/popular/Nord.conf")),
    (
        "Nord Light",
        include_str!("../themes/popular/Nord Light.conf"),
    ),
    ("One Dark", include_str!("../themes/popular/One Dark.conf")),
    (
        "One Light",
        include_str!("../themes/popular/One Light.conf"),
    ),
    (
        "Oxocarbon",
        include_str!("../themes/popular/Oxocarbon.conf"),
    ),
    (
        "Poimandres",
        include_str!("../themes/popular/Poimandres.conf"),
    ),
    (
        "Rose Pine",
        include_str!("../themes/popular/Rose Pine.conf"),
    ),
    (
        "Rose Pine Dawn",
        include_str!("../themes/popular/Rose Pine Dawn.conf"),
    ),
    (
        "Rose Pine Moon",
        include_str!("../themes/popular/Rose Pine Moon.conf"),
    ),
    (
        "Solarized Dark",
        include_str!("../themes/popular/Solarized Dark.conf"),
    ),
    (
        "Solarized Light",
        include_str!("../themes/popular/Solarized Light.conf"),
    ),
    (
        "Tokyo Night",
        include_str!("../themes/popular/Tokyo Night.conf"),
    ),
    (
        "Tokyo Night Day",
        include_str!("../themes/popular/Tokyo Night Day.conf"),
    ),
    (
        "Tokyo Night Storm",
        include_str!("../themes/popular/Tokyo Night Storm.conf"),
    ),
    ("Vesper", include_str!("../themes/popular/Vesper.conf")),
];
