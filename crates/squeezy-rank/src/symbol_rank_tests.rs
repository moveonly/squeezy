use super::*;

fn sym<'a>(name: &'a str, signature: &'a str) -> GraphSymbolView<'a> {
    GraphSymbolView { name, signature }
}

#[test]
fn fuzzy_symbol_rank_orders_camelcase() {
    let symbols = vec![
        sym("GlobMatcher", "fn glob()"),
        sym("PageManager", "struct PageManager"),
        sym("GraphManager", "struct GraphManager"),
    ];
    let ranked = rank_symbols(&symbols, "graphmgr");
    assert_eq!(symbols[ranked[0].0].name, "GraphManager");
}

#[test]
fn exact_wins_over_fuzzy() {
    let symbols = vec![
        sym("GraphMgrAlt", "fn alt()"),
        sym("GraphManager", "struct GraphManager"),
    ];
    let ranked = rank_symbols(&symbols, "GraphManager");
    assert_eq!(symbols[ranked[0].0].name, "GraphManager");
    assert_eq!(ranked[0].1, RankTier::Exact);
}

#[test]
fn case_insensitive_wins_over_substring() {
    let symbols = vec![
        sym("foo_graphmanager_bar", "fn baz()"),
        sym("graphmanager", "fn baz()"),
    ];
    let ranked = rank_symbols(&symbols, "GraphManager");
    assert_eq!(symbols[ranked[0].0].name, "graphmanager");
    assert_eq!(ranked[0].1, RankTier::CaseInsensitive);
}

#[test]
fn token_bag_matches_reordered_query() {
    let symbols = vec![sym("parse_rust_file", "fn parse_rust_file(...)")];
    let ranked = rank_symbols(&symbols, "rust parse");
    assert!(matches!(ranked[0].1, RankTier::TokenBag | RankTier::Fuzzy));
}

#[test]
fn no_match_returned_last() {
    let symbols = vec![sym("alpha", "fn alpha()"), sym("zzz_unrelated", "fn zzz()")];
    let ranked = rank_symbols(&symbols, "alpha");
    assert_eq!(symbols[ranked[0].0].name, "alpha");
    assert_eq!(ranked[1].1, RankTier::NoMatch);
}
