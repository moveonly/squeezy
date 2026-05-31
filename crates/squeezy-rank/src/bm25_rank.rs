//! BM25 reranker for multi-token natural-language queries.
//!
//! Tie-breaker only: callers run the graph's trigram prefilter first,
//! then apply this rerank when the query has 2+ whitespace-separated
//! tokens. Pure in-tree implementation — no upstream `bm25` crate so
//! we avoid the unmaintained `fxhash` advisory it pulls in.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

/// BM25 saturation parameter. 1.2 is the standard textbook value and
/// matches what the `bm25` crate uses internally.
const K1: f32 = 1.2;
/// BM25 length-normalisation parameter. 0.75 is the standard value.
const B: f32 = 0.75;

/// Borrow-only view of a graph symbol's lexical surface. The corpus is
/// built from the concatenation of `signature`, `docs`, and `attributes`.
#[derive(Debug, Clone, Copy)]
pub struct BM25Doc<'a> {
    pub signature: &'a str,
    pub docs: &'a str,
    pub attributes: &'a str,
}

#[derive(Debug)]
struct BM25DocStats<'query> {
    token_count: usize,
    term_frequency: HashMap<&'query str, u32>,
}

/// Rerank `docs` against `query`, returning up to `top_n` `(index, score)`
/// pairs sorted best-first (higher BM25 score wins).
pub fn bm25_rerank(docs: &[BM25Doc<'_>], query: &str, top_n: usize) -> Vec<(usize, f32)> {
    if docs.is_empty() || top_n == 0 {
        return Vec::new();
    }
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return Vec::new();
    }

    let query_terms: HashSet<&str> = query_tokens.iter().map(String::as_str).collect();
    let mut df: HashMap<&str, u32> = HashMap::with_capacity(query_terms.len());
    let mut total_doc_len = 0usize;
    let doc_stats: Vec<BM25DocStats<'_>> = docs
        .iter()
        .map(|doc| {
            let stats = collect_doc_stats(*doc, &query_terms, &mut df);
            total_doc_len += stats.token_count;
            stats
        })
        .collect();

    let n = doc_stats.len() as f32;
    let avgdl = if n == 0.0 {
        0.0
    } else {
        total_doc_len as f32 / n
    };

    let mut scored: Vec<(usize, f32)> = doc_stats
        .iter()
        .enumerate()
        .map(|(idx, stats)| {
            let dl = stats.token_count as f32;
            let length_norm = if avgdl > 0.0 { dl / avgdl } else { 1.0 };
            let score: f32 = query_tokens
                .iter()
                .map(|term| {
                    let term = term.as_str();
                    let term_tf = stats.term_frequency.get(term).copied().unwrap_or(0) as f32;
                    if term_tf == 0.0 {
                        return 0.0;
                    }
                    let n_qi = df.get(term).copied().unwrap_or(0) as f32;
                    let idf = ((n - n_qi + 0.5) / (n_qi + 0.5) + 1.0).ln();
                    idf * (term_tf * (K1 + 1.0)) / (term_tf + K1 * (1.0 - B + B * length_norm))
                })
                .sum();
            (idx, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();

    sort_top_scores(&mut scored, top_n);
    scored
}

fn sort_top_scores(scored: &mut Vec<(usize, f32)>, top_n: usize) {
    if scored.len() > top_n {
        scored.select_nth_unstable_by(top_n, compare_bm25_scores);
        scored.truncate(top_n);
    }
    scored.sort_by(compare_bm25_scores);
}

fn compare_bm25_scores(a: &(usize, f32), b: &(usize, f32)) -> Ordering {
    b.1.partial_cmp(&a.1)
        .unwrap_or(Ordering::Equal)
        .then(a.0.cmp(&b.0))
}

fn collect_doc_stats<'query>(
    doc: BM25Doc<'_>,
    query_terms: &HashSet<&'query str>,
    df: &mut HashMap<&'query str, u32>,
) -> BM25DocStats<'query> {
    let mut stats = BM25DocStats {
        token_count: 0,
        term_frequency: HashMap::new(),
    };
    let mut seen_terms = HashSet::new();

    for_each_doc_token(doc, |token| {
        stats.token_count += 1;
        if let Some(&term) = query_terms.get(token) {
            *stats.term_frequency.entry(term).or_insert(0) += 1;
            if seen_terms.insert(term) {
                *df.entry(term).or_insert(0) += 1;
            }
        }
    });

    stats
}

/// Split on whitespace and identifier separators (`_`, `-`, `/`, `.`,
/// `:`, parens, brackets, quotes, etc.), lowercasing each token. CamelCase
/// is not split — the token-bag tier in [`super::symbol_rank`] handles that.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut visit = |token: &str| tokens.push(token.to_owned());
    tokenize_part(input, &mut current, &mut visit);
    flush_token(&mut current, &mut visit);
    tokens
}

fn for_each_doc_token(doc: BM25Doc<'_>, mut visit: impl FnMut(&str)) {
    let mut current = String::new();
    for part in [doc.signature, doc.docs, doc.attributes] {
        tokenize_part(part, &mut current, &mut visit);
        flush_token(&mut current, &mut visit);
    }
}

fn tokenize_part(input: &str, current: &mut String, visit: &mut impl FnMut(&str)) {
    for ch in input.chars() {
        let is_sep = ch.is_whitespace()
            || matches!(
                ch,
                '_' | '-'
                    | '/'
                    | '.'
                    | ':'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '&'
                    | ','
                    | ';'
                    | '\''
                    | '"'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '!'
                    | '?'
                    | '*'
                    | '='
                    | '|'
                    | '@'
                    | '#'
                    | '$'
                    | '%'
                    | '^'
                    | '~'
                    | '`'
                    | '+'
                    | '\\'
            );
        if is_sep {
            flush_token(current, visit);
            continue;
        }
        current.extend(ch.to_lowercase());
    }
}

fn flush_token(current: &mut String, visit: &mut impl FnMut(&str)) {
    if !current.is_empty() {
        visit(current);
        current.clear();
    }
}

#[cfg(test)]
#[path = "bm25_rank_tests.rs"]
mod tests;
