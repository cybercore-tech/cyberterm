// src/input/bindings.rs
//
// Terminal-level keybindings (copy, paste, scrollback, zoom, ...). A combo
// that matches a binding is handled by Cyberterm and never reaches the
// shell; everything else is encoded by `keyboard.rs`.

use std::collections::BTreeMap;
use winit::keyboard::{Key, ModifiersState, NamedKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Copy,
    Paste,
    PastePrimary,
    ScrollPageUp,
    ScrollPageDown,
    ScrollLineUp,
    ScrollLineDown,
    ScrollToTop,
    ScrollToBottom,
    PreviousPrompt,
    NextPrompt,
    ClearScrollback,
    FontIncrease,
    FontDecrease,
    FontReset,
    ThemeMenu,
    ReloadConfig,
    SelectAll,
}

/// Every action with its config name, in the order `+list-termkeys` shows.
pub const ACTIONS: &[(Action, &str, &str)] = &[
    (Action::Copy, "copy", "Copy the selection to the clipboard"),
    (Action::Paste, "paste", "Paste from the clipboard"),
    (
        Action::PastePrimary,
        "paste_primary",
        "Paste the primary selection",
    ),
    (
        Action::SelectAll,
        "select_all",
        "Select the whole scrollback",
    ),
    (
        Action::ScrollPageUp,
        "scroll_page_up",
        "Scroll back one page",
    ),
    (
        Action::ScrollPageDown,
        "scroll_page_down",
        "Scroll forward one page",
    ),
    (
        Action::ScrollLineUp,
        "scroll_line_up",
        "Scroll back one line",
    ),
    (
        Action::ScrollLineDown,
        "scroll_line_down",
        "Scroll forward one line",
    ),
    (
        Action::ScrollToTop,
        "scroll_to_top",
        "Jump to the top of the scrollback",
    ),
    (
        Action::ScrollToBottom,
        "scroll_to_bottom",
        "Jump back to the live screen",
    ),
    (
        Action::PreviousPrompt,
        "previous_prompt",
        "Jump to the previous shell prompt",
    ),
    (
        Action::NextPrompt,
        "next_prompt",
        "Jump to the next shell prompt",
    ),
    (
        Action::ClearScrollback,
        "clear_scrollback",
        "Erase the scrollback history",
    ),
    (
        Action::FontIncrease,
        "font_increase",
        "Make the font bigger",
    ),
    (
        Action::FontDecrease,
        "font_decrease",
        "Make the font smaller",
    ),
    (Action::FontReset, "font_reset", "Reset the font size"),
    (Action::ThemeMenu, "theme_menu", "Open the theme picker"),
    (
        Action::ReloadConfig,
        "reload_config",
        "Re-read the config file now",
    ),
];

pub const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("ctrl+shift+c", "copy"),
    ("ctrl+shift+v", "paste"),
    ("shift+insert", "paste_primary"),
    ("ctrl+shift+a", "select_all"),
    ("shift+page_up", "scroll_page_up"),
    ("shift+page_down", "scroll_page_down"),
    ("ctrl+shift+up", "scroll_line_up"),
    ("ctrl+shift+down", "scroll_line_down"),
    ("shift+home", "scroll_to_top"),
    ("shift+end", "scroll_to_bottom"),
    ("ctrl+shift+z", "previous_prompt"),
    ("ctrl+shift+x", "next_prompt"),
    ("ctrl+shift+k", "clear_scrollback"),
    ("ctrl+equal", "font_increase"),
    ("ctrl+plus", "font_increase"),
    ("ctrl+minus", "font_decrease"),
    ("ctrl+0", "font_reset"),
    ("ctrl+shift+t", "theme_menu"),
    ("ctrl+shift+r", "reload_config"),
];

