use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use ratatui::backend::TestBackend;
use squeezy_core::{
    AppConfig, CostSnapshot, PermissionCapability, PermissionMode, PermissionPolicy,
    PermissionRequest, PermissionRisk, PermissionScope, Role, SessionMode, StatusVerbosity,
    TuiConfig, TurnId, TurnMetrics,
};
use squeezy_llm::UnavailableProvider;

use super::*;

#[test]
fn app_starts_ready_with_empty_transcript() {
    let config = test_config(SessionMode::Build);
    let app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );

    assert_eq!(app.provider_name, "openai");
    assert_eq!(app.model, "gpt-test");
    assert_eq!(app.mode, SessionMode::Build);
    assert_eq!(app.status, "ready");
    assert!(app.transcript.is_empty());
}

#[test]
fn app_surfaces_onboarding_summary_once() {
    let config = test_config(SessionMode::Build);
    let app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        Some("repo profile created: /tmp/project".to_string()),
        Box::new(NoopClipboard),
    );

    assert_eq!(app.status, "repo profile ready");
    assert_eq!(app.transcript.len(), 1);
    assert_eq!(
        app.transcript[0].content,
        "repo profile created: /tmp/project"
    );
    // The seeded onboarding summary is Squeezy-authored metadata, not an
    // assistant turn; surfacing it under Role::System keeps provenance
    // honest and avoids visual collision with later assistant deltas.
    assert_eq!(app.transcript[0].role, Role::System);
}

#[test]
fn status_line_surfaces_current_mode_and_switch_hints() {
    let config = test_config(SessionMode::Plan);
    let mut app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Plan,
        None,
        Box::new(NoopClipboard),
    );
    app.status = "ready".to_string();

    let status = format_status_tokens(&app);
    assert!(status.contains("mode=plan"), "missing mode: {status}");
    assert!(
        status.contains("Shift-Tab mode"),
        "missing toggle hint: {status}",
    );
    assert!(
        status.contains("Ctrl-Y copy"),
        "missing copy hint: {status}"
    );
}

#[tokio::test]
async fn shift_tab_toggles_mode() {
    let agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Plan);
    assert_eq!(agent.session_mode(), SessionMode::Plan);
    assert_eq!(app.status, "mode switched to plan");

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Build);
    assert_eq!(agent.session_mode(), SessionMode::Build);
    assert_eq!(app.status, "mode switched to build");
}

#[tokio::test]
async fn slash_plan_and_build_force_modes() {
    let agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    app.input = "/plan".to_string();
    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Plan);
    assert_eq!(agent.session_mode(), SessionMode::Plan);
    assert!(app.input.is_empty());

    app.input = "/plan".to_string();
    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Plan);
    assert_eq!(app.status, "already in plan mode");

    app.input = "/build".to_string();
    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Build);
    assert_eq!(agent.session_mode(), SessionMode::Build);
    assert_eq!(app.status, "mode switched to build");
}

#[tokio::test]
async fn mode_switch_is_refused_during_active_turn() {
    let agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(app.mode, SessionMode::Build);
    assert_eq!(agent.session_mode(), SessionMode::Build);
    assert_eq!(app.status, "mode switch unavailable during active turn");
}

#[test]
fn transcript_item_formats_role_label() {
    let item = TranscriptItem::user("hello");
    let line = format_transcript_item(&item);
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert_eq!(text, "user hello");
}

