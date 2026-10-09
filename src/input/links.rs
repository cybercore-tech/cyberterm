// src/input/links.rs
//
// Plain-text URL detection for hover underline + Ctrl+click, alongside real
// OSC 8 hyperlinks (which carry their own URI).

use std::ops::Range;

const SCHEMES: &[&str] = &["https://", "http://", "file://", "ftp://", "mailto:"];

/// Schemes Ctrl+click will hand to the desktop opener. Anything else
/// (an OSC 8 link can claim any URI) is refused.
pub fn openable(uri: &str) -> bool {
    SCHEMES.iter().any(|s| uri.starts_with(s))
}

/// Finds a URL in `text` covering the char at `index`, returning its char
/// range and the URL.
pub fn url_at(text: &str, index: usize) -> Option<(Range<usize>, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0;
    while start < chars.len() {
        let rest: String = chars[start..].iter().take(8).collect();
        if !SCHEMES.iter().any(|s| rest.starts_with(s)) || boundary_before(&chars, start) {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < chars.len() && !stops_url(chars[end]) {
            end += 1;
        }
        // Trailing punctuation usually belongs to the sentence, except a
        // closing paren balancing one inside the URL (Wikipedia links).
        while end > start {
            let last = chars[end - 1];
            let opens = chars[start..end].iter().filter(|c| **c == '(').count();
            let closes = chars[start..end].iter().filter(|c| **c == ')').count();
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | ']' | '}')
                || (last == ')' && closes > opens)
            {
                end -= 1;
            } else {
                break;
            }
        }
        let url: String = chars[start..end].iter().collect();
        let bare_scheme = SCHEMES.iter().any(|s| url == *s);
        if (start..end).contains(&index) && !bare_scheme {
            return Some((start..end, url));
        }
        start = end.max(start + 1);
    }
    None
}

fn boundary_before(chars: &[char], start: usize) -> bool {
    start > 0 && chars[start - 1].is_alphanumeric()
}

fn stops_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '│' | '\u{0}')
}

/// A `path:line[:col]` reference found in output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRef {
    pub path: String,
    pub line: u32,
    pub col: Option<u32>,
}

/// Finds a file reference covering the char at `index`: `src/a.rs:12:5`
/// (compilers, linters, grep -n, Rust's `-->`, stack traces), Python's
/// `File "x.py", line 12`, and MSVC-style `file.cs(12,5)`. The path must
/// look like one (an extension or a slash); whether it exists is for the
/// caller to check, relative to the right directory.
pub fn file_ref_at(text: &str, index: usize) -> Option<(Range<usize>, FileRef)> {
    use regex::Regex;
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let path = r"(?:~|\.{1,2})?/?(?:[\w.+@-]+/)*[\w+@-][\w.+@-]*";
        [
            r#"File "(?P<path>[^"]+)", line (?P<line>\d+)"#.to_string(),
            format!(r"(?P<path>{path})\((?P<line>\d+)(?:,(?P<col>\d+))?\)"),
            format!(r"(?P<path>{path}):(?P<line>\d+)(?::(?P<col>\d+))?"),
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    });
    // Byte offset of the char at `index`.
    let byte = text.char_indices().nth(index)?.0;
    let to_chars = |b: usize| text[..b].chars().count();
    for re in patterns {
        for caps in re.captures_iter(text) {
            let whole = caps.get(0)?;
            if !(whole.start()..whole.end()).contains(&byte) {
                continue;
            }
            let path = caps.name("path")?.as_str();
            let looks_like_path = path.contains('/')
                || path.rsplit_once('.').is_some_and(|(stem, ext)| {
                    !stem.is_empty()
                        && !ext.is_empty()
                        && ext.chars().all(|c| c.is_ascii_alphanumeric())
                });
            // Not part of a URL (`http://host:80`): no `://` earlier in the
            // same word.
            let word_start = text[..whole.start()]
                .rfind(char::is_whitespace)
                .map_or(0, |i| i + 1);
            let in_url = text[word_start..whole.end()].contains("://");
            if !looks_like_path || in_url {
                continue;
            }
            let line = caps.name("line")?.as_str().parse().ok()?;
            let col = caps.name("col").and_then(|c| c.as_str().parse().ok());
            return Some((
                to_chars(whole.start())..to_chars(whole.end()),
                FileRef {
                    path: path.to_string(),
                    line,
                    col,
                },
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_url_under_the_pointer() {
        let text = "see https://example.com/a?b=1 for details";
        let (range, url) = url_at(text, 10).unwrap();
        assert_eq!(url, "https://example.com/a?b=1");
        assert_eq!(range, 4..29);
        assert!(url_at(text, 2).is_none());
        assert!(url_at(text, 31).is_none());
    }

    #[test]
    fn trims_sentence_punctuation_but_keeps_balanced_parens() {
        assert_eq!(
            url_at("go to http://x.org/page.", 8).unwrap().1,
            "http://x.org/page"
        );
        assert_eq!(
            url_at("(https://en.wikipedia.org/wiki/Rust_(language))", 5)
                .unwrap()
                .1,
            "https://en.wikipedia.org/wiki/Rust_(language)"
        );
    }

    #[test]
    fn handles_multiple_urls_and_unicode() {
        let text = "é http://a.io and file:///tmp/x";
        assert_eq!(url_at(text, 3).unwrap().1, "http://a.io");
        assert_eq!(url_at(text, 20).unwrap().1, "file:///tmp/x");
    }

    #[test]
    fn only_known_schemes_are_openable() {
        assert!(openable("https://x"));
        assert!(openable("mailto:me@x"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("cyberterm-mark:prompt"));
    }

    fn file_ref(text: &str, at: &str) -> Option<FileRef> {
        let index = text[..text.find(at).unwrap()].chars().count();
        file_ref_at(text, index).map(|(_, r)| r)
    }

    #[test]
    fn finds_compiler_and_linter_references() {
        let r = file_ref("error[E0308]: --> src/app/mod.rs:42:17", "mod.rs").unwrap();
        assert_eq!(
            r,
            FileRef {
                path: "src/app/mod.rs".into(),
                line: 42,
                col: Some(17)
            }
        );
        let r = file_ref("main.go:12: undefined: x", "main").unwrap();
        assert_eq!((r.path.as_str(), r.line, r.col), ("main.go", 12, None));
        let r = file_ref("  at render (/home/me/app/ui.js:10:5)", "ui.js").unwrap();
        assert_eq!(r.path, "/home/me/app/ui.js");
        let r = file_ref("grep: ./notes/todo.md:3:fix it", "todo").unwrap();
        assert_eq!((r.path.as_str(), r.line), ("./notes/todo.md", 3));
        let r = file_ref("Program.cs(12,5): error CS1002", "Program").unwrap();
        assert_eq!((r.line, r.col), (12, Some(5)));
    }

    #[test]
    fn finds_python_tracebacks() {
        let text = r#"  File "/srv/app/views.py", line 88, in handler"#;
        let r = file_ref(text, "views").unwrap();
        assert_eq!((r.path.as_str(), r.line), ("/srv/app/views.py", 88));
    }

    #[test]
    fn ignores_things_that_are_not_paths() {
        assert!(file_ref("at 12:30:45 today", "30").is_none());
        assert!(file_ref("see http://example.com:8080/x", "example").is_none());
        assert!(file_ref("ratio 3:2", "3").is_none());
        // Range covers just the reference.
        let text = "warning in lib.rs:7 here";
        let (range, _) = file_ref_at(text, 12).unwrap();
        assert_eq!(&text[range], "lib.rs:7");
    }
}
