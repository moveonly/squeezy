use super::*;

#[test]
fn fuzzy_matches_subsequence_case_insensitive() {
    assert!(fuzzy_score("GraphManager", "gm").is_some());
    assert!(fuzzy_score("GraphManager", "graphmgr").is_some());
    assert!(fuzzy_score("GraphManager", "xyz").is_none());
}

#[test]
fn fuzzy_rewards_prefix_match() {
    let prefix = fuzzy_score("GraphManager", "graph").expect("matches");
    let non_prefix = fuzzy_score("MyGraphManager", "graph").expect("matches");
    assert!(
        prefix < non_prefix,
        "prefix={prefix} non_prefix={non_prefix}"
    );
}

#[test]
fn fuzzy_path_prefers_basename() {
    let a = fuzzy_path_score("crates/squeezy-graph/src/lib.rs", "lib").expect("matches");
    let b = fuzzy_path_score("crates/squeezy-libfoo/src/main.rs", "lib").expect("matches");
    assert!(
        a <= b,
        "basename match should be at least as good ({a} <= {b})"
    );
}

#[test]
fn fuzzy_no_match_returns_none() {
    assert!(fuzzy_score("abc", "xyz").is_none());
    assert!(fuzzy_path_score("path/to/file.rs", "zzz").is_none());
}

#[test]
fn camel_snake_split_breaks_compound_identifiers() {
    assert_eq!(camel_snake_split("GraphManager"), vec!["graph", "manager"]);
    assert_eq!(
        camel_snake_split("parse_rust_file"),
        vec!["parse", "rust", "file"]
    );
    assert_eq!(
        camel_snake_split("crates/squeezy-graph/src/lib.rs"),
        vec!["crates", "squeezy", "graph", "src", "lib", "rs"]
    );
    assert_eq!(camel_snake_split("XMLParser"), vec!["xml", "parser"]);
}

#[test]
fn camel_snake_split_handles_digits() {
    assert_eq!(camel_snake_split("BM25Rerank"), vec!["bm", "25", "rerank"]);
}
