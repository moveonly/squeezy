//! Case-insensitive subsequence matcher with a prefix bonus.
//!
//! Scoring is case-insensitive, Unicode-correct, with a `-100` prefix
//! bonus that pulls exact-prefix matches above subsequence hits.

use crate::tokens::split_compound_identifier;

/// Score a haystack against a needle using a case-insensitive subsequence
/// match. Returns `None` if the needle's lowercase characters do not all
/// appear, in order, somewhere in the haystack. Lower is better.
///
/// Scoring:
/// - base score = byte distance between first and last matched character
/// - subtract `100` if the first matched character is at byte 0
/// - subtract `25` if the entire match is contiguous (no gaps between chars)
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    let needle_lower: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    fuzzy_score_with_lowercase_needle(haystack, &needle_lower)
}

pub(crate) fn fuzzy_score_with_lowercase_needle(
    haystack: &str,
    needle_lower: &[char],
) -> Option<i32> {
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
        let ch_end = byte_idx + ch.len_utf8();
        // Expand multi-char lowercase sequences (e.g. U+0130 İ → "i\u{307}")
        // and match each expanded char against consecutive needle positions.
        // Using `>` rather than `!=` for the contiguity check ensures that
        // two expanded chars originating from the same source position
        // (same byte_idx, both < ch_end) are not treated as a gap.
        for lower in ch.to_lowercase() {
            if needle_idx >= needle_lower.len() {
                break;
            }
            let target = needle_lower[needle_idx];
            if lower == target {
                if first_match.is_none() {
                    first_match = Some(byte_idx);
                }
                if let Some(prev) = prev_end
                    && byte_idx > prev
                {
                    contiguous = false;
                }
                last_match = byte_idx;
                prev_end = Some(ch_end);
                needle_idx += 1;
            }
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
    [full_score, base_score].into_iter().flatten().min()
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
    split_compound_identifier(name)
}

#[cfg(test)]
#[path = "fuzzy_tests.rs"]
mod tests;
