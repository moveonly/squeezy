use super::*;
use clap::Parser;
use squeezy_llm::LlmEvent;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn model_choice_label_round_trips_to_model_id() {
    let model = models_for_provider("openai").next().expect("openai model");
    let label = model_choice_label(model);

    assert_eq!(parse_model_choice_id(&label), model.id);
}

#[test]
fn model_selection_state_detects_saved_startup_choice() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "openai"
model = "gpt-5.5"
selection_version = 1
"#,
        "test",
    )
    .expect("settings parse");

    assert!(model_selection_state(&settings).complete());
}

#[test]
fn model_selection_state_treats_provider_and_model_as_configured_without_version() {
    let settings = SettingsFile::from_toml_str(
        r#"
[model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#,
        "test",
    )
    .expect("settings parse");
    let state = model_selection_state(&settings);

    assert!(
        state.configured(),
        "explicit provider/model should skip first-run setup"
    );
    assert!(
        !state.complete(),
        "selection_version remains a migration marker, not a startup blocker"
    );
}

#[test]
fn model_selection_state_reads_project_scope_selection() {
    let root = temp_dir("project-model-selection");
    let user_path = root.join("user.toml");
    let project_path = root.join("repo").join(PROJECT_SETTINGS_FILE);
    let repo_path = root.join("local.toml");
    fs::create_dir_all(project_path.parent().expect("project parent")).expect("mkdir");
    fs::write(
        &project_path,
        r#"
[model]
provider = "anthropic"
model = "claude-haiku-4-5-20251001"
"#,
    )
    .expect("write project settings");

    let state =
        model_selection_state_from_paths(&user_path, Some(&project_path), &repo_path).unwrap();

    assert!(state.configured());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn model_selection_state_reads_local_scope_selection() {
    let root = temp_dir("local-model-selection");
    let user_path = root.join("user.toml");
    let project_path = root.join("repo").join(PROJECT_SETTINGS_FILE);
    let repo_path = root.join("local.toml");
    fs::write(
        &repo_path,
        r#"
[model]
provider = "openai"
model = "gpt-5.4-mini"
"#,
    )
    .expect("write local settings");

    let state =
        model_selection_state_from_paths(&user_path, Some(&project_path), &repo_path).unwrap();

    assert!(state.configured());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn save_startup_model_selection_preserves_existing_settings() {
    let root = temp_dir("model-selection");
    let path = root.join("settings.toml");
    fs::write(
        &path,
        r#"
[permissions]
read = "deny"
"#,
    )
    .expect("write settings");
    let selection = StartupModelSelection {
        theme: "bright".to_string(),
        provider: "openai",
        model: "gpt-5.5".to_string(),
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        base_url: None,
        reasoning_effort: Some(ReasoningEffort::XHigh),
    };

    save_startup_model_selection(&path, &selection).expect("save selection");

    let text = fs::read_to_string(&path).expect("read settings");
    assert!(text.contains("read = \"deny\""));
    assert!(text.contains("theme = \"bright\""));
    assert!(text.contains("provider = \"openai\""));
    assert!(text.contains("model = \"gpt-5.5\""));
    assert!(text.contains("reasoning_effort = \"xhigh\""));
    assert!(text.contains("selection_version = 1"));
    assert!(text.contains("api_key_env = \"OPENAI_API_KEY\""));
    assert!(!text.contains("sk-"));

    let settings = SettingsFile::from_toml_str(&text, "test").expect("round-trip");
    assert!(model_selection_state(&settings).complete());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cli_prompt_format_defaults_to_text() {
    let cli = Cli::try_parse_from(["squeezy", "--prompt", "hi"]).expect("parse");
    assert_eq!(cli.format, PromptFormat::Default);
    assert_eq!(cli.prompt, vec!["hi".to_string()]);
}

#[test]
fn cli_prompt_accepts_repeated_values_in_order() {
    let cli =
        Cli::try_parse_from(["squeezy", "--prompt", "first", "--prompt", "second"]).expect("parse");
    assert_eq!(cli.prompt, vec!["first".to_string(), "second".to_string()]);
}

#[test]
fn cli_prompt_accepts_at_mentions_and_dash() {
    let cli = Cli::try_parse_from([
        "squeezy",
        "--prompt",
        "@notes.md",
        "--prompt",
        "-",
        "--prompt",
        "follow up",
    ])
    .expect("parse");
    assert_eq!(
        cli.prompt,
        vec![
            "@notes.md".to_string(),
            "-".to_string(),
            "follow up".to_string(),
        ]
    );
}

#[test]
fn cli_prompt_defaults_to_empty_when_omitted() {
    let cli = Cli::try_parse_from(["squeezy"]).expect("parse");
    assert!(cli.prompt.is_empty());
}

#[test]
fn cli_prompt_format_json_is_accepted_lowercase() {
    let cli =
        Cli::try_parse_from(["squeezy", "--prompt", "hi", "--format", "json"]).expect("parse json");
    assert_eq!(cli.format, PromptFormat::Json);
}

#[test]
fn cli_prompt_format_rejects_unknown_value() {
    let err = Cli::try_parse_from(["squeezy", "--prompt", "hi", "--format", "yaml"])
        .expect_err("yaml is not a valid prompt format");
    assert!(
        err.to_string().to_lowercase().contains("yaml")
            || err.to_string().to_lowercase().contains("invalid"),
        "expected clap error to mention the bad value, got: {err}"
    );
}

#[test]
fn cli_help_slash_command_topic_uses_tui_help_parser() {
    let help = squeezy_skills::SqueezyHelp::new("");
    let answer = cli_help_answer(&help, Some("/theme"));

    assert_eq!(answer.topic, "/theme");
    let rendered = answer.render_markdown();
    assert!(rendered.contains("## /theme"), "{rendered}");
    assert!(rendered.contains("Syntax:"), "{rendered}");
}

#[test]
fn repo_profile_error_does_not_block_startup() {
    let mut config = AppConfig::default();
    let prepared = prepare_repo_profile_from_load(
        &mut config,
        Err(SqueezyError::Permission("blocked by sandbox".to_string())),
    );

    let summary = prepared.visible_summary.expect("warning summary");
    assert!(summary.contains("Repo profile unavailable"), "{summary}");
    assert!(summary.contains("blocked by sandbox"), "{summary}");
    assert!(prepared.language_summary.is_empty());
}

#[test]
fn ask_format_json_emits_one_object_per_line() {
    // Exercises the JSONL schema used by `squeezy --prompt ... --format json`.
    // Each emitted line must parse as a single `LlmEvent`; the tag/content
    // form (`type` + `data`) is the public contract callers pipe to `jq`.
    let events = [
        LlmEvent::Started,
        LlmEvent::TextDelta("hello".to_string()),
        LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: squeezy_core::CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        },
    ];
    let mut buf = String::new();
    for event in &events {
        let line = serde_json::to_string(event).expect("serialize event");
        assert!(
            !line.contains('\n'),
            "serialized event {line} contains an embedded newline; JSONL framing requires one object per line"
        );
        buf.push_str(&line);
        buf.push('\n');
    }
    let lines: Vec<&str> = buf.lines().collect();
    assert_eq!(lines.len(), 3);
    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("started parses");
    assert_eq!(first["type"], "started");
    let second: serde_json::Value = serde_json::from_str(lines[1]).expect("delta parses");
    assert_eq!(second["type"], "text_delta");
    assert_eq!(second["data"], "hello");
    let third: serde_json::Value = serde_json::from_str(lines[2]).expect("completed parses");
    assert_eq!(third["type"], "completed");
    assert_eq!(third["data"]["response_id"], "resp_1");
}

fn temp_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = env::temp_dir().join(format!("squeezy-cli-{name}-{suffix}"));
    fs::create_dir_all(&path).expect("temp dir");
    path
}

fn meta(id: &str, cwd: &str, started_at_ms: u64, resume_available: bool) -> SessionMetadata {
    SessionMetadata {
        session_id: id.to_string(),
        cwd: cwd.to_string(),
        started_at_ms,
        resume_available,
        ..SessionMetadata::default()
    }
}

#[test]
fn continue_flag_picks_most_recent_resumable_for_cwd() {
    // `SessionStore::list` sorts newest-first; mirror that here.
    let sessions = vec![
        meta("s-newest", "/repo", 300, true),
        meta("s-other", "/elsewhere", 250, true),
        meta("s-mid", "/repo", 200, true),
        meta("s-old", "/repo", 100, true),
    ];

    let resolved = resolve_resume_session(ResumeFlag::Continue, &sessions, "/repo");

    assert_eq!(resolved.session_id.as_deref(), Some("s-newest"));
    assert!(resolved.note.is_none());
}

#[test]
fn continue_flag_skips_unresumable_sessions() {
    let sessions = vec![
        meta("s-stale", "/repo", 400, false),
        meta("s-good", "/repo", 200, true),
    ];

    let resolved = resolve_resume_session(ResumeFlag::Continue, &sessions, "/repo");

    assert_eq!(resolved.session_id.as_deref(), Some("s-good"));
}

#[test]
fn continue_flag_falls_back_with_stderr_note_when_no_match() {
    let sessions = vec![
        meta("s-unresumable", "/repo", 200, false),
        meta("s-other-cwd", "/elsewhere", 100, true),
    ];

    let resolved = resolve_resume_session(ResumeFlag::Continue, &sessions, "/repo");

    assert_eq!(resolved.session_id, None);
    let note = resolved.note.expect("fallback note");
    assert!(note.contains("--continue"));
    assert!(note.contains("starting fresh"));
}

#[test]
fn continue_flag_emits_normalization_note_when_path_differs_but_same_location() {
    // A session stored with a trailing separator is the same location as
    // the current cwd without one. paths_same() matches them; the note
    // surface should indicate that normalization was applied.
    let sessions = vec![meta("s", "/repo/", 100, true)];

    let resolved = resolve_resume_session(ResumeFlag::Continue, &sessions, "/repo");

    assert_eq!(
        resolved.session_id.as_deref(),
        Some("s"),
        "should match via normalization"
    );
    let note = resolved.note.expect("normalization note expected");
    assert!(
        note.contains("path normalization"),
        "note should mention normalization; got: {note}"
    );
}

#[test]
fn explicit_session_flag_passes_id_through_unfiltered() {
    let sessions = vec![meta("s-only", "/repo", 100, true)];

    let resolved = resolve_resume_session(ResumeFlag::Explicit("custom-id"), &sessions, "/repo");

    assert_eq!(resolved.session_id.as_deref(), Some("custom-id"));
    assert!(resolved.note.is_none());
}

#[test]
fn no_resume_flag_starts_fresh_without_lookup() {
    let resolved = resolve_resume_session(ResumeFlag::None, &[], "/repo");

    assert_eq!(resolved.session_id, None);
    assert!(resolved.note.is_none());
}

#[test]
fn cli_continue_and_session_are_mutually_exclusive() {
    // clap should reject the combination at parse time; this guards the
    // `conflicts_with` attribute against accidental removal.
    let err = Cli::try_parse_from(["squeezy", "--continue", "--session", "abc"]).unwrap_err();
    assert!(
        err.kind() == clap::error::ErrorKind::ArgumentConflict,
        "expected ArgumentConflict, got {:?}",
        err.kind()
    );
}

#[test]
fn cli_force_cross_project_defaults_to_false() {
    let cli = Cli::try_parse_from(["squeezy"]).expect("parse");
    assert!(!cli.force_cross_project);
}

#[test]
fn cli_force_cross_project_parses_top_level_flag() {
    let cli = Cli::try_parse_from(["squeezy", "--force-cross-project", "--session", "abc"])
        .expect("parse force flag with session");
    assert!(cli.force_cross_project);
    assert_eq!(cli.session.as_deref(), Some("abc"));
}

#[test]
fn cli_force_cross_project_attaches_to_sessions_resume() {
    // Mirror the subcommand's own opt-in flag so scripted callers can
    // type `squeezy sessions resume <id> --force-cross-project` without
    // hoisting the flag above the subcommand boundary.
    let cli = Cli::try_parse_from([
        "squeezy",
        "sessions",
        "resume",
        "abc",
        "--force-cross-project",
    ])
    .expect("parse sessions resume --force-cross-project");
    match cli.command {
        Some(Command::Sessions {
            command:
                SessionsCommand::Resume {
                    id,
                    force_cross_project,
                },
        }) => {
            assert_eq!(id, "abc");
            assert!(force_cross_project);
        }
        other => panic!("expected sessions resume command, got {other:?}"),
    }
}

#[test]
fn cross_project_resume_prompt_skips_when_paths_match() {
    assert!(cross_project_resume_prompt("/repo", "/repo").is_none());
}

#[test]
fn cross_project_resume_prompt_ignores_trailing_separator() {
    // The original cwd was `current_dir().display()` which never carries
    // a trailing separator on real filesystems, but defensively normalize
    // so a hand-edited or migrated metadata.json doesn't trigger a
    // spurious prompt.
    assert!(cross_project_resume_prompt("/repo/", "/repo").is_none());
    assert!(cross_project_resume_prompt("/repo", "/repo/").is_none());
}

#[test]
fn cross_project_resume_prompt_renders_y_n_message() {
    let prompt = cross_project_resume_prompt("/old/repo", "/new/repo")
        .expect("differing cwds should require a prompt");
    assert!(prompt.contains("/old/repo"));
    assert!(prompt.contains("/new/repo"));
    assert!(prompt.contains("[y/N]"));
}

#[test]
fn confirm_cross_project_resume_proceeds_when_cwds_match() {
    let mut reader = io::Cursor::new(Vec::<u8>::new());
    let mut writer: Vec<u8> = Vec::new();

    let proceed = confirm_cross_project_resume("/repo", "/repo", false, &mut reader, &mut writer)
        .expect("matching cwds skip the prompt entirely");

    assert!(proceed, "matching cwds must proceed");
    assert!(
        writer.is_empty(),
        "no prompt should have been written for matching cwds; got {:?}",
        String::from_utf8_lossy(&writer)
    );
}

#[test]
fn confirm_cross_project_resume_force_bypasses_prompt_for_mismatch() {
    let mut reader = io::Cursor::new(Vec::<u8>::new());
    let mut writer: Vec<u8> = Vec::new();

    let proceed = confirm_cross_project_resume("/old", "/new", true, &mut reader, &mut writer)
        .expect("force bypasses the prompt");

    assert!(proceed, "force-cross-project must proceed");
    assert!(
        writer.is_empty(),
        "force should suppress the prompt; got {:?}",
        String::from_utf8_lossy(&writer)
    );
}

#[test]
fn confirm_cross_project_resume_accepts_lowercase_yes() {
    let mut reader = io::Cursor::new(b"y\n".to_vec());
    let mut writer: Vec<u8> = Vec::new();

    let proceed = confirm_cross_project_resume("/old", "/new", false, &mut reader, &mut writer)
        .expect("scripted stdin");

    assert!(proceed, "`y` should proceed");
    let rendered = String::from_utf8(writer).expect("utf-8 prompt");
    assert!(rendered.contains("/old"));
    assert!(rendered.contains("/new"));
    assert!(rendered.contains("[y/N]"));
}

#[test]
fn confirm_cross_project_resume_accepts_full_word_yes_and_trims_whitespace() {
    let mut reader = io::Cursor::new(b"  YES \n".to_vec());
    let mut writer: Vec<u8> = Vec::new();

    let proceed = confirm_cross_project_resume("/old", "/new", false, &mut reader, &mut writer)
        .expect("scripted stdin");

    assert!(proceed, "`YES` (with whitespace) should proceed");
}

#[test]
fn confirm_cross_project_resume_defaults_to_no_on_blank_input() {
    let mut reader = io::Cursor::new(b"\n".to_vec());
    let mut writer: Vec<u8> = Vec::new();

    let proceed = confirm_cross_project_resume("/old", "/new", false, &mut reader, &mut writer)
        .expect("scripted stdin");

    assert!(!proceed, "blank input must default to N");
}

#[test]
fn confirm_cross_project_resume_defaults_to_no_on_eof() {
    let mut reader = io::Cursor::new(Vec::<u8>::new());
    let mut writer: Vec<u8> = Vec::new();

    let proceed = confirm_cross_project_resume("/old", "/new", false, &mut reader, &mut writer)
        .expect("empty stdin returns EOF, not an io error");

    assert!(!proceed, "EOF (e.g. closed pipe) must default to N");
}

#[test]
fn confirm_cross_project_resume_rejects_unrelated_input() {
    let mut reader = io::Cursor::new(b"nope\n".to_vec());
    let mut writer: Vec<u8> = Vec::new();

    let proceed = confirm_cross_project_resume("/old", "/new", false, &mut reader, &mut writer)
        .expect("scripted stdin");

    assert!(!proceed, "`nope` must not be treated as `yes`");
}

#[test]
fn confirm_cross_project_resume_stdio_reads_metadata_and_skips_when_match() {
    // Plant a session metadata file on disk with `cwd` matching the
    // process's real `current_dir`. The stdio wrapper should consult
    // the store, decide no prompt is needed, and return `true` without
    // touching stdin — which is what makes this safe to run in `cargo
    // test` (no scripted I/O required).
    let root = temp_dir("cross-project-stdio-match");
    let cwd_str = env::current_dir()
        .expect("current dir")
        .display()
        .to_string();
    let session_id = plant_session_metadata(&root, "match-id", &cwd_str);

    let store = open_store_at(&root);
    let proceed = confirm_cross_project_resume_stdio(&store, &session_id, false)
        .expect("matching cwd should bypass the prompt");
    assert!(proceed);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn confirm_cross_project_resume_stdio_force_skips_metadata_lookup_decision() {
    // Even with mismatched cwds, the `--force-cross-project` short-circuit
    // must return `true` without ever reading stdin. We assert via the
    // stdio wrapper to cover the real public entry point used by the CLI.
    let root = temp_dir("cross-project-stdio-force");
    let session_id = plant_session_metadata(&root, "force-id", "/some/old/cwd");

    let store = open_store_at(&root);
    let proceed = confirm_cross_project_resume_stdio(&store, &session_id, true)
        .expect("force bypasses the prompt regardless of metadata cwd");
    assert!(proceed);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn confirm_cross_project_resume_stdio_errors_on_unknown_session() {
    let root = temp_dir("cross-project-stdio-missing");
    let store = open_store_at(&root);
    let err = confirm_cross_project_resume_stdio(&store, "ghost-id", false)
        .expect_err("unknown session id must surface an error");
    let msg = err.to_string();
    assert!(
        msg.contains("ghost-id"),
        "error should mention the missing session id; got: {msg}",
    );

    let _ = fs::remove_dir_all(&root);
}

/// Write a minimal `metadata.json` for a session id under `root` so the
/// CLI cross-project tests can exercise the real `SessionStore::read_metadata`
/// path. Only the fields the prompt depends on (`session_id`, `cwd`) are
/// populated; everything else falls back to `SessionMetadata::default()`.
fn plant_session_metadata(root: &Path, session_id: &str, cwd: &str) -> String {
    let dir = root.join("sessions").join(session_id);
    fs::create_dir_all(&dir).expect("session dir");
    let metadata = SessionMetadata {
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        ..SessionMetadata::default()
    };
    let body = serde_json::to_string_pretty(&metadata).expect("serialize metadata");
    fs::write(dir.join("metadata.json"), body).expect("write metadata.json");
    session_id.to_string()
}

/// Open a `SessionStore` rooted at `<root>/sessions` by routing the path
/// through `AppConfig::session_logs.log_dir`, which is the supported
/// public lever for redirecting the on-disk session root.
fn open_store_at(root: &Path) -> SessionStore {
    let mut config = AppConfig {
        workspace_root: root.to_path_buf(),
        ..AppConfig::default()
    };
    config.session_logs.log_dir = Some(root.join("sessions"));
    SessionStore::open(&config)
}

#[test]
fn cli_session_dir_defaults_to_none_when_flag_omitted() {
    let cli = Cli::try_parse_from(["squeezy"]).expect("parse");
    assert_eq!(cli.session_dir, None);
}

#[test]
fn cli_session_dir_parses_value_as_pathbuf() {
    let cli = Cli::try_parse_from(["squeezy", "--session-dir", "/var/log/squeezy"]).expect("parse");
    assert_eq!(
        cli.session_dir.as_deref(),
        Some(std::path::Path::new("/var/log/squeezy"))
    );
}

// `config_from_cli` reads real process env vars and HOME-anchored settings
// files; serialize the precedence tests below so they don't race each other
// or other env-mutating tests in this module.
static SESSION_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static NONEMPTY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn env_var_is_nonempty_treats_blank_values_as_unset() {
    let _guard = NONEMPTY_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = "SQUEEZY_TEST_NONEMPTY_ENV";
    let previous = env::var_os(key);
    // SAFETY: NONEMPTY_ENV_LOCK serializes mutations for this key in this
    // module.
    unsafe {
        env::remove_var(key);
    }
    assert!(!env_var_is_nonempty(key));

    unsafe {
        env::set_var(key, "   ");
    }
    assert!(!env_var_is_nonempty(key));

    unsafe {
        env::set_var(key, "configured");
    }
    assert!(env_var_is_nonempty(key));

    unsafe {
        match previous {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }
}

/// Point `HOME` and `SQUEEZY_SESSION_DIR` at known values inside a single
/// guarded section, then restore the previous environment when the returned
/// guard drops.  Keeps the precedence tests isolated from the dev's real
/// `~/.squeezy/settings.toml` and any ambient session-dir env.
struct SessionDirEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev_home: Option<std::ffi::OsString>,
    prev_session_dir: Option<std::ffi::OsString>,
    home_dir: PathBuf,
}

impl SessionDirEnvGuard {
    fn install(home_label: &str, session_dir_env: Option<&str>) -> Self {
        let lock = SESSION_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home_dir = temp_dir(home_label);
        let prev_home = env::var_os("HOME");
        let prev_session_dir = env::var_os("SQUEEZY_SESSION_DIR");
        // SAFETY: SESSION_DIR_ENV_LOCK serializes all callers in this module.
        unsafe {
            env::set_var("HOME", &home_dir);
            match session_dir_env {
                Some(value) => env::set_var("SQUEEZY_SESSION_DIR", value),
                None => env::remove_var("SQUEEZY_SESSION_DIR"),
            }
        }
        Self {
            _lock: lock,
            prev_home,
            prev_session_dir,
            home_dir,
        }
    }
}

impl Drop for SessionDirEnvGuard {
    fn drop(&mut self) {
        // SAFETY: the lock held in `_lock` keeps these mutations serialized.
        unsafe {
            match &self.prev_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }
            match &self.prev_session_dir {
                Some(value) => env::set_var("SQUEEZY_SESSION_DIR", value),
                None => env::remove_var("SQUEEZY_SESSION_DIR"),
            }
        }
        let _ = fs::remove_dir_all(&self.home_dir);
    }
}

#[test]
fn config_from_cli_uses_env_session_dir_when_flag_omitted() {
    let _guard = SessionDirEnvGuard::install("session-dir-env-only", Some("/env/sessions"));

    let cli = Cli::try_parse_from(["squeezy"]).expect("parse");
    let config = config_from_cli(&cli).expect("resolve config");

    assert_eq!(
        config.session_logs.log_dir,
        Some(PathBuf::from("/env/sessions"))
    );
    assert!(
        config.config_sources.iter().any(|source| source == "env"),
        "env source should be tagged when SQUEEZY_SESSION_DIR is consumed; got {:?}",
        config.config_sources,
    );
}

#[test]
fn config_from_cli_session_dir_flag_overrides_env_session_dir() {
    // CLI flag must beat both env and config; we set the env to verify the
    // flag is the final word in the resolution order.
    let _guard = SessionDirEnvGuard::install("session-dir-cli-vs-env", Some("/env/sessions"));

    let cli = Cli::try_parse_from(["squeezy", "--session-dir", "/cli/sessions"]).expect("parse");
    let config = config_from_cli(&cli).expect("resolve config");

    assert_eq!(
        config.session_logs.log_dir,
        Some(PathBuf::from("/cli/sessions"))
    );
    assert!(
        config.config_sources.iter().any(|source| source == "cli"),
        "cli source should be tagged when --session-dir is consumed; got {:?}",
        config.config_sources,
    );
}

#[test]
fn config_from_cli_session_dir_falls_back_to_default_without_flag_or_env() {
    let _guard = SessionDirEnvGuard::install("session-dir-default", None);

    let cli = Cli::try_parse_from(["squeezy"]).expect("parse");
    let config = config_from_cli(&cli).expect("resolve config");

    // No flag, no env, and the temp HOME has no settings.toml → the resolved
    // log_dir stays unset and downstream callers fall back to the documented
    // default of `.squeezy/sessions`.
    assert_eq!(config.session_logs.log_dir, None);
}

/// Scripted `LlmProvider` used by the print-mode tool-loop tests.  Each
/// `stream_response` call drains the next pre-baked event list, so a
/// test can simulate "first round emits a tool call, second round emits
/// the final answer" against a real `Agent::new` instance — which is
/// the only way to verify that print mode now sees the same tool
/// registry as interactive mode.
struct PrintModeScriptedProvider {
    name: &'static str,
    responses: std::sync::Mutex<std::collections::VecDeque<Vec<squeezy_llm::LlmEvent>>>,
    captured_requests: std::sync::Mutex<Vec<squeezy_llm::LlmRequest>>,
}

impl PrintModeScriptedProvider {
    fn new(responses: Vec<Vec<squeezy_llm::LlmEvent>>) -> Self {
        Self {
            name: "print-mode-scripted",
            responses: std::sync::Mutex::new(responses.into()),
            captured_requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn captured_request_count(&self) -> usize {
        self.captured_requests
            .lock()
            .expect("captured requests")
            .len()
    }

    /// Concatenate every user-text input item from every captured request
    /// into one string. The bang-bang regression test uses this to assert
    /// that the suppressed `!!cmd` body never travels to the LLM, even as
    /// quoted history attached to a later turn.
    fn captured_user_text(&self) -> String {
        self.captured_requests
            .lock()
            .expect("captured requests")
            .iter()
            .flat_map(|request| {
                request
                    .input
                    .iter()
                    .filter_map(|item| match item {
                        squeezy_llm::LlmInputItem::UserText(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl squeezy_llm::LlmProvider for PrintModeScriptedProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(
        &self,
        request: squeezy_llm::LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> squeezy_llm::LlmStream {
        self.captured_requests
            .lock()
            .expect("captured requests")
            .push(request);
        let events = self
            .responses
            .lock()
            .expect("scripted responses")
            .pop_front()
            .expect("scripted response present");
        let mapped = events
            .into_iter()
            .map(Ok::<_, squeezy_core::SqueezyError>)
            .collect::<Vec<_>>();
        let stream: std::pin::Pin<
            Box<
                dyn futures_core::Stream<Item = squeezy_core::Result<squeezy_llm::LlmEvent>> + Send,
            >,
        > = Box::pin(futures_util::stream::iter(mapped));
        stream
    }
}

fn print_mode_test_config(workspace_root: PathBuf) -> AppConfig {
    AppConfig {
        workspace_root,
        permissions: squeezy_core::PermissionPolicy {
            edit: squeezy_core::PermissionMode::Allow,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn print_mode_runs_tools_through_agent_loop_in_text_mode() {
    // This is the load-bearing regression test for F07: print mode used
    // to advertise zero tools, so any `read_file` request the model
    // emitted vanished into "tool call requested but prompt mode has no
    // tools". Building the same `Agent` interactive mode uses must now
    // actually drive the tool — observe that by checking the file
    // contents flow back to the model's second-round assistant text.
    let root = temp_dir("print-mode-text");
    fs::write(root.join("README.md"), "# squeezy readme\n").expect("write readme");

    let provider = Arc::new(PrintModeScriptedProvider::new(vec![
        vec![
            squeezy_llm::LlmEvent::Started,
            squeezy_llm::LlmEvent::ToolCall(squeezy_llm::LlmToolCall {
                call_id: "read_readme".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "README.md"}),
            }),
            squeezy_llm::LlmEvent::completed(
                Some("resp_round_1".to_string()),
                squeezy_core::CostSnapshot::default(),
            ),
        ],
        vec![
            squeezy_llm::LlmEvent::Started,
            squeezy_llm::LlmEvent::TextDelta("Title: # squeezy readme".to_string()),
            squeezy_llm::LlmEvent::completed(
                Some("resp_round_final".to_string()),
                squeezy_core::CostSnapshot::default(),
            ),
        ],
    ]));

    let config = print_mode_test_config(root.clone());
    let agent = Agent::new(config, provider);
    let rx = agent.start_turn(
        "read README.md and tell me the title".to_string(),
        tokio_util::sync::CancellationToken::new(),
    );

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    pump_prompt_events(rx, PromptFormat::Default, &mut stdout, &mut stderr)
        .await
        .expect("print-mode turn completes");

    let stdout_text = String::from_utf8(stdout).expect("utf-8 stdout");
    let stderr_text = String::from_utf8(stderr).expect("utf-8 stderr");

    assert!(
        stdout_text.contains("Title: # squeezy readme"),
        "stdout should carry the final assistant text; got: {stdout_text:?}"
    );
    assert!(
        stderr_text.contains("read_file"),
        "stderr should announce the read_file call; got: {stderr_text:?}"
    );
    assert!(
        stderr_text.contains("-> ok"),
        "stderr should show a successful tool status; got: {stderr_text:?}"
    );
    assert!(
        stderr_text.contains("tokens:"),
        "stderr should print the cost summary on completion; got: {stderr_text:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn print_mode_emits_tool_events_as_jsonl() {
    // Same flow as the text-mode regression test, but verifies the
    // experimental `--format json` schema picked up tool events.  Each
    // newline-delimited object must be a valid `PromptWireEvent`; the
    // `tool_call_started` / `tool_call_completed` entries are the
    // user-visible contract that print mode now has tools.
    let root = temp_dir("print-mode-json");
    fs::write(root.join("README.md"), "# squeezy readme\n").expect("write readme");

    let provider = Arc::new(PrintModeScriptedProvider::new(vec![
        vec![
            squeezy_llm::LlmEvent::Started,
            squeezy_llm::LlmEvent::ToolCall(squeezy_llm::LlmToolCall {
                call_id: "read_readme".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "README.md"}),
            }),
            squeezy_llm::LlmEvent::completed(
                Some("resp_round_1".to_string()),
                squeezy_core::CostSnapshot::default(),
            ),
        ],
        vec![
            squeezy_llm::LlmEvent::Started,
            squeezy_llm::LlmEvent::TextDelta("done".to_string()),
            squeezy_llm::LlmEvent::completed(
                Some("resp_round_final".to_string()),
                squeezy_core::CostSnapshot::default(),
            ),
        ],
    ]));

    let config = print_mode_test_config(root.clone());
    let agent = Agent::new(config, provider);
    let rx = agent.start_turn(
        "read README.md".to_string(),
        tokio_util::sync::CancellationToken::new(),
    );

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    pump_prompt_events(rx, PromptFormat::Json, &mut stdout, &mut stderr)
        .await
        .expect("print-mode JSON turn completes");

    let stdout_text = String::from_utf8(stdout).expect("utf-8 stdout");
    let mut types = Vec::new();
    let mut saw_tool_started = false;
    let mut saw_tool_completed = false;
    for line in stdout_text.lines() {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("non-JSON line {line:?}: {err}"));
        let ty = value["type"].as_str().expect("type tag").to_string();
        if ty == "tool_call_started" {
            saw_tool_started = true;
            assert_eq!(value["data"]["name"], "read_file");
            assert_eq!(value["data"]["call_id"], "read_readme");
        }
        if ty == "tool_call_completed" {
            saw_tool_completed = true;
            assert_eq!(value["data"]["tool_name"], "read_file");
            assert_eq!(value["data"]["status"], "Success");
        }
        types.push(ty);
    }
    assert!(
        saw_tool_started,
        "expected a tool_call_started event; saw {types:?}"
    );
    assert!(
        saw_tool_completed,
        "expected a tool_call_completed event; saw {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "completed"),
        "expected a final completed event; saw {types:?}",
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn print_mode_auto_approves_ask_capability_so_ci_does_not_hang() {
    // Permission policy with `shell = Ask` would have stalled the CLI
    // forever waiting for an operator in print mode; the auto-approval
    // path in `pump_prompt_events` is what unblocks scripted callers.
    // Exercising the approval channel directly keeps this test fast
    // (no real shell process) while still proving the wiring.
    use squeezy_agent::{ToolApprovalDecision, ToolApprovalRequest};
    use squeezy_core::{PermissionCapability, PermissionRequest, PermissionRisk, PermissionScope};

    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();

    let permission = PermissionRequest {
        call_id: "shell_call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Shell,
        target: "echo hi".to_string(),
        risk: PermissionRisk::Medium,
        summary: "echo hi".to_string(),
        metadata: Default::default(),
        suggested_rules: Vec::new(),
    };
    let request = ToolApprovalRequest {
        id: 1,
        call_id: "shell_call".to_string(),
        tool_name: "shell".to_string(),
        scope: PermissionScope::Shell,
        permission,
        matched_rule: None,
        reason: "default shell permission is ask".to_string(),
        context: None,
        preview: Vec::new(),
    };
    tx.send(squeezy_agent::AgentEvent::ApprovalRequested {
        turn_id: squeezy_core::TurnId::new(0),
        request,
        decision_tx,
    })
    .await
    .expect("send approval request");
    tx.send(squeezy_agent::AgentEvent::Completed {
        turn_id: squeezy_core::TurnId::new(0),
        message: squeezy_core::TranscriptItem::assistant(String::new()),
        response_id: None,
        cost: squeezy_core::CostSnapshot::default(),
        metrics: squeezy_core::TurnMetrics::default(),
        context_estimate: squeezy_core::ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
        session_cost: None,
    })
    .await
    .expect("send completed");
    drop(tx);

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    pump_prompt_events(rx, PromptFormat::Default, &mut stdout, &mut stderr)
        .await
        .expect("pump completes");

    let decision = decision_rx.await.expect("approval decided");
    assert!(matches!(decision, ToolApprovalDecision::AllowOnce));
    let stderr_text = String::from_utf8(stderr).expect("utf-8 stderr");
    assert!(
        stderr_text.contains("auto-approving shell"),
        "stderr should announce the auto-approval; got: {stderr_text:?}"
    );
}

#[tokio::test]
async fn pump_prompts_runs_bang_bang_prompt_in_text_mode_without_llm_context() {
    // A `--prompt "!!cmd"` value must execute locally while staying out
    // of the LLM transcript that follow-on prompts will see. We bake
    // exactly one scripted response for the normal prompt; the quiet
    // shell command should complete without consuming a provider
    // response, and the second prompt should not see the command or its
    // output in context.
    let root = temp_dir("pump-prompts-bang");

    let provider = Arc::new(PrintModeScriptedProvider::new(vec![vec![
        squeezy_llm::LlmEvent::Started,
        squeezy_llm::LlmEvent::TextDelta("normal answer".to_string()),
        squeezy_llm::LlmEvent::completed(
            Some("resp_normal".to_string()),
            squeezy_core::CostSnapshot::default(),
        ),
    ]]));

    let config = print_mode_test_config(root.clone());
    let agent = Agent::new(config, provider.clone());
    let prompts = vec![
        print_mode::PromptInput {
            content: "printf quiet-bang".to_string(),
            exclude_from_context: true,
        },
        print_mode::PromptInput {
            content: "explain README".to_string(),
            exclude_from_context: false,
        },
    ];

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    pump_prompts(
        &agent,
        prompts,
        PromptFormat::Default,
        &mut stdout,
        &mut stderr,
    )
    .await
    .expect("pump completes");

    let stdout_text = String::from_utf8(stdout).expect("utf-8 stdout");
    let stderr_text = String::from_utf8(stderr).expect("utf-8 stderr");

    assert!(
        stdout_text.contains("quiet-bang"),
        "stdout should include the local shell command output; got: {stdout_text:?}"
    );
    assert!(
        stdout_text.contains("normal answer"),
        "stdout should still stream the non-! prompt's answer; got: {stdout_text:?}"
    );
    assert!(
        stderr_text.contains("tool: shell") && stderr_text.contains("-> ok"),
        "stderr should show the local shell tool execution; got: {stderr_text:?}"
    );
    assert_eq!(
        provider.captured_request_count(),
        1,
        "the scripted provider should have been driven exactly once (for the normal prompt only)",
    );
    let user_text = provider.captured_user_text();
    assert!(
        !user_text.contains("printf quiet-bang") && !user_text.contains("quiet-bang"),
        "the quiet bang exchange must never reach the LLM transcript; observed user text: {user_text:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn pump_prompts_emits_bang_bang_tool_events_in_json_mode() {
    // The JSON wire schema is the public contract callers consume when
    // they pipe `--prompt --format json`. A `!!cmd` prompt must now look
    // like a local shell turn: tool start, tool completion, and turn
    // completion, without a provider request.
    let root = temp_dir("pump-prompts-bang-json");

    let provider = Arc::new(PrintModeScriptedProvider::new(Vec::new()));
    let config = print_mode_test_config(root.clone());
    let agent = Agent::new(config, provider.clone());
    let prompts = vec![print_mode::PromptInput {
        content: "printf json-bang".to_string(),
        exclude_from_context: true,
    }];

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    pump_prompts(
        &agent,
        prompts,
        PromptFormat::Json,
        &mut stdout,
        &mut stderr,
    )
    .await
    .expect("pump completes");

    let stdout_text = String::from_utf8(stdout).expect("utf-8 stdout");
    let mut saw_tool_started = false;
    let mut saw_tool_completed = false;
    let mut saw_completed = false;
    for line in stdout_text.lines() {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("non-JSON line {line:?}: {err}"));
        match value["type"].as_str() {
            Some("tool_call_started") => {
                saw_tool_started = true;
                assert_eq!(value["data"]["name"], "shell");
            }
            Some("tool_call_completed") => {
                saw_tool_completed = true;
                assert_eq!(value["data"]["tool_name"], "shell");
                assert_eq!(value["data"]["status"], "Success");
                assert_eq!(value["data"]["content"]["stdout"], "json-bang");
            }
            Some("completed") => saw_completed = true,
            Some("excluded_from_context") => {
                panic!("!! prompt should execute, not emit a deferral event: {value:?}");
            }
            _ => {}
        }
    }
    assert!(
        saw_tool_started && saw_tool_completed && saw_completed,
        "expected shell tool and completion events for the !! prompt; raw stdout: {stdout_text:?}"
    );
    assert_eq!(
        provider.captured_request_count(),
        0,
        "no normal prompts were queued, so the provider must never be called",
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn pump_prompts_runs_normal_prompts_unchanged_when_no_bang_bang_present() {
    // Guardrail: the new pump_prompts helper must be a drop-in for the
    // pre-F07 loop when nothing is suppressed. Two ordinary prompts
    // should consume two scripted responses, in order, and surface both
    // assistant tokens on stdout.
    let root = temp_dir("pump-prompts-normal");

    let provider = Arc::new(PrintModeScriptedProvider::new(vec![
        vec![
            squeezy_llm::LlmEvent::Started,
            squeezy_llm::LlmEvent::TextDelta("first ".to_string()),
            squeezy_llm::LlmEvent::completed(
                Some("resp_a".to_string()),
                squeezy_core::CostSnapshot::default(),
            ),
        ],
        vec![
            squeezy_llm::LlmEvent::Started,
            squeezy_llm::LlmEvent::TextDelta("second".to_string()),
            squeezy_llm::LlmEvent::completed(
                Some("resp_b".to_string()),
                squeezy_core::CostSnapshot::default(),
            ),
        ],
    ]));
    let config = print_mode_test_config(root.clone());
    let agent = Agent::new(config, provider.clone());
    let prompts = vec![
        print_mode::PromptInput {
            content: "first prompt".to_string(),
            exclude_from_context: false,
        },
        print_mode::PromptInput {
            content: "second prompt".to_string(),
            exclude_from_context: false,
        },
    ];

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    pump_prompts(
        &agent,
        prompts,
        PromptFormat::Default,
        &mut stdout,
        &mut stderr,
    )
    .await
    .expect("pump completes");

    let stdout_text = String::from_utf8(stdout).expect("utf-8 stdout");
    assert!(
        stdout_text.contains("first "),
        "stdout should carry the first turn's text; got: {stdout_text:?}"
    );
    assert!(
        stdout_text.contains("second"),
        "stdout should carry the second turn's text; got: {stdout_text:?}"
    );
    assert_eq!(
        provider.captured_request_count(),
        2,
        "both non-! prompts should have driven the provider exactly once each",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn project_init_target_resolves_ancestor_then_falls_back_to_cwd() {
    let root = temp_dir("project-init-target");
    let canonical_root = fs::canonicalize(&root).expect("canonicalize root");

    // No ancestor squeezy.toml exists yet: writing happens in the cwd itself.
    let sub = root.join("crates").join("foo");
    fs::create_dir_all(&sub).expect("mkdir sub");
    assert_eq!(
        project_init_target(&sub),
        sub.join(PROJECT_SETTINGS_FILE),
        "without an ancestor file, init writes into the current directory",
    );

    // With a root squeezy.toml, init from a subdirectory must target the
    // existing ancestor file instead of creating a shadowing closer one.
    let root_settings = canonical_root.join(PROJECT_SETTINGS_FILE);
    fs::write(&root_settings, "[session]\nmode = \"plan\"\n").expect("write root settings");
    assert_eq!(
        project_init_target(&sub),
        root_settings,
        "with an ancestor file, init must target it rather than the subdirectory",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn config_explain_matches_concrete_wildcard_field_paths() {
    let exact = find_config_field_for_path(&["model", "provider"]).expect("exact field");
    assert_eq!(exact.toml_path, ["model", "provider"]);

    let provider = find_config_field_for_path(&["providers", "openai", "cheap_model"])
        .expect("provider field");
    assert_eq!(provider.toml_path, ["providers", "*", "cheap_model"]);

    let model_limit =
        find_config_field_for_path(&["model_limits", "openai:gpt-5.5", "context_window"])
            .expect("model limit field");
    assert_eq!(
        model_limit.toml_path,
        ["model_limits", "*", "context_window"]
    );

    assert!(find_config_field_for_path(&["providers", "openai"]).is_none());
}

#[test]
fn config_explain_resolves_wildcard_sources_with_concrete_path() {
    use squeezy_core::{SeparatedSources, TierSource, config_schema::FieldSource};

    let user_doc = "[providers.openai]\ncheap_model = \"user-mini\"\n"
        .parse::<toml_edit::DocumentMut>()
        .expect("parse user doc");
    let repo_doc = "[providers.openai]\ncheap_model = \"local-mini\"\n"
        .parse::<toml_edit::DocumentMut>()
        .expect("parse local doc");
    let sources = SeparatedSources {
        user: Some(TierSource {
            path: std::path::PathBuf::from("user.toml"),
            doc: user_doc,
        }),
        project: None,
        repo: Some(TierSource {
            path: std::path::PathBuf::from("local.toml"),
            doc: repo_doc,
        }),
        user_path_default: std::path::PathBuf::from("user.toml"),
        project_path_default: std::path::PathBuf::from("squeezy.toml"),
        repo_path_default: std::path::PathBuf::from("local.toml"),
    };

    let requested = ["providers", "openai", "cheap_model"];
    let field = find_config_field_for_path(&requested).expect("provider field");
    assert_eq!(
        resolve_explain_field_source(&sources, field, &requested),
        FieldSource::Repo
    );

    let other_provider = ["providers", "anthropic", "cheap_model"];
    assert_eq!(
        resolve_explain_field_source(&sources, field, &other_provider),
        FieldSource::Default
    );
}

#[test]
fn config_explain_displays_concrete_wildcard_values() {
    let mut config = AppConfig::default();
    config.providers.insert(
        "anthropic".to_string(),
        squeezy_core::ProviderSettings {
            cheap_model: Some("custom-haiku".to_string()),
            ..Default::default()
        },
    );
    config.model_limits.insert(
        "anthropic:claude-sonnet-4-6".to_string(),
        squeezy_core::ModelLimitOverride {
            context_window: Some(123_456),
        },
    );

    let provider_path = ["providers", "anthropic", "cheap_model"];
    let provider_field = find_config_field_for_path(&provider_path).expect("provider field");
    assert_eq!(
        explain_effective_value(&config, provider_field, &provider_path),
        "custom-haiku"
    );

    let model_limit_path = [
        "model_limits",
        "anthropic:claude-sonnet-4-6",
        "context_window",
    ];
    let model_limit_field = find_config_field_for_path(&model_limit_path).expect("limit field");
    assert_eq!(
        explain_effective_value(&config, model_limit_field, &model_limit_path),
        "123456"
    );
}

#[test]
fn split_config_field_path_handles_bare_dotted_keys() {
    assert_eq!(
        split_config_field_path("model.provider").expect("parse"),
        vec!["model", "provider"]
    );
    assert_eq!(
        split_config_field_path("tui.tick_rate_ms").expect("parse"),
        vec!["tui", "tick_rate_ms"]
    );
}

#[test]
fn split_config_field_path_respects_basic_string_quoting() {
    // Realistic model id with a dot — the dominant case for `model_limits`.
    let parts =
        split_config_field_path(r#"model_limits."openai:gpt-5.5".context_window"#).expect("parse");
    assert_eq!(
        parts,
        vec!["model_limits", "openai:gpt-5.5", "context_window"]
    );
}

#[test]
fn split_config_field_path_respects_literal_string_quoting() {
    let parts = split_config_field_path("providers.'weird.alias'.cheap_model").expect("parse");
    assert_eq!(parts, vec!["providers", "weird.alias", "cheap_model"]);
}

#[test]
fn split_config_field_path_rejects_unterminated_quote() {
    let err = split_config_field_path(r#"model_limits."openai:gpt-5.5"#).expect_err("err");
    assert!(
        err.contains("unterminated"),
        "expected an unterminated-quote error, got: {err}"
    );
}

#[test]
fn split_config_field_path_rejects_empty_segments_and_input() {
    assert!(split_config_field_path("").is_err());
    assert!(split_config_field_path("model..provider").is_err());
    assert!(split_config_field_path(".model.provider").is_err());
    assert!(split_config_field_path("model.provider.").is_err());
}

#[test]
fn split_config_field_path_rejects_junk_after_closing_quote() {
    let err = split_config_field_path(r#"model_limits."openai:gpt-5.5"junk.context_window"#)
        .expect_err("err");
    assert!(
        err.contains("after closing quote"),
        "expected post-quote diagnostic, got: {err}"
    );
}

#[test]
fn config_explain_resolves_dotted_model_id_via_quoted_path() {
    let mut config = AppConfig::default();
    config.model_limits.insert(
        "openai:gpt-5.5".to_string(),
        squeezy_core::ModelLimitOverride {
            context_window: Some(2_000_000),
        },
    );

    let raw_path = r#"model_limits."openai:gpt-5.5".context_window"#;
    let parts_owned = split_config_field_path(raw_path).expect("parse");
    let parts: Vec<&str> = parts_owned.iter().map(String::as_str).collect();

    let field = find_config_field_for_path(&parts).expect("schema match");
    assert_eq!(field.toml_path, ["model_limits", "*", "context_window"]);
    assert_eq!(explain_effective_value(&config, field, &parts), "2000000");
}

#[test]
fn config_explain_rejects_unquoted_dotted_model_id_and_hints_quoting() {
    // The naïve `split('.')` behavior breaks `gpt-5.5` into two parts and
    // produces a 4-segment path that can never match the 3-segment schema.
    // We keep that failure mode (TOML semantics force it), but surface a
    // hint mentioning the quoted spelling.
    let parts_owned = split_config_field_path("model_limits.openai:gpt-5.5.context_window")
        .expect("split should still succeed for an unquoted dot path");
    let parts: Vec<&str> = parts_owned.iter().map(String::as_str).collect();
    assert!(
        find_config_field_for_path(&parts).is_none(),
        "unquoted dotted model id must fall through to the unknown-field branch",
    );
}

#[test]
fn explain_effective_value_redacts_secret_fields() {
    use squeezy_core::config_schema::{ApplyTier, FieldKind, FieldMeta, FieldValue};

    fn raw_string_get(_: &AppConfig) -> FieldValue {
        // Stand in for a misbehaving getter that returns plaintext rather than
        // `FieldValue::Secret`. The redaction in `explain_effective_value`
        // must not depend on the getter's discipline.
        FieldValue::String("super-secret-token".to_string())
    }
    fn noop_set(_: &mut AppConfig, _: FieldValue) -> Result<(), &'static str> {
        Ok(())
    }
    fn unset_default() -> FieldValue {
        FieldValue::Unset
    }

    let kind_secret = FieldMeta {
        label: "api key",
        toml_path: &["providers", "openai", "api_key"],
        kind: FieldKind::Secret {
            env_var: "OPENAI_API_KEY",
        },
        tier: ApplyTier::NextPrompt,
        get: raw_string_get,
        set: noop_set,
        default_display: "—",
        default: unset_default,
        help: "",
        env_override: None,
        secret: false,
    };

    let flag_secret = FieldMeta {
        label: "api key (flagged)",
        toml_path: &["providers", "openai", "api_key"],
        kind: FieldKind::String { multiline: false },
        tier: ApplyTier::NextPrompt,
        get: raw_string_get,
        set: noop_set,
        default_display: "—",
        default: unset_default,
        help: "",
        env_override: None,
        secret: true,
    };

    let config = AppConfig::default();
    let requested = ["providers", "openai", "api_key"];

    assert_eq!(
        explain_effective_value(&config, &kind_secret, &requested).to_string(),
        "••••",
        "FieldKind::Secret must short-circuit to the redacted sentinel"
    );
    assert_eq!(
        explain_effective_value(&config, &flag_secret, &requested).to_string(),
        "••••",
        "FieldMeta::secret = true must short-circuit to the redacted sentinel"
    );
}

#[test]
fn skills_upsert_entry_inserts_when_no_match() {
    let mut entries = toml_edit::ArrayOfTables::new();
    let selector = SkillsSelector {
        name: Some("alpha".to_string()),
        path: None,
    };
    skills_upsert_entry(&mut entries, &selector, false).expect("upsert");
    assert_eq!(entries.len(), 1);
    let first = entries.iter().next().expect("entry");
    assert_eq!(first.get("name").and_then(|v| v.as_str()), Some("alpha"));
    assert_eq!(first.get("enabled").and_then(|v| v.as_bool()), Some(false));
}

#[test]
fn skills_upsert_entry_updates_existing_by_name() {
    let mut entries = toml_edit::ArrayOfTables::new();
    let mut existing = toml_edit::Table::new();
    existing.insert(
        "name",
        toml_edit::Item::Value(toml_edit::Value::from("alpha")),
    );
    existing.insert(
        "enabled",
        toml_edit::Item::Value(toml_edit::Value::from(true)),
    );
    entries.push(existing);

    let selector = SkillsSelector {
        name: Some("alpha".to_string()),
        path: None,
    };
    skills_upsert_entry(&mut entries, &selector, false).expect("upsert");
    assert_eq!(entries.len(), 1, "must update in place, not duplicate");
    let updated = entries.iter().next().expect("entry");
    assert_eq!(
        updated.get("enabled").and_then(|v| v.as_bool()),
        Some(false)
    );
}

#[test]
fn skills_upsert_entry_inserts_by_path_when_no_match() {
    let mut entries = toml_edit::ArrayOfTables::new();
    let selector = SkillsSelector {
        name: None,
        path: Some(PathBuf::from("/skills/alpha")),
    };
    skills_upsert_entry(&mut entries, &selector, true).expect("upsert");
    assert_eq!(entries.len(), 1);
    let first = entries.iter().next().expect("entry");
    assert_eq!(
        first.get("path").and_then(|v| v.as_str()),
        Some("/skills/alpha")
    );
    assert_eq!(first.get("enabled").and_then(|v| v.as_bool()), Some(true));
    assert!(first.get("name").is_none());
}
