use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::backend::TestBackend;
use squeezy_agent::{JobKind, JobStatus};
use squeezy_core::{
    AppConfig, CostSnapshot, PermissionCapability, PermissionMode, PermissionPolicy,
    PermissionRequest, PermissionRisk, PermissionScope, Role, SessionMode, StatusVerbosity,
    TaskStateSnapshot, TaskStateStatus, TaskStateStep, TaskStepStatus, TaskVerificationState,
    ToolOutputVerbosity, TuiConfig, TurnId, TurnMetrics,
};
use squeezy_llm::UnavailableProvider;
use squeezy_tools::{ToolCostHint, ToolReceipt, ToolResult, ToolStatus};

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
    let TranscriptEntryKind::Message(item) = &app.transcript[0].kind else {
        panic!("onboarding entry should be a message");
    };
    assert_eq!(item.content, "repo profile created: /tmp/project");
    // The seeded onboarding summary is Squeezy-authored metadata, not an
    // assistant turn; surfacing it under Role::System keeps provenance
    // honest and avoids visual collision with later assistant deltas.
    assert_eq!(item.role, Role::System);
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
        status.contains("Ctrl-E collapse"),
        "missing collapse hint: {status}"
    );
}

#[tokio::test]
async fn shift_tab_toggles_mode() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Plan);
    assert_eq!(agent.session_mode(), SessionMode::Plan);
    assert_eq!(app.status, "mode switched to plan");

    handle_key(
        &mut app,
        &mut agent,
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
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    app.input = "/plan".to_string();
    handle_key(
        &mut app,
        &mut agent,
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
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Plan);
    assert_eq!(app.status, "already in plan mode");

    app.input = "/build".to_string();
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Build);
    assert_eq!(agent.session_mode(), SessionMode::Build);
    assert_eq!(app.status, "mode switched to build");
}

#[tokio::test]
async fn slash_cost_reports_empty_session_without_model_turn() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/cost").await);

    let output = last_message_content(&app).expect("cost output");
    assert_eq!(app.status, "cost snapshot");
    assert!(output.contains("Cost accounting"), "{output}");
    assert!(
        output.contains("provider=scripted model=gpt-5-nano"),
        "{output}"
    );
    assert!(
        output.contains("provider_tokens input=- output=-"),
        "{output}"
    );
    assert!(output.contains("tools calls=0"), "{output}");
    assert!(app.jobs.is_empty());
}

#[tokio::test]
async fn slash_context_reports_known_model_budget_percentages() {
    let mut config = test_config(SessionMode::Build);
    config.model = squeezy_core::DEFAULT_OPENAI_MODEL.to_string();
    let mut agent = Agent::new(
        config.clone(),
        Arc::new(UnavailableProvider::new("openai", "test provider")),
    );
    let mut app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );

    assert!(handle_slash_command(&mut app, &mut agent, "/context").await);

    let output = last_message_content(&app).expect("context output");
    assert_eq!(app.status, "context snapshot");
    assert!(output.contains("Context accounting"), "{output}");
    assert!(output.contains("context_window=400000"), "{output}");
    assert!(output.contains("remaining_input_budget="), "{output}");
    assert!(output.contains("used="), "{output}");
    assert!(output.contains('%'), "{output}");
    assert!(app.jobs.is_empty());
}

#[tokio::test]
async fn slash_context_keeps_percentages_unknown_without_model_limits() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/context").await);

    let output = last_message_content(&app).expect("context output");
    assert!(output.contains("context_window=unknown"), "{output}");
    assert!(
        output.contains("remaining_input_budget=unknown"),
        "{output}"
    );
    assert!(output.contains("used=unknown"), "{output}");
    assert!(!output.contains('%'), "{output}");
}

