// src/input/bindings.rs
//
// Terminal-level keybindings (copy, paste, scrollback, splits, tabs, ...).
// A combo that matches a binding is handled by Cyberterm and never reaches
// the shell; everything else is encoded by `keyboard.rs`.
//
// Two tables:
// - Direct bindings. Split/tab defaults follow Ghostty's, so muscle memory
//   carries over.
// - Leader bindings (`"leader+%" = "split_right"`), active only when
//   `[keyboard] leader` is set: press the leader, then the key -- tmux's
//   prefix model. The defaults mirror tmux's own keys.

use std::collections::BTreeMap;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::layout::Direction;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Copy,
    Paste,
    PastePrimary,
    SelectAll,
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
    Split(Direction),
    ClosePane,
    Focus(Direction),
    FocusNext,
    FocusPrevious,
    Resize(Direction),
    EqualizeSplits,
    ToggleZoom,
    ToggleBroadcast,
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    /// 1-based; 9 is reserved for "last tab", as in browsers and Ghostty.
    GotoTab(u8),
    LastTab,
    MoveTabLeft,
    MoveTabRight,
    NewWindow,
    CopyLastOutput,
    ShowLastOutput,
    FindInScrollback,
    HistorySearch,
}

use Direction::{Down, Left, Right, Up};

/// Every action with its config name and description, in the order
/// `+list-termkeys` shows them.
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
    (
        Action::Split(Right),
        "split_right",
        "Split: new pane to the right",
    ),
    (Action::Split(Down), "split_down", "Split: new pane below"),
    (
        Action::Split(Left),
        "split_left",
        "Split: new pane to the left",
    ),
    (Action::Split(Up), "split_up", "Split: new pane above"),
    (Action::ClosePane, "close_pane", "Close the focused pane"),
    (
        Action::Focus(Left),
        "focus_left",
        "Focus the pane to the left",
    ),
    (
        Action::Focus(Right),
        "focus_right",
        "Focus the pane to the right",
    ),
    (Action::Focus(Up), "focus_up", "Focus the pane above"),
    (Action::Focus(Down), "focus_down", "Focus the pane below"),
    (Action::FocusNext, "focus_next", "Focus the next pane"),
    (
        Action::FocusPrevious,
        "focus_previous",
        "Focus the previous pane",
    ),
    (
        Action::Resize(Left),
        "resize_left",
        "Move the nearest vertical divider left",
    ),
    (
        Action::Resize(Right),
        "resize_right",
        "Move the nearest vertical divider right",
    ),
    (
        Action::Resize(Up),
        "resize_up",
        "Move the nearest horizontal divider up",
    ),
    (
        Action::Resize(Down),
        "resize_down",
        "Move the nearest horizontal divider down",
    ),
    (
        Action::EqualizeSplits,
        "equalize_splits",
        "Give every pane in the tab equal space",
    ),
    (
        Action::ToggleZoom,
        "toggle_zoom",
        "Zoom the focused pane to the full tab (toggle)",
    ),
    (
        Action::ToggleBroadcast,
        "toggle_broadcast",
        "Type into every pane in the tab (toggle)",
    ),
    (Action::NewTab, "new_tab", "Open a new tab"),
    (Action::CloseTab, "close_tab", "Close the current tab"),
    (Action::NextTab, "next_tab", "Switch to the next tab"),
    (
        Action::PreviousTab,
        "previous_tab",
        "Switch to the previous tab",
    ),
    (Action::GotoTab(1), "goto_tab_1", "Switch to tab 1"),
    (Action::GotoTab(2), "goto_tab_2", "Switch to tab 2"),
    (Action::GotoTab(3), "goto_tab_3", "Switch to tab 3"),
    (Action::GotoTab(4), "goto_tab_4", "Switch to tab 4"),
    (Action::GotoTab(5), "goto_tab_5", "Switch to tab 5"),
    (Action::GotoTab(6), "goto_tab_6", "Switch to tab 6"),
    (Action::GotoTab(7), "goto_tab_7", "Switch to tab 7"),
    (Action::GotoTab(8), "goto_tab_8", "Switch to tab 8"),
    (Action::LastTab, "last_tab", "Switch to the last tab"),
    (
        Action::MoveTabLeft,
        "move_tab_left",
        "Move the current tab left",
    ),
    (
        Action::MoveTabRight,
        "move_tab_right",
        "Move the current tab right",
    ),
    (
        Action::NewWindow,
        "new_window",
        "Open a new Cyberterm window",
    ),
    (
        Action::CopyLastOutput,
        "copy_last_output",
        "Copy the last command's output",
    ),
    (
        Action::ShowLastOutput,
        "show_last_output",
        "Open the last command's output in a pager",
    ),
    (
        Action::FindInScrollback,
        "find",
        "Search the scrollback (Enter older, Shift+Enter newer)",
    ),
    (
        Action::HistorySearch,
        "history_search",
        "Search saved commands and their output",
    ),
];

