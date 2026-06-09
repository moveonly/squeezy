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

#[test]
fn fuzzy_score_handles_multichar_lowercase_expansion() {
    // U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE lowercases to two code
    // points: U+0069 (i) + U+0307 (combining dot above).  The scorer must
    // match both expanded chars against consecutive needle positions rather
    // than silently dropping the second expansion char.
    //
    // Expected score: İ occupies bytes 0–1 (2 bytes); s=2, t=3, a=4, n=5,
    // b=6, u=7, l=8.  All 9 needle chars matched contiguously starting at
    // byte 0.  Score = last_match(8) − first_match(0) − prefix_bonus(100)
    // − contiguous_bonus(25) = 8 − 100 − 25 = −117.
    let score = fuzzy_score("\u{0130}stanbul", "i\u{0307}stanbul");
    assert_eq!(
        score,
        Some(-117),
        "multi-char lowercase expansion: distance=8, prefix−100, contiguous−25"
    );
}

#[test]
fn fuzzy_path_score_handles_backslash_query() {
    // Windows-pasted queries with `\` separators normalise the same as `/`,
    // so `src\foo` should find `src/foo/bar.rs`.
    assert!(fuzzy_path_score("src/foo/bar.rs", "src\\foo").is_some());
}
