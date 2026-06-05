use super::*;

#[test]
fn script_run_detection_matches_runner_plus_extension() {
    let tokens = vec![
        "python3".to_string(),
        "-u".to_string(),
        "scripts/fetch_comments.py".to_string(),
    ];
    assert_eq!(script_run_token(&tokens), Some("scripts/fetch_comments.py"));
}

#[test]
fn script_run_detection_excludes_python_c() {
    let tokens = vec![
        "python3".to_string(),
        "-c".to_string(),
        "print(1)".to_string(),
    ];
    assert_eq!(script_run_token(&tokens), None);
}

#[test]
fn tokenizer_preserves_quoted_paths() {
    let tokens = tokenize_command("python3 \"scripts/my tool.py\"");
    assert_eq!(tokens, vec!["python3", "scripts/my tool.py"]);
}

#[test]
fn doc_prefilter_rejects_unrelated_reader_tokens() {
    let mut by_doc_path = BTreeMap::new();
    by_doc_path.insert(
        PathBuf::from("/repo/.squeezy/skills/nav/SKILL.md"),
        "nav".to_string(),
    );

    assert!(!doc_token_may_match_indexed_path("a.rs", &by_doc_path));
    assert!(!doc_token_may_match_indexed_path("README.md", &by_doc_path));
}

#[test]
fn doc_prefilter_keeps_plausible_skill_doc_tokens() {
    let mut by_doc_path = BTreeMap::new();
    by_doc_path.insert(
        PathBuf::from("/repo/.squeezy/skills/nav/SKILL.md"),
        "nav".to_string(),
    );

    assert!(doc_token_may_match_indexed_path("SKILL.md", &by_doc_path));
    assert!(doc_token_may_match_indexed_path(
        ".squeezy/skills/nav/SKILL.md",
        &by_doc_path
    ));
}

#[test]
fn doc_prefilter_keeps_skill_doc_tokens_when_canonical_target_differs() {
    let mut by_doc_path = BTreeMap::new();
    by_doc_path.insert(PathBuf::from("/repo/canonical/nav.md"), "nav".to_string());

    assert!(doc_token_may_match_indexed_path(
        ".squeezy/skills/nav/SKILL.md",
        &by_doc_path
    ));
}