#[tokio::test]
async fn multiline_paste_becomes_attached_context() {
    let root = temp_workspace("tui_paste");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    handle_paste(
        &mut app,
        &mut agent,
        "2026-05-24 ERROR failed\nOPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz\n".to_string(),
    )
    .await
    .expect("handle paste");

    assert_eq!(app.attachments.len(), 1);
    assert!(app.status.contains("attached paste"), "{}", app.status);
    assert!(
        !app.attachments[0]
            .preview
            .contains("sk-abcdefghijklmnopqrstuvwxyz")
    );
    let rendered = render_to_string(&app, 100, 20);
    assert!(
        rendered.contains(&app.attachments[0].id),
        "attachment should render: {rendered}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn small_single_line_paste_stays_in_prompt() {
    let root = temp_workspace("tui_inline_paste");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    handle_paste(&mut app, &mut agent, "small paste".to_string())
        .await
        .expect("handle paste");

    assert_eq!(app.input, "small paste");
    assert!(app.attachments.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn slash_attach_and_detach_update_active_context() {
    let root = temp_workspace("tui_attach");
    fs::write(
        root.join("error.log"),
        "2026-05-24 ERROR failed\n2026-05-24 WARN retry\n",
    )
    .expect("write log");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/attach error.log").await);
    assert_eq!(app.attachments.len(), 1);
    assert!(app.status.contains("attached file"), "{}", app.status);

    let id = app.attachments[0].id.clone();
    let command = format!("/detach {id}");
    assert!(handle_slash_command(&mut app, &mut agent, &command).await);
    assert!(app.attachments.is_empty());
    assert_eq!(app.status, format!("detached {id}"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn slash_attach_surfaces_unsupported_images() {
    let root = temp_workspace("tui_attach_image");
    fs::write(root.join("shot.png"), b"\x89PNG\r\n\x1a\nimage").expect("write image");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/attach shot.png").await);
    assert!(app.attachments.is_empty());
    assert!(app.status.contains("unsupported file"), "{}", app.status);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn mode_switch_is_refused_during_active_turn() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);

    handle_key(
        &mut app,
        &mut agent,
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
fn tool_result_entries_collapse_by_default_and_expand_when_toggled() {
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));

    assert!(app.transcript[0].collapsed);
    let collapsed = render_to_string(&app, 100, 12);
    assert!(collapsed.contains("tool result"), "{collapsed}");
    assert!(collapsed.contains("grep Success"), "{collapsed}");
    assert!(
        !collapsed.contains("needle found"),
        "collapsed view should hide payload: {collapsed}"
    );

    select_previous_transcript_entry(&mut app);
    toggle_selected_transcript_entry(&mut app);

    assert!(!app.transcript[0].collapsed);
    let expanded = render_to_string(&app, 100, 12);
    assert!(expanded.contains("needle found"), "{expanded}");
}

#[tokio::test]
async fn slash_collapse_and_expand_apply_to_tool_entries() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));

    assert!(handle_slash_command(&mut app, &mut agent, "/expand tools").await);
    assert!(!app.transcript[0].collapsed);

    assert!(handle_slash_command(&mut app, &mut agent, "/collapse tools").await);
    assert!(app.transcript[0].collapsed);
}

#[test]
fn tool_output_verbosity_changes_preview_length() {
    let result = sample_tool_result("grep", &"x".repeat(1_000));
    let compact = preview_tool_result(&result, ToolOutputVerbosity::Compact);
    let verbose = preview_tool_result(&result, ToolOutputVerbosity::Verbose);

    assert!(compact.len() < verbose.len());
    assert!(compact.ends_with("..."), "{compact}");
}

#[test]
fn reasoning_usage_status_is_hidden_when_disabled() {
    let mut app = test_app(SessionMode::Build);
    app.cost = CostSnapshot {
        input_tokens: Some(10),
        output_tokens: Some(5),
        reasoning_output_tokens: Some(3),
        ..CostSnapshot::default()
    };

    let visible = format_status_tokens(&app);
    assert!(visible.contains("reasoning=3"), "{visible}");

    app.show_reasoning_usage = false;
    let hidden = format_status_tokens(&app);
    assert!(!hidden.contains("reasoning=3"), "{hidden}");
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
        status.contains("cost=- tok=-/- ctx=0 pins=0 compact=0 tools=0 budget=ok"),
        "{status}"
    );
    assert!(
        !status.contains("cfg="),
        "compact status should stay calm: {status}"
    );
}

#[test]
fn status_line_surfaces_job_counts_and_latest_notification() {
    let mut app = test_app(SessionMode::Build);
    app.jobs.insert(1, test_job(1, JobStatus::Running));
    app.jobs.insert(2, test_job(2, JobStatus::Completed));
    app.notifications.push_back(JobNotification {
        job_id: 2,
        kind: JobKind::Shell,
        status: JobStatus::Completed,
        title: "shell".to_string(),
        summary: "shell Success".to_string(),
        ts_unix_ms: 42,
    });

    let status = format_status_tokens(&app);
    assert!(status.contains("jobs=1/2"), "{status}");
    assert!(status.contains("note=job2:completed"), "{status}");
    assert!(status.contains("/jobs"), "{status}");
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
        reasoning_output_tokens: None,
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
        status.contains("cost=$0.000042 tok=10/5 ctx=0 pins=0 compact=0 tools=2 budget=denied:1"),
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

    let output = render_to_string(&app, 140, 14);
    assert!(output.contains("openai:gpt-test"), "{output}");
    assert!(output.contains("repo=feature"), "{output}");
    assert!(output.contains("Ctrl-E collapse"), "{output}");
}

#[test]
fn task_panel_renders_progress_blocker_next_action_and_verification() {
    let mut app = test_app(SessionMode::Build);
    app.task_state = Some(sample_task_state());

    let output = render_to_string(&app, 120, 24);
    assert!(output.contains("Task"), "{output}");
    assert!(output.contains("Implement task UX"), "{output}");
    assert!(output.contains("[completed] Inspect TUI"), "{output}");
    assert!(output.contains("[active] Wire task panel"), "{output}");
    assert!(output.contains("Blocker: approval pending"), "{output}");
    assert!(output.contains("Next: run focused tests"), "{output}");
    assert!(output.contains("Verification: running"), "{output}");
    assert!(
        output.contains("Replan: status footer is too compact"),
        "{output}"
    );
}

#[tokio::test]
async fn ctrl_p_collapses_and_expands_task_panel() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.task_state = Some(sample_task_state());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
    )
    .await
    .expect("collapse task panel");
    assert!(app.task_panel_collapsed);
    let collapsed = render_to_string(&app, 120, 16);
    assert!(collapsed.contains("Task (collapsed)"), "{collapsed}");
    assert!(collapsed.contains("active=Wire task panel"), "{collapsed}");

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
    )
    .await
    .expect("expand task panel");
    assert!(!app.task_panel_collapsed);
}

