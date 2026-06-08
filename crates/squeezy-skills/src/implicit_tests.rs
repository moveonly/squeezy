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
    let mut doc_filenames = BTreeSet::new();
    doc_filenames.insert("skill.md".to_string());

    assert!(!doc_token_may_match_indexed_path("a.rs", &doc_filenames));
    assert!(!doc_token_may_match_indexed_path(
        "README.md",
        &doc_filenames
    ));
}

#[test]
fn doc_prefilter_keeps_plausible_skill_doc_tokens() {
    let mut doc_filenames = BTreeSet::new();
    doc_filenames.insert("skill.md".to_string());

    // SKILL.md always matches via the early-return fast path.
    assert!(doc_token_may_match_indexed_path("SKILL.md", &doc_filenames));
    assert!(doc_token_may_match_indexed_path(
        ".squeezy/skills/nav/SKILL.md",
        &doc_filenames
    ));
}

#[test]
fn doc_prefilter_keeps_skill_doc_tokens_when_canonical_target_differs() {
    // Even when the indexed path uses a different name, SKILL.md tokens
    // should still pass via the fast-path early return.
    let doc_filenames = BTreeSet::new();

    assert!(doc_token_may_match_indexed_path(
        ".squeezy/skills/nav/SKILL.md",
        &doc_filenames
    ));
}

#[test]
fn doc_prefilter_matches_non_skill_doc_by_filename() {
    let mut doc_filenames = BTreeSet::new();
    doc_filenames.insert("guide.md".to_string());

    assert!(doc_token_may_match_indexed_path("guide.md", &doc_filenames));
    assert!(doc_token_may_match_indexed_path(
        "skills/guide.md",
        &doc_filenames
    ));
    // Case-insensitive matching.
    assert!(doc_token_may_match_indexed_path("GUIDE.MD", &doc_filenames));
    assert!(!doc_token_may_match_indexed_path(
        "other.md",
        &doc_filenames
    ));
}

#[test]
fn powershell_readers_trigger_doc_read_detection() {
    // get-content and gc are cross-platform (not standard Unix commands).
    assert!(command_reads_file(&[
        "Get-Content".to_string(),
        "SKILL.md".to_string()
    ]));
    assert!(command_reads_file(&[
        "gc".to_string(),
        "SKILL.md".to_string()
    ]));
    // Non-reader should not match on any platform.
    assert!(!command_reads_file(&[
        "Invoke-WebRequest".to_string(),
        "SKILL.md".to_string()
    ]));
}

#[cfg(windows)]
#[test]
fn type_command_triggers_read_on_windows() {
    // On Windows, `type` is the cmd.exe file-display command and is a reader.
    assert!(command_reads_file(&[
        "type".to_string(),
        "SKILL.md".to_string()
    ]));
}

#[cfg(not(windows))]
#[test]
fn type_command_does_not_trigger_read_on_unix() {
    // On Unix, `type` is a shell introspection built-in, not a file reader.
    assert!(!command_reads_file(&[
        "type".to_string(),
        "SKILL.md".to_string()
    ]));
}

#[test]
fn unix_tokenizer_treats_backslash_as_escape() {
    // tokenize_command_unix is always compiled; verify backslash-as-escape behavior.
    let tokens = tokenize_command_unix("cat foo\\ bar.txt");
    assert_eq!(tokens, vec!["cat", "foo bar.txt"]);
}

#[test]
fn windows_tokenizer_preserves_windows_path_separators() {
    // tokenize_command_windows is always compiled; verify Windows path preservation.
    let tokens = tokenize_command_windows(r"pwsh -File .\.squeezy\skills\nav\scripts\init.ps1");
    assert_eq!(tokens.len(), 3);
    assert_eq!(tokens[0], "pwsh");
    assert_eq!(tokens[1], "-File");
    assert_eq!(tokens[2], r".\.squeezy\skills\nav\scripts\init.ps1");
}

#[test]
fn windows_tokenizer_preserves_absolute_windows_path() {
    let tokens = tokenize_command_windows(r#"pwsh -File "C:\Users\alice\SKILL.md""#);
    assert_eq!(tokens.len(), 3);
    assert_eq!(tokens[2], r"C:\Users\alice\SKILL.md");
}

#[test]
fn dispatch_routes_to_platform_tokenizer() {
    // On Unix the dispatcher should use the Unix tokenizer (backslash escapes).
    // On Windows the dispatcher should use the Windows tokenizer (backslash is literal).
    if cfg!(windows) {
        let tokens = tokenize_command(r"pwsh -File .\.squeezy\skills\SKILL.md");
        assert_eq!(
            tokens.len(),
            3,
            "Windows: backslash should not split tokens"
        );
    } else {
        let tokens = tokenize_command("cat foo\\ bar.txt");
        assert_eq!(
            tokens,
            vec!["cat", "foo bar.txt"],
            "Unix: backslash should escape space"
        );
    }
}
