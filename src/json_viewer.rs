// src/json_viewer.rs
//
// `cyberterm +json [file]`: a collapsible JSON tree in the terminal. Reads
// the file or stdin (so `curl ... | cyberterm +json` works; keys then come
// from /dev/tty), accepts JSON Lines too, and keeps object key order.
//
// Keys: arrows / hjkl move and fold, Enter/Space toggle, e / c expand or
// collapse everything, g / G top and bottom, PgUp/PgDn, / search and n for
// the next match, y copy the node's path (`.items[3].name`), Y copy its
// value (both via OSC 52), q / Esc quit.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Object,
    Array,
    Scalar,
}

#[derive(Debug)]
struct Node {
    depth: usize,
    /// Key in the parent (`"name"`) or index (`3`), empty for the root.
    key: Option<String>,
    index: Option<usize>,
    kind: Kind,
    /// Scalar text (JSON-formatted) or the child count for containers.
    text: String,
    children: Vec<usize>,
    parent: Option<usize>,
    expanded: bool,
}

/// The tree, flattened into an arena in document order.
pub struct Tree {
    nodes: Vec<Node>,
    values: Vec<Value>,
}

impl Tree {
    pub fn new(value: Value) -> Self {
        let mut tree = Self {
            nodes: Vec::new(),
            values: Vec::new(),
        };
        tree.add(value, 0, None, None, None);
        // Open the first two levels.
        for node in tree.nodes.iter_mut() {
            node.expanded = node.depth < 2;
        }
        tree
    }

    fn add(
        &mut self,
        value: Value,
        depth: usize,
        key: Option<String>,
        index: Option<usize>,
        parent: Option<usize>,
    ) -> usize {
        let id = self.nodes.len();
        let (kind, text) = match &value {
            Value::Object(m) => (Kind::Object, m.len().to_string()),
            Value::Array(a) => (Kind::Array, a.len().to_string()),
            other => (Kind::Scalar, other.to_string()),
        };
        self.nodes.push(Node {
            depth,
            key,
            index,
            kind,
            text,
            children: Vec::new(),
            parent,
            expanded: false,
        });
        self.values.push(Value::Null);
        let children: Vec<usize> = match &value {
            Value::Object(m) => m
                .iter()
                .map(|(k, v)| self.add(v.clone(), depth + 1, Some(k.clone()), None, Some(id)))
                .collect(),
            Value::Array(a) => a
                .iter()
                .enumerate()
                .map(|(i, v)| self.add(v.clone(), depth + 1, None, Some(i), Some(id)))
                .collect(),
            _ => Vec::new(),
        };
        self.nodes[id].children = children;
        self.values[id] = value;
        id
    }

    /// Node ids currently shown, in order.
    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![0];
        while let Some(id) = stack.pop() {
            out.push(id);
            let node = &self.nodes[id];
            if node.expanded {
                stack.extend(node.children.iter().rev());
            }
        }
        out
    }

    pub fn toggle(&mut self, id: usize) {
        if self.nodes[id].kind != Kind::Scalar {
            self.nodes[id].expanded = !self.nodes[id].expanded;
        }
    }

    pub fn set_all(&mut self, expanded: bool) {
        for node in self.nodes.iter_mut() {
            node.expanded = expanded || node.depth == 0;
        }
    }

    /// `.items[3].name` (jq syntax; keys that aren't identifiers quoted).
    pub fn path(&self, id: usize) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(id);
        while let Some(i) = cur {
            let n = &self.nodes[i];
            if let Some(k) = &n.key {
                if !k.is_empty()
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !k.starts_with(|c: char| c.is_ascii_digit())
                {
                    parts.push(format!(".{k}"));
                } else {
                    parts.push(format!("[{}]", Value::String(k.clone())));
                }
            } else if let Some(idx) = n.index {
                parts.push(format!("[{idx}]"));
            }
            cur = n.parent;
        }
        parts.reverse();
        if parts.is_empty() {
            ".".into()
        } else {
            parts.concat()
        }
    }

    pub fn value_text(&self, id: usize) -> String {
        serde_json::to_string_pretty(&self.values[id]).unwrap_or_default()
    }

    /// The next node after `from` (in document order, wrapping) whose key
    /// or scalar value contains `needle`; its ancestors are expanded.
    pub fn find(&mut self, needle: &str, from: usize) -> Option<usize> {
        let needle = needle.to_lowercase();
        let n = self.nodes.len();
        let hit = (1..=n).map(|d| (from + d) % n).find(|&i| {
            let node = &self.nodes[i];
            node.key
                .as_deref()
                .is_some_and(|k| k.to_lowercase().contains(&needle))
                || (node.kind == Kind::Scalar && node.text.to_lowercase().contains(&needle))
        })?;
        let mut cur = self.nodes[hit].parent;
        while let Some(p) = cur {
            self.nodes[p].expanded = true;
            cur = self.nodes[p].parent;
        }
        Some(hit)
    }

    fn line(&self, id: usize, width: usize, selected: bool) -> String {
        let n = &self.nodes[id];
        let indent = "  ".repeat(n.depth);
        let marker = match (n.kind, n.expanded) {
            (Kind::Scalar, _) => "  ",
            (_, true) => "▾ ",
            (_, false) => "▸ ",
        };
        let key = match (&n.key, n.index) {
            (Some(k), _) => format!("\x1b[34m{}\x1b[39m: ", Value::String(k.clone())),
            (None, Some(i)) => format!("\x1b[90m{i}:\x1b[39m "),
            _ => String::new(),
        };
        let value = match n.kind {
            Kind::Object if n.expanded => "{".to_string(),
            Kind::Array if n.expanded => "[".to_string(),
            Kind::Object => format!("\x1b[90m{{…}} {} keys\x1b[39m", n.text),
            Kind::Array => format!("\x1b[90m[…] {} items\x1b[39m", n.text),
            Kind::Scalar => {
                let color = match n.text.chars().next() {
                    Some('"') => 32,
                    Some('t' | 'f') => 33,
                    Some('n') => 90,
                    _ => 36,
                };
                format!("\x1b[{color}m{}\x1b[39m", n.text)
            }
        };
        let mut line = format!("{indent}{marker}{key}{value}");
        // Truncate by visible chars (escape sequences don't count).
        let mut visible = 0;
        let mut cut = line.len();
        let mut in_escape = false;
        for (i, ch) in line.char_indices() {
            if in_escape {
                in_escape = ch != 'm';
                continue;
            }
            if ch == '\x1b' {
                in_escape = true;
                continue;
            }
            visible += 1;
            if visible > width {
                cut = i;
                break;
            }
        }
        line.truncate(cut);
        if selected {
            format!("\x1b[7m{line}\x1b[27m\x1b[K")
        } else {
            format!("{line}\x1b[K")
        }
    }
}