#[tokio::test]
async fn esc_cancels_active_turn_but_quits_when_idle() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    app.turn_rx = Some(rx);
    app.cancel = Some(cancel.clone());

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("active esc");
    assert!(!quit);
    assert!(cancel.is_cancelled());
    assert_eq!(app.status, "cancelling");

    app.turn_rx = None;
    app.cancel = None;
    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("idle esc");
    assert!(quit);
}

#[tokio::test]
async fn ctrl_y_copies_last_assistant_message() {
    let mut agent = test_agent(SessionMode::Build);
    let writes = Arc::new(StdMutex::new(Vec::new()));
    let mut app = test_app_with_clipboard(
        SessionMode::Build,
        Box::new(RecordingClipboard {
            writes: writes.clone(),
            error: None,
        }),
    );
    app.push_transcript_item(TranscriptItem::user("hello"));
    app.push_transcript_item(TranscriptItem::assistant("answer"));

    handle_key(
        &mut app,
        &mut agent,
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
    let mut agent = test_agent(SessionMode::Build);
    let writes = Arc::new(StdMutex::new(Vec::new()));
    let mut app = test_app_with_clipboard(
        SessionMode::Build,
        Box::new(RecordingClipboard {
            writes: writes.clone(),
            error: None,
        }),
    );
    app.push_transcript_item(TranscriptItem::user("hello"));
    app.push_transcript_item(TranscriptItem::assistant("answer"));

    assert!(handle_slash_command(&mut app, &mut agent, "/copy transcript").await);
    assert_eq!(
        writes.lock().unwrap().as_slice(),
        ["user: hello\nassistant: answer"]
    );
    assert!(app.status.contains("copied transcript"), "{}", app.status);
}

#[tokio::test]
async fn slash_pin_pins_and_unpins_transcript_context() {
    let root = temp_workspace("pin_context");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("keep this decision"));

    assert!(handle_slash_command(&mut app, &mut agent, "/pin last").await);
    assert_eq!(app.context_compaction.pinned.len(), 1);
    let pin_id = app.context_compaction.pinned[0].id.clone();
    assert_eq!(app.status, format!("pinned {pin_id}"));

    assert!(handle_slash_command(&mut app, &mut agent, "/pins").await);
    assert!(app.status.contains("1 pinned"), "{}", app.status);
    assert!(
        last_message_content(&app).is_some_and(|content| content.contains("keep this decision")),
        "pins transcript should include pinned summary"
    );

    assert!(handle_slash_command(&mut app, &mut agent, &format!("/unpin {pin_id}")).await);
    assert!(app.context_compaction.pinned.is_empty());
    assert_eq!(app.status, format!("unpinned {pin_id}"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn slash_feedback_previews_redacted_message_before_send() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(
        handle_slash_command(
            &mut app,
            &mut agent,
            "/feedback OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz123456 broke"
        )
        .await
    );

    assert_eq!(app.status, "feedback preview ready");
    assert!(app.pending_feedback.is_some());
    let TranscriptEntryKind::Message(item) = &app.transcript.last().expect("preview").kind else {
        panic!("feedback preview should be a message entry");
    };
    let preview = item.content.clone();
    assert!(preview.contains("feedback preview"), "{preview}");
    assert!(preview.contains("<redacted:"), "{preview}");
    assert!(!preview.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
    assert!(preview.contains("/feedback send"));
}

#[tokio::test]
async fn slash_jobs_lists_and_shows_jobs() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let job = agent.start_local_tool_job(ToolCall {
        call_id: "test-checkpoints".to_string(),
        name: "checkpoint_list".to_string(),
        arguments: serde_json::json!({}),
    });
    app.jobs.insert(job.id, job.clone());

    assert!(handle_slash_command(&mut app, &mut agent, "/jobs").await);
    assert_eq!(app.status, "1 jobs");
    assert!(
        last_message_content(&app).is_some_and(|content| content.contains("checkpoint_list")),
        "expected jobs list to include checkpoint_list"
    );

    assert!(handle_slash_command(&mut app, &mut agent, &format!("/job {}", job.id)).await);
    assert!(app.status.starts_with(&format!("job {} ", job.id)));
    let detail = last_message_content(&app).unwrap_or_default().to_string();
    assert!(
        detail.contains("output_handle=-"),
        "expected job detail to include output handle placeholder: {detail}"
    );
    assert!(
        detail.contains("tool=checkpoint_list"),
        "expected job detail to include tool name: {detail}"
    );
    assert!(
        detail.contains("call_id=test-checkpoints"),
        "expected job detail to include call_id: {detail}"
    );
}

#[tokio::test]
async fn slash_job_cancel_cancels_active_job() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let job = agent.start_local_tool_job(ToolCall {
        call_id: "test-cancel".to_string(),
        name: "checkpoint_list".to_string(),
        arguments: serde_json::json!({}),
    });
    app.jobs.insert(job.id, job.clone());

    assert!(handle_slash_command(&mut app, &mut agent, &format!("/job-cancel {}", job.id)).await);
    assert!(
        app.status.starts_with("cancelling job ")
            || app.status.starts_with(&format!("job {} ", job.id)),
        "expected cancel acknowledgement, got {}",
        app.status
    );

    // A second cancel for the same id should report inactive once the job has settled.
    let max_attempts = 50;
    let mut saw_inactive = false;
    for _ in 0..max_attempts {
        assert!(
            handle_slash_command(&mut app, &mut agent, &format!("/job-cancel {}", job.id)).await
        );
        if app.status == format!("job {} not active", job.id) {
            saw_inactive = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        saw_inactive,
        "job never reported as inactive: {}",
        app.status
    );
}

#[tokio::test]
async fn slash_job_cancel_rejects_non_numeric_id() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/job-cancel abc").await);
    assert_eq!(app.status, "job id must be a number");

    assert!(handle_slash_command(&mut app, &mut agent, "/job-cancel").await);
    assert_eq!(app.status, "usage: /job-cancel <job_id>");
}

#[tokio::test]
async fn slash_checkpoint_starts_local_job_instead_of_blocking() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/checkpoints").await);
    assert!(app.status.starts_with("started job "), "{}", app.status);
    assert_eq!(app.jobs.len(), 1);
}