#[test]
fn approval_prompt_surfaces_risk_target_and_persistence_keys() {
    let permission = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Compiler,
        target: "cargo test:*".to_string(),
        risk: PermissionRisk::Medium,
        summary: "shell description=\"run tests\"".to_string(),
        metadata: BTreeMap::from([
            ("command".to_string(), "cargo test".to_string()),
            ("cwd".to_string(), ".".to_string()),
            ("env".to_string(), "allowlist (values redacted)".to_string()),
            ("network".to_string(), "none".to_string()),
            ("destructive".to_string(), "false".to_string()),
            ("timeout_ms".to_string(), "30000".to_string()),
            ("output_byte_cap".to_string(), "32000".to_string()),
            ("sandbox".to_string(), "required".to_string()),
            ("sandbox_network".to_string(), "deny_by_default".to_string()),
        ]),
        suggested_rules: Vec::new(),
    };
    let request = ToolApprovalRequest {
        id: 1,
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        scope: PermissionScope::Shell,
        permission,
        matched_rule: None,
        reason: "default compiler permission is ask".to_string(),
    };

    let prompt = format_approval_prompt(&request);
    assert!(prompt.contains("risk=medium"), "missing risk: {prompt}");
    assert!(
        prompt.contains("target=cargo test:*"),
        "missing target: {prompt}",
    );
    assert!(
        prompt.contains("command=\"cargo test\""),
        "missing command metadata: {prompt}",
    );
    assert!(prompt.contains("cwd=\".\""), "missing cwd: {prompt}");
    assert!(
        prompt.contains("env=\"allowlist (values redacted)\""),
        "missing env: {prompt}"
    );
    assert!(
        prompt.contains("network=\"none\""),
        "missing network: {prompt}"
    );
    assert!(
        prompt.contains("destructive=\"false\""),
        "missing destructive flag: {prompt}",
    );
    assert!(
        prompt.contains("timeout_ms=\"30000\""),
        "missing timeout: {prompt}",
    );
    assert!(
        prompt.contains("output_byte_cap=\"32000\""),
        "missing output_byte_cap: {prompt}",
    );
    assert!(
        prompt.contains("sandbox=\"required\""),
        "missing sandbox mode: {prompt}",
    );
    assert!(
        prompt.contains("sandbox_network=\"deny_by_default\""),
        "missing sandbox_network: {prompt}",
    );
    // Reason text appears verbatim on its own line.
    assert!(
        prompt.contains("reason=default compiler permission is ask"),
        "missing reason: {prompt}",
    );
    // Multi-line format puts each field on its own line.
    let line_count = prompt.matches('\n').count();
    assert!(
        line_count >= 6,
        "expected multi-line prompt; got {line_count} newlines:\n{prompt}",
    );
    assert!(prompt.contains("[y] once"));
    assert!(prompt.contains("[a] user allow"));
    assert!(prompt.contains("[p] project allow"));
    assert!(prompt.contains("[u] user deny"));
    assert!(prompt.contains("[d] project deny"));
    assert!(prompt.contains("[n] deny once"));
}

#[test]
fn approval_status_line_is_compact_single_line() {
    let permission = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Shell,
        target: "shell:*".to_string(),
        risk: PermissionRisk::High,
        summary: "shell description=\"do stuff\"".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    };
    let request = ToolApprovalRequest {
        id: 1,
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        scope: PermissionScope::Shell,
        permission,
        matched_rule: None,
        reason: "default shell permission is ask".to_string(),
    };
    let line = format_approval_status_line(&request);
    assert!(!line.contains('\n'), "status line must be single line");
    assert!(line.contains("approval pending"));
    assert!(line.contains("risk=high"));
    assert!(line.contains("target=shell:*"));
}

#[test]
fn compact_status_surfaces_context_without_dense_counters() {
    let config = test_config(SessionMode::Build);
    let mut app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );
    app.repo = RepoStatus {
        branch: Some("feature".to_string()),
        changed_files: 2,
        operation: None,
        available: true,
    };
    app.status = "running search".to_string();

    let status = format_status_tokens(&app);
    assert!(status.contains("openai:gpt-test"), "{status}");
    assert!(status.contains("mode=build"), "{status}");
    assert!(status.contains("repo=feature*2"), "{status}");
    assert!(
        status.contains("perm=r:allow e:ask sh:ask web:ask"),
        "{status}"
    );
    assert!(
        status.contains("sandbox=required/net=deny_by_default"),
        "{status}"
    );
    assert!(status.contains("telemetry=on"), "{status}");
    assert!(status.contains("status=running search"), "{status}");
    assert!(
        status.contains("cost=- tok=-/- tools=0 budget=ok"),
        "{status}"
    );
    assert!(
        !status.contains("cfg="),
        "compact status should stay calm: {status}"
    );
}

