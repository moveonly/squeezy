//! BM25 reranker for multi-token natural-language queries.
//!
//! Tie-breaker only: callers run the graph's trigram prefilter first,
//! then apply this rerank when the query has 2+ whitespace-separated
//! tokens. Pure in-tree implementation — no upstream `bm25` crate so
//! we avoid the unmaintained `fxhash` advisory it pulls in.

use std::collections::HashMap;

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

    let doc_tokens: Vec<Vec<String>> = docs.iter().map(|doc| tokenize_doc(*doc)).collect();

    let n = doc_tokens.len() as f32;
    let avgdl = if n == 0.0 {
        0.0
    } else {
        doc_tokens.iter().map(|d| d.len() as f32).sum::<f32>() / n
    };

    // doc-frequency: how many docs contain each query token at least once
    let mut df: HashMap<&str, u32> = HashMap::new();
    for term in &query_tokens {
        let count = doc_tokens
            .iter()
            .filter(|tokens| tokens.iter().any(|t| t == term))
            .count() as u32;
        df.insert(term.as_str(), count);
    }

    let mut scored: Vec<(usize, f32)> = doc_tokens
        .iter()
        .enumerate()
        .map(|(idx, tokens)| {
            let dl = tokens.len() as f32;
            let length_norm = if avgdl > 0.0 { dl / avgdl } else { 1.0 };
            let mut tf: HashMap<&str, u32> = HashMap::new();
            for token in tokens {
                *tf.entry(token.as_str()).or_insert(0) += 1;
            }
            let score: f32 = query_tokens
                .iter()
                .map(|term| {
                    let term_tf = tf.get(term.as_str()).copied().unwrap_or(0) as f32;
                    if term_tf == 0.0 {
                        return 0.0;
                    }
                    let n_qi = df.get(term.as_str()).copied().unwrap_or(0) as f32;
                    let idf = ((n - n_qi + 0.5) / (n_qi + 0.5) + 1.0).ln();
                    idf * (term_tf * (K1 + 1.0)) / (term_tf + K1 * (1.0 - B + B * length_norm))
                })
                .sum();
            (idx, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_n);
    scored
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

fn tokenize_doc(doc: BM25Doc<'_>) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut visit = |token: &str| tokens.push(token.to_owned());
    for part in [doc.signature, doc.docs, doc.attributes] {
        tokenize_part(part, &mut current, &mut visit);
        flush_token(&mut current, &mut visit);
    }
    tokens
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
