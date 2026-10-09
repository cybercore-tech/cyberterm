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
}