pub const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("ctrl+shift+c", "copy"),
    ("ctrl+shift+v", "paste"),
    // Ctrl+Insert / Shift+Insert are also what Omarchy's universal
    // Super+C / Super+V send to the focused window.
    ("ctrl+insert", "copy"),
    ("shift+insert", "paste"),
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
    ("ctrl+shift+comma", "theme_menu"),
    ("ctrl+shift+r", "reload_config"),
    // Splits and tabs: Ghostty's defaults.
    ("ctrl+shift+o", "split_right"),
    ("ctrl+shift+e", "split_down"),
    ("ctrl+shift+w", "close_pane"),
    ("ctrl+alt+left", "focus_left"),
    ("ctrl+alt+right", "focus_right"),
    ("ctrl+alt+up", "focus_up"),
    ("ctrl+alt+down", "focus_down"),
    ("ctrl+super+]", "focus_next"),
    ("ctrl+super+[", "focus_previous"),
    ("ctrl+super+shift+left", "resize_left"),
    ("ctrl+super+shift+right", "resize_right"),
    ("ctrl+super+shift+up", "resize_up"),
    ("ctrl+super+shift+down", "resize_down"),
    // Omarchy's Ghostty config resizes with Alt added.
    ("ctrl+super+shift+alt+left", "resize_left"),
    ("ctrl+super+shift+alt+right", "resize_right"),
    ("ctrl+super+shift+alt+up", "resize_up"),
    ("ctrl+super+shift+alt+down", "resize_down"),
    ("ctrl+super+shift+equal", "equalize_splits"),
    ("ctrl+shift+enter", "toggle_zoom"),
    ("ctrl+shift+b", "toggle_broadcast"),
    ("ctrl+shift+t", "new_tab"),
    ("ctrl+tab", "next_tab"),
    ("ctrl+shift+tab", "previous_tab"),
    ("ctrl+page_down", "next_tab"),
    ("ctrl+page_up", "previous_tab"),
    ("ctrl+shift+page_up", "move_tab_left"),
    ("ctrl+shift+page_down", "move_tab_right"),
    ("alt+1", "goto_tab_1"),
    ("alt+2", "goto_tab_2"),
    ("alt+3", "goto_tab_3"),
    ("alt+4", "goto_tab_4"),
    ("alt+5", "goto_tab_5"),
    ("alt+6", "goto_tab_6"),
    ("alt+7", "goto_tab_7"),
    ("alt+8", "goto_tab_8"),
    ("alt+9", "last_tab"),
    ("ctrl+shift+n", "new_window"),
    ("ctrl+shift+g", "show_last_output"),
    ("ctrl+shift+y", "copy_last_output"),
    ("ctrl+shift+f", "find"),
    ("ctrl+shift+h", "history_search"),
];

