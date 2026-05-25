//! Case-insensitive subsequence matcher with a prefix bonus.
//!
//! Mirrors the *shape* of Codex's `utils/fuzzy-match` (case-insensitive
//! subsequence, Unicode-correct, `-100` prefix bonus). Implemented from
//! scratch; no upstream code is copied.

/// Score a haystack against a needle using a case-insensitive subsequence
/// match. Returns `None` if the needle's lowercase characters do not all
/// appear, in order, somewhere in the haystack. Lower is better.
///
/// Scoring:
/// - base score = byte distance between first and last matched character
/// - subtract `100` if the first matched character is at byte 0
/// - subtract `25` if the entire match is contiguous (no gaps between chars)
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle_lower: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle_lower.is_empty() {
        return Some(0);
    }

    let mut needle_idx = 0usize;
    let mut first_match: Option<usize> = None;
    let mut last_match: usize = 0;
    let mut prev_end: Option<usize> = None;
    let mut contiguous = true;

    for (byte_idx, ch) in haystack.char_indices() {
        if needle_idx >= needle_lower.len() {
            break;
        }
        let target = needle_lower[needle_idx];
        let lower = ch.to_lowercase().next().unwrap_or(ch);
        if lower == target {
            if first_match.is_none() {
                first_match = Some(byte_idx);
            }
            if let Some(prev) = prev_end
                && byte_idx != prev
            {
                contiguous = false;
            }
            last_match = byte_idx;
            prev_end = Some(byte_idx + ch.len_utf8());
            needle_idx += 1;
        }
    }

    if needle_idx < needle_lower.len() {
        return None;
    }

    let first = first_match.unwrap_or(0);
    let mut score = (last_match - first) as i32;
    if first == 0 {
        score -= 100;
    }
    if contiguous {
        score -= 25;
    }
    Some(score)
}

/// Score a path against a needle, biasing toward the trailing path segment
/// (basename) which is the common shape of "find a file" queries.
///
/// Separator-insensitive: `_`, `-`, and `/` in either the path or the
/// needle are treated as equivalent, so `squeezy_graph` matches
/// `crates/squeezy-graph/src/lib.rs`.
///
/// Returns the best (minimum) of the full-path score and the basename score.
pub fn fuzzy_path_score(path: &str, needle: &str) -> Option<i32> {
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let path_norm = normalize_path_separators(path);
    let base_norm = normalize_path_separators(basename);
    let needle_norm = normalize_path_separators(needle);
    let full_score = fuzzy_score(&path_norm, &needle_norm);
    let base_score = fuzzy_score(&base_norm, &needle_norm);
    match (full_score, base_score) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn normalize_path_separators(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '_' | '-' | '/' | '\\' => '_',
            other => other,
        })
        .collect()
}

/// Split an identifier into its camelCase / snake_case / kebab-case / dotted
/// tokens. ASCII-only word boundaries are detected; runs of upper-case
/// letters followed by lower-case (e.g. `XMLParser`) split into `XML` +
/// `Parser`. Returns lowercase tokens.
pub fn camel_snake_split(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = name.chars().collect();

    for (i, &ch) in chars.iter().enumerate() {
        let is_sep = matches!(ch, '_' | '-' | '/' | '.' | ' ' | ':');
        if is_sep {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }

        let prev = if i > 0 {
            chars.get(i - 1).copied()
        } else {
            None
        };
        let next = chars.get(i + 1).copied();

        let camel_boundary = match (prev, ch) {
            (Some(p), c) if p.is_ascii_lowercase() && c.is_ascii_uppercase() => true,
            (Some(p), c)
                if p.is_ascii_uppercase()
                    && c.is_ascii_uppercase()
                    && matches!(next, Some(n) if n.is_ascii_lowercase()) =>
            {
                true
            }
            (Some(p), c) if p.is_ascii_alphabetic() && c.is_ascii_digit() => true,
            (Some(p), c) if p.is_ascii_digit() && c.is_ascii_alphabetic() => true,
            _ => false,
        };

        if camel_boundary && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
        .into_iter()
        .map(|tok| tok.to_lowercase())
        .filter(|tok| !tok.is_empty())
        .collect()
}

#[cfg(test)]
#[path = "fuzzy_tests.rs"]
mod tests;
