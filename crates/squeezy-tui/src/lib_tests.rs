use std::collections::BTreeMap;
use std::sync::Arc;

use squeezy_core::{
    AppConfig, PermissionCapability, PermissionRequest, PermissionRisk, PermissionScope,
    SessionMode,
};
use squeezy_llm::UnavailableProvider;

use super::*;

#[test]
fn app_starts_ready_with_empty_transcript() {
    let app = TuiApp::new(
        "openai",
        "gpt-test".to_string(),
        "defaults".to_string(),
        SessionMode::Build,
        None,
    );

    assert_eq!(app.provider_name, "openai");
    assert_eq!(app.model, "gpt-test");
    assert_eq!(app.mode, SessionMode::Build);
    assert_eq!(app.status, "ready");
    assert!(app.transcript.is_empty());
}

#[test]
fn app_surfaces_onboarding_summary_once() {
    let app = TuiApp::new(
        "openai",
        "gpt-test".to_string(),
        "defaults".to_string(),
        SessionMode::Build,
        Some("repo profile created: /tmp/project".to_string()),
    );

    assert_eq!(app.status, "repo profile ready");
    assert_eq!(app.transcript.len(), 1);
    assert_eq!(
        app.transcript[0].content,
        "repo profile created: /tmp/project"
    );
}

#[test]
fn status_line_surfaces_current_mode_and_switch_hints() {
    let mut app = TuiApp::new(
        "openai",
        "gpt-test".to_string(),
        "defaults".to_string(),
        SessionMode::Plan,
        None,
    );
    app.status = "ready".to_string();

    let status = format_status_tokens(&app);
    assert!(status.contains("mode=plan"), "missing mode: {status}");
    assert!(
        status.contains("Shift-Tab mode"),
        "missing toggle hint: {status}",
    );
    assert!(
        status.contains("/plan /build"),
        "missing commands: {status}"
    );
}

#[tokio::test]
async fn shift_tab_toggles_mode() {
    let agent = test_agent(SessionMode::Build);
    let mut app = TuiApp::new(
        "scripted",
        "gpt-test".to_string(),
        "defaults".to_string(),
        SessionMode::Build,
        None,
    );

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
    let mut app = TuiApp::new(
        "scripted",
        "gpt-test".to_string(),
        "defaults".to_string(),
        SessionMode::Build,
        None,
    );

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
    let mut app = TuiApp::new(
        "scripted",
        "gpt-test".to_string(),
        "defaults".to_string(),
        SessionMode::Build,
        None,
    );
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

fn test_agent(mode: SessionMode) -> Agent {
    Agent::new(
        AppConfig {
            session_mode: mode,
            ..Default::default()
        },
        Arc::new(UnavailableProvider::new("scripted", "test provider")),
    )
}