#[test]
fn verbose_status_surfaces_budget_and_cache_details() {
    let mut config = test_config(SessionMode::Plan);
    config.tui = TuiConfig {
        status_verbosity: StatusVerbosity::Verbose,
        ..config.tui
    };
    let mut app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Plan,
        None,
        Box::new(NoopClipboard),
    );
    app.cost = CostSnapshot {
        input_tokens: Some(10),
        output_tokens: Some(5),
        cached_input_tokens: Some(7),
        cache_write_input_tokens: Some(3),
        estimated_usd_micros: Some(42),
    };
    app.metrics = TurnMetrics {
        tool_calls: 2,
        bytes_read: 1024,
        receipt_stub_hits: 1,
        budget_denials: 1,
        redactions: 4,
        ..Default::default()
    };

    let status = format_status_tokens(&app);
    assert!(status.contains("mode=plan"), "{status}");
    assert!(
        status.contains("cost=$0.000042 tok=10/5 tools=2 budget=denied:1"),
        "{status}"
    );
    assert!(status.contains("cfg="), "{status}");
    assert!(status.contains("read=1024B"), "{status}");
    assert!(status.contains("receipts=1"), "{status}");
    assert!(status.contains("redactions=4"), "{status}");
    assert!(status.contains("cached=7"), "{status}");
    assert!(status.contains("cache_write=3"), "{status}");
}

#[test]
fn render_uses_two_line_status_footer() {
    let config = test_config(SessionMode::Build);
    let mut app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );
    app.repo = RepoStatus {
        branch: Some("feature".to_string()),
        changed_files: 0,
        operation: None,
        available: true,
    };

    let output = render_to_string(&app, 100, 14);
    assert!(output.contains("openai:gpt-test"), "{output}");
    assert!(output.contains("repo=feature"), "{output}");
    assert!(output.contains("Ctrl-Y copy"), "{output}");
}

#[tokio::test]
async fn ctrl_y_copies_last_assistant_message() {
    let agent = test_agent(SessionMode::Build);
    let writes = Arc::new(StdMutex::new(Vec::new()));
    let mut app = test_app_with_clipboard(
        SessionMode::Build,
        Box::new(RecordingClipboard {
            writes: writes.clone(),
            error: None,
        }),
    );
    app.transcript.push(TranscriptItem::user("hello"));
    app.transcript.push(TranscriptItem::assistant("answer"));

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");

    assert_eq!(writes.lock().unwrap().as_slice(), ["answer"]);
    assert!(
        app.status.contains("copied assistant message"),
        "{}",
        app.status
    );
}

#[tokio::test]
async fn slash_copy_transcript_copies_plain_text_transcript() {
    let agent = test_agent(SessionMode::Build);
    let writes = Arc::new(StdMutex::new(Vec::new()));
    let mut app = test_app_with_clipboard(
        SessionMode::Build,
        Box::new(RecordingClipboard {
            writes: writes.clone(),
            error: None,
        }),
    );
    app.transcript.push(TranscriptItem::user("hello"));
    app.transcript.push(TranscriptItem::assistant("answer"));

    assert!(handle_slash_command(&mut app, &agent, "/copy transcript").await);
    assert_eq!(
        writes.lock().unwrap().as_slice(),
        ["user: hello\nassistant: answer"]
    );
    assert!(app.status.contains("copied transcript"), "{}", app.status);
}

#[tokio::test]
async fn copy_failure_is_actionable_status() {
    let agent = test_agent(SessionMode::Build);
    let mut app = test_app_with_clipboard(
        SessionMode::Build,
        Box::new(RecordingClipboard {
            writes: Arc::new(StdMutex::new(Vec::new())),
            error: Some("clipboard unavailable".to_string()),
        }),
    );
    app.transcript.push(TranscriptItem::assistant("answer"));

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");

    assert_eq!(app.status, "copy failed: clipboard unavailable");
}

#[tokio::test]
async fn transcript_navigation_keys_update_scroll_state() {
    let agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.transcript_scroll_from_bottom, 8);

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.transcript_scroll_from_bottom, 0);

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.transcript_scroll_from_bottom, u16::MAX);
}

#[test]
fn transcript_scroll_offset_defaults_to_bottom() {
    assert_eq!(transcript_scroll_offset(20, 10, 0), 12);
    assert_eq!(transcript_scroll_offset(20, 10, 8), 4);
    assert_eq!(transcript_scroll_offset(20, 10, u16::MAX), 0);
}

#[test]
fn common_errors_get_actionable_status_text() {
    let provider = format_error_status(&SqueezyError::ProviderNotConfigured("missing".into()));
    assert!(
        provider.contains("configure provider credentials"),
        "{provider}"
    );

    let denied = format_error_status(&SqueezyError::Permission("shell denied".into()));
    assert!(
        denied.contains("approve, adjust policy, or change request"),
        "{denied}"
    );
}