/// Keys after the leader, when one is configured: tmux's own defaults.
pub const DEFAULT_LEADER_BINDINGS: &[(&str, &str)] = &[
    ("%", "split_right"),
    ("\"", "split_down"),
    ("x", "close_pane"),
    ("z", "toggle_zoom"),
    ("o", "focus_next"),
    (";", "focus_previous"),
    ("left", "focus_left"),
    ("right", "focus_right"),
    ("up", "focus_up"),
    ("down", "focus_down"),
    ("ctrl+left", "resize_left"),
    ("ctrl+right", "resize_right"),
    ("ctrl+up", "resize_up"),
    ("ctrl+down", "resize_down"),
    ("space", "equalize_splits"),
    ("c", "new_tab"),
    ("&", "close_tab"),
    ("n", "next_tab"),
    ("p", "previous_tab"),
    ("1", "goto_tab_1"),
    ("2", "goto_tab_2"),
    ("3", "goto_tab_3"),
    ("4", "goto_tab_4"),
    ("5", "goto_tab_5"),
    ("6", "goto_tab_6"),
    ("7", "goto_tab_7"),
    ("8", "goto_tab_8"),
    ("9", "last_tab"),
    ("[", "scroll_page_up"),
];

const LEADER_PREFIX: &str = "leader+";

fn action_by_name(name: &str) -> Option<Action> {
    ACTIONS
        .iter()
        .find(|(_, n, _)| *n == name)
        .map(|(a, _, _)| *a)
}

fn description(action: Action) -> &'static str {
    ACTIONS
        .iter()
        .find(|(a, _, _)| *a == action)
        .map(|(_, _, d)| *d)
        .unwrap_or("")
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
            "comma" => KeyName::Char(",".into()),
            "period" => KeyName::Char(".".into()),
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

/// `ctrl+shift+c` -> `Ctrl+Shift+C`, for display.
fn pretty(combo: &str) -> String {
    combo
        .split('+')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => "+".to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
        .replace("++", "+")
        .replace("Comma", ",")
        .replace("Period", ".")
        .replace("Equal", "=")
        .replace("Minus", "-")
        .replace("Plus", "+")
}

type Entry = (Combo, Action, String);

pub struct Bindings {
    /// Combo, action, and the combo as written (for menus and help).
    entries: Vec<Entry>,
    leader: Option<(Combo, String)>,
    leader_entries: Vec<Entry>,
}