/// Parses JSON, or JSON Lines (one value per line) as an array.
pub fn parse(text: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => Ok(v),
        Err(e) => {
            let lines: Result<Vec<Value>, _> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(serde_json::from_str)
                .collect();
            match lines {
                Ok(values) if !values.is_empty() => Ok(Value::Array(values)),
                _ => Err(format!("not JSON: {e}")),
            }
        }
    }
}

/// Whether text is worth offering the viewer for: an object or array.
pub fn looks_like_json(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('{') || t.starts_with('['))
        && parse(t).is_ok_and(|v| v.is_object() || v.is_array())
}

struct TtyGuard {
    tty: File,
    saved: libc::termios,
}

impl TtyGuard {
    fn new() -> io::Result<Self> {
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let fd = tty.as_raw_fd();
        // SAFETY: termios calls on an open terminal fd.
        let saved = unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
            saved
        };
        let mut guard = Self { tty, saved };
        guard.tty.write_all(b"\x1b[?1049h\x1b[?25l")?;
        Ok(guard)
    }

    fn size(&self) -> (usize, usize) {
        // SAFETY: TIOCGWINSZ fills a winsize.
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(self.tty.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                (ws.ws_col as usize, ws.ws_row as usize)
            } else {
                (80, 24)
            }
        }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        let _ = self.tty.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        // SAFETY: restores the saved termios.
        unsafe {
            libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.saved);
        }
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// Runs the viewer on a file (or stdin when `None` / `-`).
pub fn run(path: Option<&str>) -> io::Result<()> {
    let mut text = String::new();
    match path.filter(|p| *p != "-") {
        Some(p) => {
            File::open(p)?.read_to_string(&mut text)?;
        }
        None => {
            io::stdin().read_to_string(&mut text)?;
        }
    }
    let value = parse(&text).map_err(io::Error::other)?;
    let mut tree = Tree::new(value);
    let mut tty = TtyGuard::new()?;
    let (mut selected, mut top) = (0usize, 0usize);
    let mut search = String::new();
    let mut typing_search = false;
    let mut message = String::new();
    let mut buf = [0u8; 64];

    loop {
        let (cols, rows) = tty.size();
        let body = rows.saturating_sub(1).max(1);
        let visible = tree.visible();
        selected = selected.min(visible.len().saturating_sub(1));
        if selected < top {
            top = selected;
        } else if selected >= top + body {
            top = selected + 1 - body;
        }

        let mut out = String::from("\x1b[H");
        for row in 0..body {
            match visible.get(top + row) {
                Some(&id) => out.push_str(&tree.line(id, cols, top + row == selected)),
                None => out.push_str("\x1b[K"),
            }
            out.push_str("\r\n");
        }
        let status = if typing_search {
            format!("/{search}")
        } else if !message.is_empty() {
            std::mem::take(&mut message)
        } else {
            format!(
                " {}  ·  arrows/hjkl move · Enter fold · e/c all · / find · y path · Y value · q quit",
                tree.path(visible[selected])
            )
        };
        let status: String = status.chars().take(cols).collect();
        out.push_str(&format!("\x1b[7m{status}\x1b[K\x1b[0m"));
        tty.tty.write_all(out.as_bytes())?;
        tty.tty.flush()?;

        let n = tty.tty.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let keys = &buf[..n];
        let id = visible[selected];

        if typing_search {
            match keys {
                [b'\r'] | [b'\n'] => {
                    typing_search = false;
                    if let Some(hit) = tree.find(&search, id) {
                        selected = tree
                            .visible()
                            .iter()
                            .position(|&i| i == hit)
                            .unwrap_or(selected);
                    } else {
                        message = format!(" no match for {search:?}");
                    }
                }
                [0x1b] | [3] => typing_search = false,
                [0x7f] | [8] => {
                    search.pop();
                }
                bytes => search.push_str(&String::from_utf8_lossy(bytes)),
            }
            continue;
        }

        match keys {
            b"q" | [0x1b] | [3] => return Ok(()),
            b"j" | b"\x1b[B" | b"\x1bOB" => selected = (selected + 1).min(visible.len() - 1),
            b"k" | b"\x1b[A" | b"\x1bOA" => selected = selected.saturating_sub(1),
            b"g" | b"\x1b[H" => selected = 0,
            b"G" | b"\x1b[F" => selected = visible.len() - 1,
            b"\x1b[6~" => selected = (selected + body).min(visible.len() - 1),
            b"\x1b[5~" => selected = selected.saturating_sub(body),
            b"\r" | b" " => tree.toggle(id),
            b"l" | b"\x1b[C" | b"\x1bOC" => {
                if tree.nodes[id].kind != Kind::Scalar && !tree.nodes[id].expanded {
                    tree.toggle(id);
                } else {
                    selected = (selected + 1).min(visible.len() - 1);
                }
            }
            b"h" | b"\x1b[D" | b"\x1bOD" => {
                if tree.nodes[id].expanded && tree.nodes[id].kind != Kind::Scalar {
                    tree.toggle(id);
                } else if let Some(parent) = tree.nodes[id].parent {
                    selected = visible
                        .iter()
                        .position(|&i| i == parent)
                        .unwrap_or(selected);
                }
            }
            b"e" => tree.set_all(true),
            b"c" => {
                tree.set_all(false);
                selected = 0;
            }
            b"/" => {
                typing_search = true;
                search.clear();
            }
            b"n" if !search.is_empty() => {
                if let Some(hit) = tree.find(&search, id) {
                    selected = tree
                        .visible()
                        .iter()
                        .position(|&i| i == hit)
                        .unwrap_or(selected);
                }
            }
            b"y" | b"Y" => {
                let text = if keys == b"y" {
                    tree.path(id)
                } else {
                    tree.value_text(id)
                };
                tty.tty
                    .write_all(format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).as_bytes())?;
                message = format!(" copied {}", if keys == b"y" { "path" } else { "value" });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Tree {
        Tree::new(json!({
            "name": "cyberterm",
            "items": [{"id": 1, "tags": ["a"]}, {"id": 2, "weird key": null}],
            "ok": true
        }))
    }

    #[test]
    fn opens_two_levels_and_folds() {
        let mut t = sample();
        // root, name, items (expanded), items[0], items[1] (collapsed), ok
        assert_eq!(t.visible().len(), 6);
        t.set_all(true);
        assert_eq!(t.visible().len(), t.nodes.len());
        // Collapse-all keeps the top-level keys visible.
        t.set_all(false);
        assert_eq!(t.visible().len(), 4);
        t.toggle(0);
        assert_eq!(t.visible(), vec![0]);
    }

    #[test]
    fn paths_use_jq_syntax_and_keep_key_order() {
        let mut t = sample();
        t.set_all(true);
        let keys: Vec<_> = t.nodes[0]
            .children
            .iter()
            .map(|&c| t.nodes[c].key.clone().unwrap())
            .collect();
        assert_eq!(keys, vec!["name", "items", "ok"]);
        let paths: Vec<String> = (0..t.nodes.len()).map(|i| t.path(i)).collect();
        assert!(paths.contains(&".items[0].tags[0]".to_string()));
        assert!(paths.contains(&r#".items[1]["weird key"]"#.to_string()));
        assert_eq!(t.path(0), ".");
    }

    #[test]
    fn search_expands_to_the_match() {
        let mut t = sample();
        t.set_all(false);
        let hit = t.find("weird", 0).unwrap();
        assert!(t.visible().contains(&hit));
        assert_eq!(t.path(hit), r#".items[1]["weird key"]"#);
        assert!(t.find("nothing-like-this", 0).is_none());
    }

    #[test]
    fn json_lines_and_detection() {
        assert_eq!(
            parse("{\"a\":1}\n{\"a\":2}\n").unwrap(),
            json!([{"a":1},{"a":2}])
        );
        assert!(parse("nope").is_err());
        assert!(looks_like_json("  {\"x\": [1,2]}\n"));
        assert!(!looks_like_json("42"));
        assert!(!looks_like_json("[not json"));
    }
}
