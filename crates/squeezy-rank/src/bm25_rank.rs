//! BM25 reranker for multi-token natural-language queries.
//!
//! Tie-breaker only: callers run the graph's trigram prefilter first,
//! then apply this rerank when the query has 2+ whitespace-separated
//! tokens. Mirrors the pattern Codex uses in
//! `core/src/tools/handlers/tool_search.rs`.

use bm25::{Document, Language, SearchEngineBuilder};

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
    let documents: Vec<Document<usize>> = docs
        .iter()
        .enumerate()
        .map(|(idx, doc)| {
            let body = normalize_identifier_text(&format!(
                "{} {} {}",
                doc.signature, doc.docs, doc.attributes
            ));
            Document::new(idx, body)
        })
        .collect();
    let engine = SearchEngineBuilder::<usize>::with_documents(Language::English, documents).build();
    let results = engine.search(&normalize_identifier_text(query), top_n);
    results
        .into_iter()
        .map(|hit| (hit.document.id, hit.score))
        .collect()
}

/// Replace identifier separators (`_`, `-`, `/`, `.`, `:`, `(`, `)`, `<`, `>`,
/// `&`, `,`, `;`) with whitespace so the BM25 tokenizer sees natural words.
/// CamelCase boundaries are not split here; the token-bag tier in
/// [`super::symbol_rank`] handles those.
fn normalize_identifier_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(
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
        ) {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
#[path = "bm25_rank_tests.rs"]
mod tests;