impl Bindings {
    /// Built-in bindings with the user's `[keybindings]` table applied on
    /// top, plus the leader table when `leader` is set. Returns the
    /// problems found so they can be reported instead of silently ignored.
    pub fn new(overrides: &BTreeMap<String, String>, leader: Option<&str>) -> (Self, Vec<String>) {
        let mut errors = Vec::new();
        let defaults = |table: &[(&str, &str)]| -> Vec<Entry> {
            table
                .iter()
                .filter_map(|(combo, action)| {
                    Some((
                        Combo::parse(combo).ok()?,
                        action_by_name(action)?,
                        combo.to_string(),
                    ))
                })
                .collect()
        };
        let mut entries = defaults(DEFAULT_BINDINGS);
        let mut leader_entries = defaults(DEFAULT_LEADER_BINDINGS);

        let leader = match leader.map(str::trim).filter(|l| !l.is_empty()) {
            Some(text) => match Combo::parse(text) {
                Ok(combo) => {
                    // The leader itself must never also trigger a binding.
                    entries.retain(|(c, _, _)| *c != combo);
                    Some((combo, text.to_string()))
                }
                Err(e) => {
                    errors.push(format!("leader: {e}"));
                    None
                }
            },
            None => None,
        };

        for (combo_text, action_name) in overrides {
            let (table, key_text) = match combo_text.strip_prefix(LEADER_PREFIX) {
                Some(rest) => (&mut leader_entries, rest),
                None => (&mut entries, combo_text.as_str()),
            };
            let combo = match Combo::parse(key_text) {
                Ok(c) => c,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            table.retain(|(c, _, _)| *c != combo);
            if action_name == "none" {
                continue;
            }
            match action_by_name(action_name) {
                Some(action) => table.push((combo, action, key_text.to_string())),
                None => errors.push(format!("unknown action `{action_name}` for `{combo_text}`")),
            }
        }
        (
            Self {
                entries,
                leader,
                leader_entries,
            },
            errors,
        )
    }

    pub fn lookup(&self, logical: &Key, base: &Key, mods: ModifiersState) -> Option<Action> {
        find(&self.entries, logical, base, mods)
    }

    /// Whether this key is the configured leader.
    pub fn is_leader(&self, logical: &Key, base: &Key, mods: ModifiersState) -> bool {
        self.leader
            .as_ref()
            .is_some_and(|(combo, _)| combo.matches(logical, base, mods))
    }

    pub fn lookup_leader(&self, logical: &Key, base: &Key, mods: ModifiersState) -> Option<Action> {
        find(&self.leader_entries, logical, base, mods)
    }

    /// The first combo bound to `action`, formatted for display.
    pub fn hint(&self, action: Action) -> String {
        if let Some((_, _, text)) = self.entries.iter().find(|(_, a, _)| *a == action) {
            return pretty(text);
        }
        match (
            &self.leader,
            self.leader_entries.iter().find(|(_, a, _)| *a == action),
        ) {
            (Some((_, leader)), Some((_, _, text))) => {
                format!("{} {}", pretty(leader), pretty(text))
            }
            _ => String::new(),
        }
    }

    /// The effective binding list, for `cyberterm +list-termkeys`.
    pub fn describe(&self) -> Vec<(String, &'static str)> {
        let mut out: Vec<(String, &'static str)> = self
            .entries
            .iter()
            .map(|(_, action, text)| (text.clone(), description(*action)))
            .collect();
        if let Some((_, leader)) = &self.leader {
            out.extend(
                self.leader_entries
                    .iter()
                    .map(|(_, action, text)| (format!("{leader}, {text}"), description(*action))),
            );
        }
        out
    }
}

fn find(entries: &[Entry], logical: &Key, base: &Key, mods: ModifiersState) -> Option<Action> {
    entries
        .iter()
        .find(|(combo, _, _)| combo.matches(logical, base, mods))
        .map(|(_, a, _)| *a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    fn none() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn defaults_match_shifted_letters_by_base_key() {
        let (b, errors) = Bindings::new(&none(), None);
        assert!(errors.is_empty());
        let mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(b.lookup(&ch("C"), &ch("c"), mods), Some(Action::Copy));
        assert_eq!(b.lookup(&ch("V"), &ch("v"), mods), Some(Action::Paste));
        // Plain Ctrl+C is the shell's interrupt, never a copy.
        assert_eq!(b.lookup(&ch("c"), &ch("c"), ModifiersState::CONTROL), None);
    }

    #[test]
    fn shifted_symbols_match_their_logical_key() {
        let (b, _) = Bindings::new(&none(), None);
        let mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(
            b.lookup(&ch("+"), &ch("="), mods),
            Some(Action::FontIncrease)
        );
        assert_eq!(
            b.lookup(&ch("="), &ch("="), ModifiersState::CONTROL),
            Some(Action::FontIncrease)
        );
        let page_up = Key::Named(NamedKey::PageUp);
        assert_eq!(
            b.lookup(&page_up, &page_up, ModifiersState::SHIFT),
            Some(Action::ScrollPageUp)
        );
    }

    #[test]
    fn ghostty_split_and_tab_defaults() {
        let (b, _) = Bindings::new(&none(), None);
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(b.lookup(&ch("O"), &ch("o"), cs), Some(Action::Split(Right)));
        assert_eq!(b.lookup(&ch("E"), &ch("e"), cs), Some(Action::Split(Down)));
        assert_eq!(b.lookup(&ch("T"), &ch("t"), cs), Some(Action::NewTab));
        let tab = Key::Named(NamedKey::Tab);
        assert_eq!(
            b.lookup(&tab, &tab, ModifiersState::CONTROL),
            Some(Action::NextTab)
        );
        assert_eq!(b.lookup(&tab, &tab, cs), Some(Action::PreviousTab));
        assert_eq!(
            b.lookup(&ch("3"), &ch("3"), ModifiersState::ALT),
            Some(Action::GotoTab(3))
        );
        let left = Key::Named(NamedKey::ArrowLeft);
        assert_eq!(
            b.lookup(&left, &left, ModifiersState::CONTROL | ModifiersState::ALT),
            Some(Action::Focus(Left))
        );
    }

    #[test]
    fn omarchy_universal_copy_paste_keys_work() {
        let (b, _) = Bindings::new(&none(), None);
        let insert = Key::Named(NamedKey::Insert);
        assert_eq!(
            b.lookup(&insert, &insert, ModifiersState::CONTROL),
            Some(Action::Copy)
        );
        assert_eq!(
            b.lookup(&insert, &insert, ModifiersState::SHIFT),
            Some(Action::Paste)
        );
    }

    #[test]
    fn overrides_rebind_and_unbind() {
        let mut o = none();
        o.insert("ctrl+shift+c".to_string(), "none".to_string());
        o.insert("ctrl+alt+c".to_string(), "copy".to_string());
        o.insert("ctrl+q".to_string(), "bogus".to_string());
        o.insert("hyper+q".to_string(), "copy".to_string());
        let (b, errors) = Bindings::new(&o, None);
        assert_eq!(errors.len(), 2);
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(b.lookup(&ch("C"), &ch("c"), cs), None);
        let ca = ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(b.lookup(&ch("c"), &ch("c"), ca), Some(Action::Copy));
    }

    #[test]
    fn leader_bindings_follow_tmux() {
        let mut o = none();
        o.insert("leader+v".to_string(), "split_right".to_string());
        o.insert("leader+c".to_string(), "none".to_string());
        let (b, errors) = Bindings::new(&o, Some("ctrl+b"));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(b.is_leader(&ch("b"), &ch("b"), ModifiersState::CONTROL));
        assert!(!b.is_leader(&ch("b"), &ch("b"), ModifiersState::empty()));
        // `%` is Shift+5.
        assert_eq!(
            b.lookup_leader(&ch("%"), &ch("5"), ModifiersState::SHIFT),
            Some(Action::Split(Right))
        );
        assert_eq!(
            b.lookup_leader(&ch("\""), &ch("'"), ModifiersState::SHIFT),
            Some(Action::Split(Down))
        );
        assert_eq!(
            b.lookup_leader(&ch("v"), &ch("v"), ModifiersState::empty()),
            Some(Action::Split(Right))
        );
        assert_eq!(
            b.lookup_leader(&ch("c"), &ch("c"), ModifiersState::empty()),
            None
        );
        assert_eq!(b.hint(Action::Split(Right)), "Ctrl+Shift+O");
        assert_eq!(b.hint(Action::Focus(Left)), "Ctrl+Alt+Left");
        assert!(b.describe().iter().any(|(c, _)| c == "ctrl+b, %"));
    }

    #[test]
    fn no_leader_means_no_leader_key() {
        let (b, _) = Bindings::new(&none(), None);
        assert!(!b.is_leader(&ch("b"), &ch("b"), ModifiersState::CONTROL));
        let (_, errors) = Bindings::new(&none(), Some("ctrl+nonsense"));
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn hints_are_readable() {
        let (b, _) = Bindings::new(&none(), None);
        assert_eq!(b.hint(Action::Copy), "Ctrl+Shift+C");
        assert_eq!(b.hint(Action::PastePrimary), "");
        assert_eq!(pretty("ctrl++"), "Ctrl++");
        assert_eq!(pretty("ctrl+shift+comma"), "Ctrl+Shift+,");
        assert_eq!(pretty("ctrl+super+shift+equal"), "Ctrl+Super+Shift+=");
    }

    #[test]
    fn parses_special_spellings() {
        assert_eq!(
            Combo::parse("ctrl++").unwrap(),
            Combo::parse("ctrl+plus").unwrap()
        );
        assert!(Combo::parse("alt+f5").is_ok());
        assert!(Combo::parse("ctrl+shift+comma").is_ok());
        assert!(Combo::parse("ctrl+f13").is_err());
        assert!(Combo::parse("ctrl+nonsense").is_err());
    }

    #[test]
    fn every_default_names_a_real_action() {
        for (combo, action) in DEFAULT_BINDINGS.iter().chain(DEFAULT_LEADER_BINDINGS) {
            assert!(Combo::parse(combo).is_ok(), "{combo}");
            assert!(action_by_name(action).is_some(), "{action}");
        }
    }

    #[test]
    fn default_combos_are_unique() {
        let mut seen = Vec::new();
        for (combo, _) in DEFAULT_BINDINGS {
            let parsed = Combo::parse(combo).unwrap();
            assert!(!seen.contains(&parsed), "{combo} bound twice");
            seen.push(parsed);
        }
    }
}