fn action_by_name(name: &str) -> Option<Action> {
    ACTIONS
        .iter()
        .find(|(_, n, _)| *n == name)
        .map(|(a, _, _)| *a)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyName {
    Char(String),
    Named(NamedKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Combo {
    mods: ModifiersState,
    key: KeyName,
}

impl Combo {
    /// Parses `ctrl+shift+c`, `shift+page_up`, `ctrl+equal`, `alt+f5`, ...
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut mods = ModifiersState::empty();
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        // `ctrl++` means ctrl+plus.
        let (key_part, mod_parts) = match parts.as_slice() {
            [rest @ .., "", ""] => ("+", rest),
            [rest @ .., last] => (*last, rest),
            [] => return Err("empty keybinding".into()),
        };
        for m in mod_parts {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods |= ModifiersState::CONTROL,
                "shift" => mods |= ModifiersState::SHIFT,
                "alt" | "opt" | "option" => mods |= ModifiersState::ALT,
                "super" | "cmd" | "logo" | "meta" => mods |= ModifiersState::SUPER,
                other => return Err(format!("unknown modifier `{other}` in `{text}`")),
            }
        }
        let lower = key_part.to_ascii_lowercase();
        let key = match lower.as_str() {
            "page_up" | "pageup" => KeyName::Named(NamedKey::PageUp),
            "page_down" | "pagedown" => KeyName::Named(NamedKey::PageDown),
            "home" => KeyName::Named(NamedKey::Home),
            "end" => KeyName::Named(NamedKey::End),
            "insert" => KeyName::Named(NamedKey::Insert),
            "delete" => KeyName::Named(NamedKey::Delete),
            "up" => KeyName::Named(NamedKey::ArrowUp),
            "down" => KeyName::Named(NamedKey::ArrowDown),
            "left" => KeyName::Named(NamedKey::ArrowLeft),
            "right" => KeyName::Named(NamedKey::ArrowRight),
            "enter" | "return" => KeyName::Named(NamedKey::Enter),
            "tab" => KeyName::Named(NamedKey::Tab),
            "space" => KeyName::Named(NamedKey::Space),
            "escape" | "esc" => KeyName::Named(NamedKey::Escape),
            "backspace" => KeyName::Named(NamedKey::Backspace),
            "equal" => KeyName::Char("=".into()),
            "plus" => KeyName::Char("+".into()),
            "minus" => KeyName::Char("-".into()),
            f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
                let n: u8 = f[1..].parse().unwrap_or(0);
                KeyName::Named(function_key(n).ok_or_else(|| format!("no key `{f}`"))?)
            }
            c if c.chars().count() == 1 => KeyName::Char(c.to_string()),
            other => return Err(format!("unknown key `{other}` in `{text}`")),
        };
        Ok(Self { mods, key })
    }

    /// `logical` is the key as typed (Shift applied), `base` the same key
    /// without modifiers. A binding on a shifted symbol like `ctrl+plus`
    /// matches the logical key and ignores the Shift it took to type it.
    fn matches(&self, logical: &Key, base: &Key, mods: ModifiersState) -> bool {
        let relevant = ModifiersState::CONTROL
            | ModifiersState::SHIFT
            | ModifiersState::ALT
            | ModifiersState::SUPER;
        let mods = mods & relevant;
        match &self.key {
            KeyName::Named(named) => {
                matches!(logical, Key::Named(n) if n == named) && mods == self.mods
            }
            KeyName::Char(c) => {
                let base_hit = matches!(base, Key::Character(b) if b.to_lowercase() == *c);
                let logical_hit = matches!(logical, Key::Character(l) if l.to_lowercase() == *c);
                (base_hit && mods == self.mods)
                    || (logical_hit && mods - ModifiersState::SHIFT == self.mods)
            }
        }
    }
}

fn function_key(n: u8) -> Option<NamedKey> {
    use NamedKey::*;
    [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12]
        .get((n as usize).checked_sub(1)?)
        .copied()
}

pub struct Bindings {
    entries: Vec<(Combo, Action)>,
}

