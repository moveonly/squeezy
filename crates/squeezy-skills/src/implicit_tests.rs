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

// ── env-prefix detection ──────────────────────────────────────────────────────

#[test]
fn env_prefix_activates_runner_plus_script() {
    let tokens = vec![
        "env".to_string(),
        "python3".to_string(),
        "scripts/fetch.py".to_string(),
    ];
    assert_eq!(script_run_token(&tokens), Some("scripts/fetch.py"));
}

#[test]
fn env_with_var_assignment_activates_runner() {
    let tokens = vec![
        "env".to_string(),
        "PYTHONPATH=/lib".to_string(),
        "python3".to_string(),
        "scripts/run.py".to_string(),
    ];
    assert_eq!(script_run_token(&tokens), Some("scripts/run.py"));
}

#[test]
fn env_with_flag_and_runner() {
    // `env -i python3 script.py` — `-i` is a bare flag
    let tokens = vec![
        "env".to_string(),
        "-i".to_string(),
        "python3".to_string(),
        "script.py".to_string(),
    ];
    assert_eq!(script_run_token(&tokens), Some("script.py"));
}

#[test]
fn env_with_unknown_runner_returns_none() {
    let tokens = vec![
        "env".to_string(),
        "notarunner".to_string(),
        "script.py".to_string(),
    ];
    assert_eq!(script_run_token(&tokens), None);
}

#[test]
fn env_empty_after_prefix_returns_none() {
    let tokens = vec!["env".to_string()];
    assert_eq!(script_run_token(&tokens), None);
}

// ── direct executable path detection ─────────────────────────────────────────

#[test]
fn direct_dotslash_script_is_detected() {
    let tokens = vec!["./scripts/task.sh".to_string()];
    assert_eq!(script_run_token(&tokens), Some("./scripts/task.sh"));
}

#[test]
fn direct_absolute_script_is_detected() {
    let tokens = vec!["/usr/local/lib/skill/run.py".to_string()];
    assert_eq!(
        script_run_token(&tokens),
        Some("/usr/local/lib/skill/run.py")
    );
}

#[test]
fn direct_dotdot_script_is_detected() {
    let tokens = vec!["../shared/scripts/run.sh".to_string()];
    assert_eq!(script_run_token(&tokens), Some("../shared/scripts/run.sh"));
}

#[test]
fn direct_path_without_script_extension_returns_none() {
    // A `./binary` with no recognized extension is not a script run.
    let tokens = vec!["./mybinary".to_string(), "arg".to_string()];
    assert_eq!(script_run_token(&tokens), None);
}

// ── rg / fd / find as skill-doc readers ──────────────────────────────────────

#[test]
fn rg_is_recognized_as_file_reader() {
    let tokens: Vec<String> = vec!["rg".to_string(), "SKILL.md".to_string()];
    assert!(command_reads_file(&tokens));
}

#[test]
fn fd_is_recognized_as_file_reader() {
    let tokens: Vec<String> = vec!["fd".to_string(), "SKILL.md".to_string()];
    assert!(command_reads_file(&tokens));
}

#[test]
fn find_is_recognized_as_file_reader() {
    let tokens: Vec<String> = vec![
        "find".to_string(),
        ".".to_string(),
        "-name".to_string(),
        "SKILL.md".to_string(),
    ];
    assert!(command_reads_file(&tokens));
}

// ── is_path_like helper ───────────────────────────────────────────────────────

#[test]
fn is_path_like_classifies_correctly() {
    assert!(is_path_like("./foo.sh"));
    assert!(is_path_like("../foo.sh"));
    assert!(is_path_like("/abs/path/foo.py"));
    assert!(!is_path_like("python3"));
    assert!(!is_path_like("env"));
    assert!(!is_path_like("script.py"));
}

// ── env -S is treated as a bare flag, not consuming a payload token ───────────

#[test]
fn env_dash_s_is_treated_as_bare_flag() {
    // `env -S "python3 script.py"` — the embedded split-string is opaque;
    // we cannot extract the runner, so the result must be None rather than
    // incorrectly treating the split-string token as the runner.
    let tokens = vec![
        "env".to_string(),
        "-S".to_string(),
        "python3 script.py".to_string(),
    ];
    // With -S treated as a bare flag, i advances by 1 (to consume the -S
    // token), then "python3 script.py" (the split-string) is the next
    // candidate runner — it is not in RUNNERS, so None.
    let result = script_run_token(&tokens);
    assert_eq!(
        result, None,
        "env -S with split-string should not produce a spurious activation"
    );
}

// ── detect_for_command end-to-end with rg/fd reading SKILL.md ────────────────

fn make_test_skill_entry(name: &str, doc_path: &std::path::Path) -> SkillEntry {
    SkillEntry {
        summary: SkillSummary {
            name: name.to_string(),
            description: format!("{name} skill"),
            when_to_use: None,
            source: crate::SkillSource::User,
            location: doc_path.to_path_buf(),
            disabled: false,
            manifest: None,
            context_mode: crate::SkillContextMode::Inline,
        },
        base_dir: doc_path.parent().unwrap_or(doc_path).to_path_buf(),
        triggers: vec![],
    }
}

#[test]
fn detect_for_command_activates_skill_on_rg_skill_doc() {
    let workdir = PathBuf::from("/repo");
    let skill_doc = PathBuf::from("/repo/.squeezy/skills/nav/SKILL.md");

    let mut by_doc_path = BTreeMap::new();
    by_doc_path.insert(skill_doc.clone(), "nav".to_string());
    let mut doc_filenames = BTreeSet::new();
    doc_filenames.insert("skill.md".to_string());

    let entry = make_test_skill_entry("nav", &skill_doc);
    let mut skills = BTreeMap::new();
    skills.insert("nav".to_string(), entry);

    // `rg '' .squeezy/skills/nav/SKILL.md` should activate via doc-read path.
    let result = detect_for_command(
        "rg '' .squeezy/skills/nav/SKILL.md",
        &workdir,
        &BTreeMap::new(),
        &by_doc_path,
        &doc_filenames,
        &skills,
    );
    assert!(
        result.is_some(),
        "rg reading SKILL.md should activate the skill"
    );
}

#[test]
fn detect_for_command_activates_skill_on_fd_skill_doc() {
    let workdir = PathBuf::from("/repo");
    let skill_doc = PathBuf::from("/repo/.squeezy/skills/nav/SKILL.md");

    let mut by_doc_path = BTreeMap::new();
    by_doc_path.insert(skill_doc.clone(), "nav".to_string());
    let mut doc_filenames = BTreeSet::new();
    doc_filenames.insert("skill.md".to_string());

    let entry = make_test_skill_entry("nav", &skill_doc);
    let mut skills = BTreeMap::new();
    skills.insert("nav".to_string(), entry);

    // Use the form where the full skill doc path appears as an argument token.
    // `fd -tf .squeezy/skills/nav/SKILL.md` — checking a specific file.
    let result = detect_for_command(
        "fd -tf .squeezy/skills/nav/SKILL.md",
        &workdir,
        &BTreeMap::new(),
        &by_doc_path,
        &doc_filenames,
        &skills,
    );
    assert!(
        result.is_some(),
        "fd with explicit SKILL.md path should activate the skill"
    );
}
