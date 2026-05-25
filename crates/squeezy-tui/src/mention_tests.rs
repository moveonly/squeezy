use super::*;
use std::path::PathBuf;

#[test]
fn detects_mention_at_start_of_input() {
    let q = detect_mention("@gra", 4).expect("expected mention");
    assert_eq!(q.start, 0);
    assert_eq!(q.end, 4);
    assert_eq!(q.query, "gra");
}

#[test]
fn detects_mention_after_whitespace() {
    let q = detect_mention("hello @foo", 10).expect("mention after space");
    assert_eq!(q.query, "foo");
}

#[test]
fn does_not_detect_mention_mid_word() {
    // `email@host` — no popup because `@` is preceded by `l`.
    let result = detect_mention("email@host", 10);
    assert!(result.is_none(), "got: {result:?}");
}

#[test]
fn does_not_detect_when_cursor_is_before_at() {
    let result = detect_mention("hello @foo", 5);
    assert!(result.is_none(), "got: {result:?}");
}

#[test]
fn returns_empty_query_just_after_at() {
    let q = detect_mention("@", 1).expect("just `@`");
    assert_eq!(q.query, "");
}

#[test]
fn ranks_prefix_match_above_subsequence() {
    let files = vec![
        PathBuf::from("docs/zebra.md"),
        PathBuf::from("crates/graph/lib.rs"),
        PathBuf::from("readme.md"),
    ];
    let out = rank_files("gra", &files);
    assert_eq!(out.first().unwrap(), &PathBuf::from("crates/graph/lib.rs"));
}

#[test]
fn ranks_filename_prefix_above_path_substring() {
    let files = vec![
        PathBuf::from("crates/squeezy-graph/src/lib.rs"),
        PathBuf::from("graph_helpers.rs"),
    ];
    let out = rank_files("graph", &files);
    assert_eq!(out[0], PathBuf::from("graph_helpers.rs"));
}

#[test]
fn rank_empty_query_returns_first_n_paths() {
    let files: Vec<PathBuf> = (0..20)
        .map(|i| PathBuf::from(format!("file{i}.rs")))
        .collect();
    let out = rank_files("", &files);
    assert_eq!(out.len(), MAX_MATCHES);
}

#[test]
fn apply_inserts_path_and_returns_new_cursor() {
    let q = MentionQuery {
        start: 6,
        end: 10,
        query: "gra".to_string(),
    };
    let popup = MentionPopup::from_query(q, vec![PathBuf::from("crates/squeezy-graph/src/lib.rs")]);
    let (new_input, cursor) = popup.apply("hello @gra").expect("apply");
    assert_eq!(new_input, "hello crates/squeezy-graph/src/lib.rs ");
    assert_eq!(cursor, new_input.len());
}

#[test]
fn popup_navigation_clamps_at_bounds() {
    let q = MentionQuery {
        start: 0,
        end: 4,
        query: "a".to_string(),
    };
    let mut popup = MentionPopup::from_query(q, vec![PathBuf::from("a"), PathBuf::from("b")]);
    popup.move_up();
    assert_eq!(popup.selected, 0);
    popup.move_down();
    popup.move_down();
    assert_eq!(popup.selected, 1, "should clamp");
}