impl Bindings {
    /// Built-in bindings with the user's `[keybindings]` table applied on
    /// top. Returns the problems found so they can be reported instead of
    /// silently ignored.
    pub fn new(overrides: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut errors = Vec::new();
        let mut entries: Vec<(Combo, Action)> = DEFAULT_BINDINGS
            .iter()
            .filter_map(|(combo, action)| {
                Some((Combo::parse(combo).ok()?, action_by_name(action)?))
            })
            .collect();

        for (combo_text, action_name) in overrides {
            let combo = match Combo::parse(combo_text) {
                Ok(c) => c,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            entries.retain(|(c, _)| *c != combo);
            if action_name == "none" {
                continue;
            }
            match action_by_name(action_name) {
                Some(action) => entries.push((combo, action)),
                None => errors.push(format!("unknown action `{action_name}` for `{combo_text}`")),
            }
        }
        (Self { entries }, errors)
    }

    pub fn lookup(&self, logical: &Key, base: &Key, mods: ModifiersState) -> Option<Action> {
        self.entries
            .iter()
            .find(|(combo, _)| combo.matches(logical, base, mods))
            .map(|(_, a)| *a)
    }
}

/// The effective binding list, for `cyberterm +list-termkeys`.
pub fn describe(overrides: &BTreeMap<String, String>) -> Vec<(String, &'static str)> {
    let mut combos: Vec<(String, String)> = DEFAULT_BINDINGS
        .iter()
        .map(|(c, a)| (c.to_string(), a.to_string()))
        .collect();
    for (combo, action) in overrides {
        combos.retain(|(c, _)| c != combo);
        if action != "none" {
            combos.push((combo.clone(), action.clone()));
        }
    }
    combos
        .into_iter()
        .filter_map(|(combo, action)| {
            ACTIONS
                .iter()
                .find(|(_, n, _)| *n == action)
                .map(|(_, _, desc)| (combo, *desc))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    #[test]
    fn defaults_match_shifted_letters_by_base_key() {
        let (b, errors) = Bindings::new(&BTreeMap::new());
        assert!(errors.is_empty());
        let mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(b.lookup(&ch("C"), &ch("c"), mods), Some(Action::Copy));
        assert_eq!(b.lookup(&ch("V"), &ch("v"), mods), Some(Action::Paste));
        // Plain Ctrl+C is the shell's interrupt, never a copy.
        assert_eq!(b.lookup(&ch("c"), &ch("c"), ModifiersState::CONTROL), None);
    }

    #[test]
    fn shifted_symbols_match_their_logical_key() {
        let (b, _) = Bindings::new(&BTreeMap::new());
        let mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(
            b.lookup(&ch("+"), &ch("="), mods),
            Some(Action::FontIncrease)
        );
        assert_eq!(
            b.lookup(&ch("="), &ch("="), ModifiersState::CONTROL),
            Some(Action::FontIncrease)
        );
        assert_eq!(
            b.lookup(
                &Key::Named(NamedKey::PageUp),
                &Key::Named(NamedKey::PageUp),
                ModifiersState::SHIFT
            ),
            Some(Action::ScrollPageUp)
        );
    }

    #[test]
    fn overrides_rebind_and_unbind() {
        let mut o = BTreeMap::new();
        o.insert("ctrl+shift+c".to_string(), "none".to_string());
        o.insert("ctrl+alt+c".to_string(), "copy".to_string());
        o.insert("ctrl+q".to_string(), "bogus".to_string());
        o.insert("hyper+q".to_string(), "copy".to_string());
        let (b, errors) = Bindings::new(&o);
        assert_eq!(errors.len(), 2);
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(b.lookup(&ch("C"), &ch("c"), cs), None);
        let ca = ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(b.lookup(&ch("c"), &ch("c"), ca), Some(Action::Copy));
    }

    #[test]
    fn parses_special_spellings() {
        assert_eq!(
            Combo::parse("ctrl++").unwrap(),
            Combo::parse("ctrl+plus").unwrap()
        );
        assert!(Combo::parse("alt+f5").is_ok());
        assert!(Combo::parse("ctrl+f13").is_err());
        assert!(Combo::parse("ctrl+nonsense").is_err());
    }

    #[test]
    fn every_default_names_a_real_action() {
        for (combo, action) in DEFAULT_BINDINGS {
            assert!(Combo::parse(combo).is_ok(), "{combo}");
            assert!(action_by_name(action).is_some(), "{action}");
        }
    }
}