#[test]
fn repo_status_handles_non_git_roots() {
    let config = AppConfig {
        workspace_root: std::env::temp_dir(),
        ..test_config(SessionMode::Build)
    };

    assert_eq!(RepoStatus::detect(&config).compact(), "repo=none");
}

#[test]
fn base64_encoder_supports_osc52_payloads() {
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"a"), "YQ==");
    assert_eq!(base64_encode(b"ab"), "YWI=");
    assert_eq!(base64_encode(b"abc"), "YWJj");
}

#[tokio::test]
async fn assistant_delta_preserves_scroll_offset_in_history() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_scroll_from_bottom = 8;
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::AssistantDelta {
        turn_id: TurnId::new(1),
        delta: "streamed".to_string(),
    })
    .await
    .expect("send delta");
    drop(tx);

    drain_agent_events(&mut app).await;

    assert_eq!(
        app.transcript_scroll_from_bottom, 8,
        "history scroll must survive incoming deltas",
    );
    assert_eq!(app.pending_assistant, "streamed");
}

#[tokio::test]
async fn completed_event_preserves_scroll_offset_in_history() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_scroll_from_bottom = 5;
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("done"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
    })
    .await
    .expect("send completed");
    drop(tx);

    drain_agent_events(&mut app).await;

    assert_eq!(
        app.transcript_scroll_from_bottom, 5,
        "history scroll must survive turn completion",
    );
    assert_eq!(app.status, "ready");
    assert!(app.turn_rx.is_none());
}

#[tokio::test]
async fn scroll_keys_preserve_status_text() {
    let agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.status = "tool foo finished".to_string();

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(
        app.status, "tool foo finished",
        "PageUp must not clobber the status text",
    );

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(
        app.status, "tool foo finished",
        "Home must not clobber the status text",
    );

    handle_key(
        &mut app,
        &agent,
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(
        app.status, "tool foo finished",
        "End must not clobber the status text",
    );
}

#[test]
fn status_marker_surfaces_history_scroll_state() {
    let config = test_config(SessionMode::Build);
    let mut app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );

    let live = format_status_tokens(&app);
    assert!(
        !live.contains("scroll=history"),
        "no marker while at bottom: {live}",
    );

    app.transcript_scroll_from_bottom = 4;
    let scrolled = format_status_tokens(&app);
    assert!(
        scrolled.contains("scroll=history"),
        "missing scroll marker: {scrolled}",
    );
}

#[test]
fn osc52_clipboard_rejects_payloads_above_cap() {
    let mut clipboard = Osc52Clipboard;
    let oversized = "x".repeat(OSC52_MAX_PAYLOAD_BYTES + 1);
    let err = clipboard
        .copy_text(&oversized)
        .expect_err("oversized payload must fail");
    assert!(err.contains("exceeds"), "{err}");
    assert!(err.contains(&OSC52_MAX_PAYLOAD_BYTES.to_string()), "{err}");
}

fn test_app(mode: SessionMode) -> TuiApp {
    test_app_with_clipboard(mode, Box::new(NoopClipboard))
}

fn test_app_with_clipboard(mode: SessionMode, clipboard: Box<dyn Clipboard>) -> TuiApp {
    let config = test_config(mode);
    TuiApp::new_with_clipboard("scripted", &config, mode, None, clipboard)
}

fn test_config(mode: SessionMode) -> AppConfig {
    AppConfig {
        model: "gpt-test".to_string(),
        session_mode: mode,
        permissions: PermissionPolicy {
            read: PermissionMode::Allow,
            edit: PermissionMode::Ask,
            shell: PermissionMode::Ask,
            web: PermissionMode::Ask,
            ..Default::default()
        },
        config_sources: vec!["defaults".to_string()],
        ..Default::default()
    }
}

fn render_to_string(app: &TuiApp, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| render(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

struct NoopClipboard;

impl Clipboard for NoopClipboard {
    fn copy_text(&mut self, _text: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

struct RecordingClipboard {
    writes: Arc<StdMutex<Vec<String>>>,
    error: Option<String>,
}

impl Clipboard for RecordingClipboard {
    fn copy_text(&mut self, text: &str) -> std::result::Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.writes.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

fn test_agent(mode: SessionMode) -> Agent {
    Agent::new(
        AppConfig {
            session_mode: mode,
            ..Default::default()
        },
        Arc::new(UnavailableProvider::new("scripted", "test provider")),
    )
}
