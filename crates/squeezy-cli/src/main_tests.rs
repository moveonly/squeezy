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
        provider: "openai",
        model: "gpt-5.5".to_string(),
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        base_url: None,
        reasoning_effort: Some(ReasoningEffort::XHigh),
    };

    save_startup_model_selection(&path, &selection).expect("save selection");

    let text = fs::read_to_string(&path).expect("read settings");
    assert!(text.contains("read = \"deny\""));
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
    assert_eq!(cli.prompt.as_deref(), Some("hi"));
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
