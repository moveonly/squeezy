use super::*;

#[test]
fn bar_widget_query_prefers_bar_path_over_zzz_path() {
    // F13 acceptance: `foo/bar/widget.rs` outranks `foo/zzz/widget.rs`
    // for query `bar widget`. Both paths share `widget`, only the first
    // also contains `bar`.
    let paths = ["foo/zzz/widget.rs", "foo/bar/widget.rs"];
    let ranked = rank_paths(&paths, "bar widget");
    assert_eq!(
        paths[ranked[0].0], "foo/bar/widget.rs",
        "expected bar path to win; ranked={ranked:?}"
    );
    assert!(
        ranked[0].1.overlap >= ranked[1].1.overlap,
        "winner should have >= overlap"
    );
}

#[test]
fn path_rank_counts_token_overlap() {
    let rank = path_rank("crates/squeezy-graph/src/lib.rs", "graph lib");
    assert_eq!(rank.overlap, 2);
}

#[test]
fn path_rank_handles_dot_and_underscore_separators() {
    // `parse_rust_file.rs` tokens are [parse, rust, file, rs].
    let rank = path_rank("src/parse_rust_file.rs", "rust file");
    assert_eq!(rank.overlap, 2);
}

#[test]
fn path_rank_query_is_case_insensitive() {
    let rank = path_rank("foo/bar/Widget.rs", "BAR widget");
    assert_eq!(rank.overlap, 2);
}

#[test]
fn empty_query_scores_zero_overlap() {
    let rank = path_rank("foo/bar/widget.rs", "");
    assert_eq!(rank.overlap, 0);
    assert_eq!(rank.trigram, 0.0);
}

#[test]
fn trigram_breaks_overlap_ties() {
    // Two paths share the same single token `widget`. The one whose
    // basename is closer to the query string (`widgetx` vs `wodget`)
    // should win on trigram similarity.
    let paths = ["foo/a/wodget.rs", "foo/b/widgetx.rs"];
    let ranked = rank_paths(&paths, "widget");
    assert_eq!(paths[ranked[0].0], "foo/b/widgetx.rs");
}

#[test]
fn rank_paths_is_stable_on_full_tie() {
    let paths = ["alpha/beta.rs", "alpha/beta.rs"];
    let ranked = rank_paths(&paths, "alpha");
    assert_eq!(ranked[0].0, 0);
    assert_eq!(ranked[1].0, 1);
}

#[test]
fn sort_key_orders_higher_overlap_first() {
    let high = PathRank {
        overlap: 3,
        exact_case_overlap: 0,
        trigram: 0.1,
    };
    let low = PathRank {
        overlap: 1,
        exact_case_overlap: 0,
        trigram: 0.9,
    };
    assert!(high.sort_key() < low.sort_key());
}

// ── Windows path tests ───────────────────────────────────────────────────────

#[test]
fn path_rank_handles_windows_backslash_query() {
    // A query using `\` separators tokenizes the same as `/` separators,
    // so `src\foo` should produce the same overlap as `src/foo`.
    let rank_slash = path_rank("src/foo/bar.rs", "src/foo");
    let rank_backslash = path_rank("src/foo/bar.rs", "src\\foo");
    assert_eq!(rank_slash.overlap, rank_backslash.overlap);
    assert_eq!(rank_slash.overlap, 2);
}

#[test]
fn path_rank_windows_csharp_query() {
    // Common Windows source paths.
    let rank = path_rank("src/Program.cs", "src\\Program");
    assert!(rank.overlap >= 1);
}

#[test]
fn rank_paths_is_deterministic_with_path_tiebreaker() {
    // When (overlap, trigram) is identical, paths are sorted lexicographically
    // to avoid nondeterministic ordering on Windows where HashMap enumeration
    // order may differ from Linux/macOS.
    let paths = ["zzz/widget.rs", "aaa/widget.rs"];
    let ranked = rank_paths(&paths, "widget");
    // Both paths have equal rank; `aaa` < `zzz` lexicographically.
    assert_eq!(
        paths[ranked[0].0], "aaa/widget.rs",
        "lexicographic tiebreaker must place aaa before zzz; ranked={ranked:?}"
    );
}

#[test]
fn exact_case_query_prefers_matching_case_path_on_linux() {
    // Linux case-sensitivity: when the query uses the uppercase form `Foo.rs`,
    // `src/Foo.rs` should rank above `src/foo.rs` even though both paths have
    // equal lowercase-token overlap.  The exact_case_overlap tiebreaker
    // implements this preference.
    let paths = ["src/foo.rs", "src/Foo.rs"];
    let ranked = rank_paths(&paths, "Foo.rs");
    assert_eq!(
        paths[ranked[0].0], "src/Foo.rs",
        "exact-case query should prefer Foo.rs over foo.rs; ranked={ranked:?}"
    );
}

#[test]
fn lowercase_query_prefers_lowercase_path_via_exact_case_overlap() {
    // A lowercase query `foo` gives both paths equal case-insensitive overlap,
    // but `src/foo.rs` gets exact_case_overlap=1 (token `foo` matches exactly)
    // while `src/Foo.rs` gets exact_case_overlap=0.  So `src/foo.rs` wins,
    // which is the correct Linux intent: lowercase query → lowercase path.
    let paths = ["src/foo.rs", "src/Foo.rs"];
    let ranked = rank_paths(&paths, "foo");
    assert_eq!(
        ranked[0].1.overlap, ranked[1].1.overlap,
        "lowercase query should give equal case-insensitive overlap to both paths"
    );
    assert_eq!(
        paths[ranked[0].0], "src/foo.rs",
        "lowercase query should prefer lowercase path via exact-case overlap"
    );
}

#[test]
fn exact_case_overlap_equals_overlap_for_fully_lowercase_paths() {
    // When both the query and the path are entirely lowercase, every token
    // that contributes to (case-insensitive) overlap also matches with exact
    // case — so exact_case_overlap == overlap.
    let rank = path_rank("src/parser/lib.rs", "parser");
    assert_eq!(rank.exact_case_overlap, rank.overlap);
}
