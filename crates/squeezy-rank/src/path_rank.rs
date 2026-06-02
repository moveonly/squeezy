//! Path-token-overlap reranker with a trigram tiebreaker.
//!
//! Designed for "the file I want has these words in its path" queries
//! such as `bar widget`. The primary signal is the count of query
//! tokens that appear (substring) in the path's tokens after splitting
//! on `/`, `_`, `-`, `.`, and whitespace. Ties are broken by trigram
//! Jaccard similarity against the path's basename, which is a stable
//! lexical proxy when overlap counts are equal.

use std::collections::HashSet;

/// Result of ranking a single path. Sort key is (`-overlap`, `-trigram`,
/// `path`) so callers can combine multiple ranks without juggling
/// orientations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathRank {
    /// Number of query tokens that appear in the path tokens.
    pub overlap: u32,
    /// Trigram Jaccard similarity in `[0.0, 1.0]` against the basename.
    pub trigram: f32,
}

impl PathRank {
    /// Returns a tuple ordered best-first by Rust's natural ordering:
    /// higher `overlap` first, then higher `trigram` (negated so that
    /// `sort` puts the best result at index 0).
    pub fn sort_key(self) -> (i32, i32) {
        // i32 negation keeps the result Ord-friendly; trigram is mapped
        // to a fixed-point integer to avoid NaN traps without pulling in
        // the `ordered-float` crate.
        let trigram_fp = (self.trigram.clamp(0.0, 1.0) * 10_000.0).round() as i32;
        (-(self.overlap as i32), -trigram_fp)
    }
}

#[derive(Debug, Clone)]
struct PathQueryContext {
    tokens: Vec<String>,
    collapsed: String,
}

impl PathQueryContext {
    fn new(query: &str) -> Self {
        let tokens = path_tokens(query);
        let collapsed = tokens.join("");
        Self { tokens, collapsed }
    }
}

/// Score `path` against `query`. `query` is tokenised the same way as
/// the path (split on `/`, `_`, `-`, `.`, whitespace) and lowercased.
/// Empty queries score zero overlap and zero trigram similarity.
pub fn path_rank(path: &str, query: &str) -> PathRank {
    let context = PathQueryContext::new(query);
    path_rank_with_context(path, &context)
}

fn path_rank_with_context(path: &str, context: &PathQueryContext) -> PathRank {
    if context.tokens.is_empty() {
        return PathRank {
            overlap: 0,
            trigram: 0.0,
        };
    }
    let path_token_set: HashSet<String> = path_tokens(path).into_iter().collect();
    let overlap = context
        .tokens
        .iter()
        .filter(|q| {
            path_token_set
                .iter()
                .any(|p| p == *q || p.contains(q.as_str()) || q.contains(p.as_str()))
        })
        .count() as u32;

    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let trigram = trigram_jaccard(basename, &context.collapsed);

    PathRank { overlap, trigram }
}

/// Rank a slice of paths against `query`, returning `(index, PathRank)`
/// pairs sorted best-first. Stable: paths with identical ranks keep
/// their original relative order.
pub fn rank_paths(paths: &[&str], query: &str) -> Vec<(usize, PathRank)> {
    let context = PathQueryContext::new(query);
    let mut scored: Vec<(usize, PathRank)> = paths
        .iter()
        .enumerate()
        .map(|(idx, path)| (idx, path_rank_with_context(path, &context)))
        .collect();
    scored.sort_by(|a, b| a.1.sort_key().cmp(&b.1.sort_key()).then(a.0.cmp(&b.0)));
    scored
}

/// Split `input` into lowercase tokens on `/`, `\`, `_`, `-`, `.`, and
/// whitespace. Empty tokens are dropped. Stays deliberately ASCII-only
/// in its separator set so behaviour mirrors what models type for
/// file-shaped queries.
fn path_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        let is_sep = matches!(ch, '/' | '\\' | '_' | '-' | '.') || ch.is_whitespace();
        if is_sep {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.extend(ch.to_lowercase());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Jaccard similarity over the character-trigram sets of `a` and `b`,
/// lowercased. Returns 0.0 when either side has no trigrams (strings
/// shorter than 3 characters), matching the conservative behaviour of
/// most BM25-adjacent similarity functions.
fn trigram_jaccard(a: &str, b: &str) -> f32 {
    let set_a = trigrams(a);
    let set_b = trigrams(b);
    if set_a.is_empty() || set_b.is_empty() {
        return 0.0;
    }
    let inter = set_a.intersection(&set_b).count() as f32;
    let union = set_a.union(&set_b).count() as f32;
    if union == 0.0 { 0.0 } else { inter / union }
}

fn trigrams(input: &str) -> HashSet<String> {
    let chars: Vec<char> = input
        .chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| !c.is_whitespace())
        .collect();
    if chars.len() < 3 {
        return HashSet::new();
    }
    chars
        .windows(3)
        .map(|w| w.iter().collect::<String>())
        .collect()
}

#[cfg(test)]
#[path = "path_rank_tests.rs"]
mod tests;
