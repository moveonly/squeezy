//! Multi-tier symbol ranking that complements (never replaces) the graph.
//!
//! The tier order mirrors `squeezy_tools::symbol_rank`'s existing ladder:
//! exact > case-insensitive > signature substring. We add a fourth tier
//! (token-bag match across camel/snake-split tokens) and a fifth (fuzzy
//! subsequence) for casual queries like `graphmgr → GraphManager`.

use crate::fuzzy::{camel_snake_split, fuzzy_score_with_lowercase_needle};

/// Borrow-only view over a graph symbol. Keeps this crate's surface free
/// of any dependency on `squeezy-graph`.
#[derive(Debug, Clone, Copy)]
pub struct GraphSymbolView<'a> {
    pub name: &'a str,
    pub signature: &'a str,
}

/// Ranking tier produced by `rank_symbol`. Lower is better. The numeric
/// values are part of the public contract because callers compose them
/// into composite sort keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RankTier {
    Exact = 0,
    CaseInsensitive = 1,
    SignatureSubstring = 2,
    TokenBag = 3,
    Fuzzy = 4,
    NoMatch = 5,
}

impl RankTier {
    pub fn as_usize(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone)]
struct SymbolQueryContext<'query> {
    raw: &'query str,
    token_bag_tokens: Vec<String>,
    fuzzy_needle_lower: Vec<char>,
}

impl<'query> SymbolQueryContext<'query> {
    fn new(query: &'query str) -> Self {
        Self {
            raw: query,
            token_bag_tokens: camel_snake_split(query),
            fuzzy_needle_lower: query.chars().flat_map(char::to_lowercase).collect(),
        }
    }
}

/// Score a single symbol against a query, returning (tier, lexical_score).
/// `lexical_score` is meaningful only in fuzzy/token tiers; callers should
/// sort primarily by tier and use the score as a secondary key.
pub fn rank_symbol(symbol: GraphSymbolView<'_>, query: &str) -> (RankTier, i32) {
    let context = SymbolQueryContext::new(query);
    rank_symbol_with_context(symbol, &context)
}

fn rank_symbol_with_context(
    symbol: GraphSymbolView<'_>,
    context: &SymbolQueryContext<'_>,
) -> (RankTier, i32) {
    if symbol.name == context.raw {
        return (RankTier::Exact, 0);
    }
    if symbol.name.eq_ignore_ascii_case(context.raw) {
        return (RankTier::CaseInsensitive, 0);
    }
    if symbol.signature.contains(context.raw) {
        return (RankTier::SignatureSubstring, 0);
    }
    if token_bag_match_with_query_tokens(symbol.name, &context.token_bag_tokens) {
        return (RankTier::TokenBag, 0);
    }
    if let Some(score) = fuzzy_score_with_lowercase_needle(symbol.name, &context.fuzzy_needle_lower)
    {
        return (RankTier::Fuzzy, score);
    }
    (RankTier::NoMatch, i32::MAX)
}

/// Rank all symbols, returning `(index, tier, score)` sorted best-first.
pub fn rank_symbols(symbols: &[GraphSymbolView<'_>], query: &str) -> Vec<(usize, RankTier, i32)> {
    let context = SymbolQueryContext::new(query);
    let mut scored: Vec<(usize, RankTier, i32)> = symbols
        .iter()
        .enumerate()
        .map(|(idx, sym)| {
            let (tier, score) = rank_symbol_with_context(*sym, &context);
            (idx, tier, score)
        })
        .collect();
    scored.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
    scored
}

fn token_bag_match_with_query_tokens(name: &str, query_tokens: &[String]) -> bool {
    if query_tokens.is_empty() {
        return false;
    }
    let name_tokens = camel_snake_split(name);
    if name_tokens.is_empty() {
        return false;
    }
    query_tokens.iter().all(|qt| {
        name_tokens
            .iter()
            .any(|nt| nt.contains(qt.as_str()) || qt.contains(nt.as_str()))
    })
}

#[cfg(test)]
#[path = "symbol_rank_tests.rs"]
mod tests;
