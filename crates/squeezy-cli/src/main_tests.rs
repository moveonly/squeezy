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
}

impl PrintModeScriptedProvider {
    fn new(responses: Vec<Vec<squeezy_llm::LlmEvent>>) -> Self {
        Self {
            name: "print-mode-scripted",
            responses: std::sync::Mutex::new(responses.into()),
        }
    }
}

impl squeezy_llm::LlmProvider for PrintModeScriptedProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(
        &self,
        _request: squeezy_llm::LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> squeezy_llm::LlmStream {
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