#[tokio::test]
async fn copy_failure_is_actionable_status() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app_with_clipboard(
        SessionMode::Build,
        Box::new(RecordingClipboard {
            writes: Arc::new(StdMutex::new(Vec::new())),
            error: Some("clipboard unavailable".to_string()),
        }),
    );
    app.push_transcript_item(TranscriptItem::assistant("answer"));

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");

    assert_eq!(app.status, "copy failed: clipboard unavailable");
}

#[tokio::test]
async fn transcript_navigation_keys_update_scroll_state() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.transcript_scroll_from_bottom, 8);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.transcript_scroll_from_bottom, 0);

    handle_key(
        &mut app,
        &mut agent,
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
async fn job_events_update_state_without_resetting_turn() {
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::JobUpdated {
        job: test_job(9, JobStatus::Running),
    })
    .await
    .expect("send job update");
    tx.send(AgentEvent::JobNotification {
        notification: JobNotification {
            job_id: 9,
            kind: JobKind::Shell,
            status: JobStatus::Completed,
            title: "shell".to_string(),
            summary: "shell Success".to_string(),
            ts_unix_ms: 42,
        },
    })
    .await
    .expect("send notification");
    drop(tx);

    drain_agent_events(&mut app).await;

    assert_eq!(app.jobs[&9].status, JobStatus::Running);
    assert_eq!(app.notifications.len(), 1);
    assert_eq!(app.status, "job 9 completed: shell Success");
    assert!(app.turn_rx.is_some());
}

