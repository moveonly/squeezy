//! Cross-module integration tests for `squeezy-rank`.

use super::*;

#[test]
fn crate_facade_reexports_are_stable() {
    let v: Vec<String> = fuzzy::camel_snake_split("GraphManager");
    assert_eq!(v, vec!["graph", "manager"]);
    let view = symbol_rank::GraphSymbolView {
        name: "GraphManager",
        signature: "struct GraphManager",
    };
    let (tier, _) = symbol_rank::rank_symbol(view, "GraphManager");
    assert_eq!(tier, symbol_rank::RankTier::Exact);
}

#[test]
fn rank_symbols_and_bm25_compose_for_multiword_queries() {
    let symbols = vec![
        symbol_rank::GraphSymbolView {
            name: "parse_rust_file",
            signature: "fn parse_rust_file(path: &Path)",
        },
        symbol_rank::GraphSymbolView {
            name: "rust_file_writer",
            signature: "fn rust_file_writer(path: &Path)",
        },
    ];
    let ranked = symbol_rank::rank_symbols(&symbols, "parse rust file");

    let docs: Vec<bm25_rank::BM25Doc<'_>> = ranked
        .iter()
        .map(|(idx, _, _)| bm25_rank::BM25Doc {
            signature: symbols[*idx].signature,
            docs: "",
            attributes: "",
        })
        .collect();
    let reranked = bm25_rank::bm25_rerank(&docs, "parse rust file", docs.len());
    assert!(!reranked.is_empty());
    let winner_idx = ranked[reranked[0].0].0;
    assert_eq!(symbols[winner_idx].name, "parse_rust_file");
}
