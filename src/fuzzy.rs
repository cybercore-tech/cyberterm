// src/fuzzy.rs
//
// Fuzzy matching for the command palette: the query's characters must
// appear in order in the text (smart case: a query with no capitals
// ignores case). Matches score higher when they're consecutive, start
// words, or start the text, and lower the more they're spread out.

/// A match: its score (higher is better) and the matched char indices.
pub fn fuzzy(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    let query: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let fold = !query.iter().any(|c| c.is_uppercase());
    let norm = |c: char| {
        if fold {
            c.to_lowercase().next().unwrap_or(c)
        } else {
            c
        }
    };
    let chars: Vec<char> = text.chars().collect();

    // Greedy left-to-right match, then a second pass from the end to
    // tighten it (prefers "split" in "split right" over scattered hits).
    let mut positions = Vec::with_capacity(query.len());
    let mut qi = 0;
    for (i, &c) in chars.iter().enumerate() {
        if qi < query.len() && norm(c) == norm(query[qi]) {
            positions.push(i);
            qi += 1;
        }
    }
    if qi < query.len() {
        return None;
    }
    let mut end = *positions.last()?;
    let mut tight = vec![0; query.len()];
    let mut qi = query.len();
    let mut i = end as isize;
    while qi > 0 && i >= 0 {
        if norm(chars[i as usize]) == norm(query[qi - 1]) {
            qi -= 1;
            tight[qi] = i as usize;
        }
        i -= 1;
    }
    if qi == 0 {
        positions = tight;
        end = *positions.last()?;
    }

    let is_boundary = |i: usize| {
        i == 0 || {
            let prev = chars[i - 1];
            !prev.is_alphanumeric() || (prev.is_lowercase() && chars[i].is_uppercase())
        }
    };
    let mut score: i64 = 0;
    for (k, &p) in positions.iter().enumerate() {
        score += 10;
        if is_boundary(p) {
            score += 15;
        }
        if k > 0 && positions[k - 1] + 1 == p {
            score += 20;
        }
    }
    if positions[0] == 0 {
        score += 25;
    }
    let spread = (end - positions[0] + 1 - positions.len()) as i64;
    score -= spread * 2;
    score -= chars.len() as i64 / 8;
    Some((score, positions))
}

#[cfg(test)]
mod tests {
    use super::fuzzy;

    #[test]
    fn subsequences_match_and_others_dont() {
        assert!(fuzzy("spr", "Split Right").is_some());
        assert!(fuzzy("xyz", "Split Right").is_none());
        assert_eq!(fuzzy("", "anything").unwrap().0, 0);
    }

    #[test]
    fn smart_case() {
        assert!(fuzzy("split", "Split Right").is_some());
        assert!(fuzzy("Split", "split right").is_none());
    }

    #[test]
    fn word_starts_and_runs_rank_higher() {
        let a = fuzzy("sr", "Split Right").unwrap().0;
        let b = fuzzy("sr", "Search Scrollback").unwrap().0;
        assert!(a > b, "{a} vs {b}");
        let run = fuzzy("theme", "Theme: tokyo_grid").unwrap().0;
        let scattered = fuzzy("theme", "Toggle Highlight Everything Mode Extra")
            .unwrap()
            .0;
        assert!(run > scattered);
    }

    #[test]
    fn positions_are_tightened() {
        let (_, pos) = fuzzy("ab", "a x a b").unwrap();
        assert_eq!(pos, vec![4, 6]);
    }
}
