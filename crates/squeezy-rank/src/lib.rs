//! Lexical reranking complements for the semantic graph.
//!
//! The graph is the moat; this crate adds cheap lexical tiebreakers
//! (fuzzy subsequence, camel/snake token bag, BM25) that recover
//! near-miss queries without changing high-confidence semantics.

pub mod bm25_rank;
pub mod fuzzy;
pub mod path_rank;
pub mod symbol_rank;

pub use bm25_rank::{BM25Doc, bm25_rerank};
pub use fuzzy::{camel_snake_split, fuzzy_path_score, fuzzy_score};
pub use path_rank::{PathRank, path_rank, rank_paths};
pub use symbol_rank::{GraphSymbolView, RankTier, rank_symbols};

pub const CRATE_NAME: &str = "squeezy-rank";

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
