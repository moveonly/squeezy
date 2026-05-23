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
    );

    assert_eq!(app.provider_name, "openai");
    assert_eq!(app.model, "gpt-test");
    assert_eq!(app.mode, SessionMode::Build);
    assert_eq!(app.status, "ready");
    assert!(app.transcript.is_empty());
}

#[test]
fn status_line_surfaces_current_mode_and_switch_hints() {
    let mut app = TuiApp::new(
        "openai",
        "gpt-test".to_string(),
        "defaults".to_string(),
        SessionMode::Plan,
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
        capability: PermissionCapability::Shell,
        target: "cargo test:*".to_string(),
        risk: PermissionRisk::High,
        summary: "shell description=\"run tests\" command=\"cargo test\"".to_string(),
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

    let prompt = format_approval_prompt(&request);
    assert!(prompt.contains("risk=high"), "missing risk: {prompt}");
    assert!(
        prompt.contains("target=cargo test:*"),
        "missing target: {prompt}",
    );
    assert!(prompt.contains("y once"));
    assert!(prompt.contains("a user allow"));
    assert!(prompt.contains("p project allow"));
    assert!(prompt.contains("u user deny"));
    assert!(prompt.contains("d project deny"));
    assert!(prompt.contains("n deny once"));
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
