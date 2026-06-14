//! Path-token-overlap reranker with a trigram tiebreaker.
//!
//! Designed for "the file I want has these words in its path" queries
//! such as `bar widget`. The primary signal is the count of query
//! tokens that appear (substring) in the path's tokens after splitting
//! on `/`, `_`, `-`, `.`, and whitespace. Ties are broken first by
//! exact-case token overlap (so `Foo.rs` query prefers `src/Foo.rs` over
//! `src/foo.rs` on case-sensitive Linux filesystems), then by trigram
//! Jaccard similarity against the path's basename.

use crate::tokens::{TokenCase, path_token_separator, split_tokens, split_tokens_both};
use std::collections::HashSet;

/// Result of ranking a single path. Sort key is (`-overlap`,
/// `-exact_case_overlap`, `-trigram`, `index`) so callers can combine
/// multiple ranks without juggling orientations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathRank {
    /// Number of query tokens that appear in the path tokens (case-insensitive).
    pub overlap: u32,
    /// Number of query tokens that match a path token with exact case.
    /// Used as a secondary key to prefer `src/Foo.rs` over `src/foo.rs`
    /// when the query uses the uppercase form on case-sensitive filesystems.
    pub exact_case_overlap: u32,
    /// Trigram Jaccard similarity in `[0.0, 1.0]` against the basename.
    pub trigram: f32,
}

impl PathRank {
    /// Returns a tuple ordered best-first by Rust's natural ordering:
    /// higher `overlap` first, then higher `exact_case_overlap`, then
    /// higher `trigram` (negated so that `sort` puts the best result at
    /// index 0).
    pub fn sort_key(self) -> (i32, i32, i32) {
        // i32 negation keeps the result Ord-friendly; trigram is mapped
        // to a fixed-point integer to avoid NaN traps without pulling in
        // the `ordered-float` crate.
        let trigram_fp = (self.trigram.clamp(0.0, 1.0) * 10_000.0).round() as i32;
        (
            -(self.overlap as i32),
            -(self.exact_case_overlap as i32),
            -trigram_fp,
        )
    }
}

#[derive(Debug, Clone)]
struct PathQueryContext {
    /// Lowercased query tokens for case-insensitive overlap.
    tokens: Vec<String>,
    /// Case-preserving query tokens for exact-case boost.
    raw_tokens: Vec<String>,
    collapsed: String,
}

impl PathQueryContext {
    fn new(query: &str) -> Self {
        let tokens = path_tokens(query);
        let raw_tokens = path_tokens_preserving_case(query);
        let collapsed = tokens.join("");
        Self {
            tokens,
            raw_tokens,
            collapsed,
        }
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
            exact_case_overlap: 0,
            trigram: 0.0,
        };
    }
    // Build both lowercase and case-preserving path token vectors in a single
    // pass to avoid iterating over the path twice.
    let (path_token_vec, path_token_raw) = path_split_both(path);

    let overlap = context
        .tokens
        .iter()
        .filter(|q| {
            path_token_vec
                .iter()
                .any(|p| p == *q || p.contains(q.as_str()) || q.contains(p.as_str()))
        })
        .count() as u32;

    // Exact-case overlap: count how many raw (case-preserving) query tokens
    // appear verbatim in the path's case-preserving tokens.  This lets
    // `Foo.rs` rank above `foo.rs` when the query uses the uppercase form,
    // matching Linux case-sensitive filesystem semantics.
    let exact_case_overlap = context
        .raw_tokens
        .iter()
        .filter(|q| path_token_raw.iter().any(|p| p == *q))
        .count() as u32;

    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let trigram = trigram_jaccard(basename, &context.collapsed);

    PathRank {
        overlap,
        exact_case_overlap,
        trigram,
    }
}

/// Rank a slice of paths against `query`, returning `(index, PathRank)`
/// pairs sorted best-first.
///
/// Sort key priority: `(-overlap, -trigram)` → `path` (lexicographic) →
/// `original index`. The path tiebreaker keeps results deterministic on
/// Windows repos where two candidates may have equal rank scores but differ
/// only by case or separator representation.
pub fn rank_paths(paths: &[&str], query: &str) -> Vec<(usize, PathRank)> {
    let context = PathQueryContext::new(query);
    let mut scored: Vec<(usize, PathRank)> = paths
        .iter()
        .enumerate()
        .map(|(idx, path)| (idx, path_rank_with_context(path, &context)))
        .collect();
    scored.sort_by(|a, b| {
        a.1.sort_key()
            .cmp(&b.1.sort_key())
            .then_with(|| paths[a.0].cmp(paths[b.0]))
            .then(a.0.cmp(&b.0))
    });
    scored
}

/// Split `input` into lowercase tokens on `/`, `\`, `_`, `-`, `.`, and
/// whitespace. Empty tokens are dropped. Stays deliberately ASCII-only
/// in its separator set so behaviour mirrors what models type for
/// file-shaped queries.
fn path_tokens(input: &str) -> Vec<String> {
    split_tokens(input, path_token_separator, TokenCase::Lowercase)
}

/// Split `input` into tokens on `/`, `\`, `_`, `-`, `.`, and whitespace
/// without lowercasing.  Used for the exact-case overlap signal so that
/// `Foo.rs` in a query can match `Foo.rs` in a path but not `foo.rs`.
fn path_tokens_preserving_case(input: &str) -> Vec<String> {
    split_tokens(input, path_token_separator, TokenCase::Preserve)
}

/// Returns `(lowercase_tokens, case_preserving_tokens)` in a single pass,
/// avoiding two separate `path_split_raw` traversals per candidate path.
fn path_split_both(input: &str) -> (Vec<String>, Vec<String>) {
    split_tokens_both(input, path_token_separator)
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