#[tokio::test]
async fn scroll_keys_preserve_status_text() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.status = "tool foo finished".to_string();

    handle_key(
        &mut app,
        &mut agent,
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
        &mut agent,
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
        &mut agent,
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

fn test_app_with_config(config: &AppConfig, mode: SessionMode) -> TuiApp {
    TuiApp::new_with_clipboard("scripted", config, mode, None, Box::new(NoopClipboard))
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

fn test_config_with_root(mode: SessionMode, root: PathBuf) -> AppConfig {
    AppConfig {
        workspace_root: root,
        ..test_config(mode)
    }
}

fn sample_task_state() -> TaskStateSnapshot {
    TaskStateSnapshot {
        task: "Implement task UX".to_string(),
        status: TaskStateStatus::Blocked,
        summary: Some("Task panel is live".to_string()),
        steps: vec![
            TaskStateStep {
                title: "Inspect TUI".to_string(),
                status: TaskStepStatus::Completed,
                detail: None,
            },
            TaskStateStep {
                title: "Wire task panel".to_string(),
                status: TaskStepStatus::Active,
                detail: Some("render workflow state".to_string()),
            },
        ],
        blocker: Some("approval pending".to_string()),
        next_action: Some("run focused tests".to_string()),
        verification: TaskVerificationState::Running,
        recent_changes: vec!["added state model".to_string()],
        replan_reason: Some("status footer is too compact".to_string()),
    }
}

fn test_job(id: JobId, status: JobStatus) -> JobSnapshot {
    JobSnapshot {
        id,
        kind: JobKind::Shell,
        status,
        title: "shell description=\"run tests\"".to_string(),
        progress: None,
        result_summary: None,
        output_handle: None,
        turn_id: Some(TurnId::new(1)),
        tool_name: Some("shell".to_string()),
        call_id: Some(format!("call_{id}")),
        created_at_ms: 1,
        updated_at_ms: 1,
        ended_at_ms: None,
    }
}

fn last_message_content(app: &TuiApp) -> Option<&str> {
    match &app.transcript.last()?.kind {
        TranscriptEntryKind::Message(item) => Some(item.content.as_str()),
        _ => None,
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
    test_agent_with_config(AppConfig {
        session_mode: mode,
        ..Default::default()
    })
}

fn test_agent_with_config(config: AppConfig) -> Agent {
    Agent::new(
        config,
        Arc::new(UnavailableProvider::new("scripted", "test provider")),
    )
}

fn temp_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy_tui_{name}_{nonce}"));
    fs::create_dir_all(&root).expect("create temp workspace");
    root
}

fn sample_tool_result(name: &str, output: &str) -> ToolResult {
    ToolResult {
        call_id: "call-1".to_string(),
        tool_name: name.to_string(),
        status: ToolStatus::Success,
        content: serde_json::json!({ "output": output }),
        cost_hint: ToolCostHint {
            output_bytes: output.len() as u64,
            ..ToolCostHint::default()
        },
        receipt: ToolReceipt {
            output_sha256: "abcdef1234567890".to_string(),
            content_sha256: Some("0123456789abcdef".to_string()),
        },
        spill_model_output: None,
    }
}
