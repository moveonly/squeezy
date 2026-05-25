use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::backend::TestBackend;
use squeezy_agent::{JobKind, JobStatus};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextAttachmentKind, ContextAttachmentSource,
    ContextAttachmentStatus, CostSnapshot, PermissionCapability, PermissionMode, PermissionPolicy,
    PermissionRequest, PermissionRisk, PermissionScope, SessionMode, StatusVerbosity,
    TaskStateSnapshot, TaskStateStatus, TaskStateStep, TaskStepStatus, TaskVerificationState,
    ToolOutputVerbosity, TuiAlternateScreen, TuiConfig, TurnId, TurnMetrics,
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
fn app_does_not_seed_onboarding_summary_into_fresh_transcript() {
    let config = test_config(SessionMode::Build);
    let app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        Some("repo profile created: /tmp/project".to_string()),
        Box::new(NoopClipboard),
    );

    assert_eq!(app.status, "ready");
    assert!(app.transcript.is_empty());
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
    assert!(
        status.contains("Plan mode (Shift+Tab to cycle)"),
        "missing mode: {status}"
    );
    assert!(
        status.contains("Ctrl+J newline"),
        "missing toggle hint: {status}",
    );
    assert!(
        status.contains("Up/Down menu/history"),
        "missing menu/history hint: {status}"
    );
    assert!(
        !status.contains("Wheel/PgUp/PgDn scroll"),
        "default inline mode should leave wheel scrolling to the terminal: {status}"
    );
}

#[test]
fn status_mode_color_distinguishes_build_and_plan() {
    let mut app = test_app(SessionMode::Build);

    let build = format_status_overview_line(&app, 120);
    assert_eq!(
        build.spans.last().and_then(|span| span.style.fg),
        Some(MODE_BUILD_GREEN)
    );

    app.mode = SessionMode::Plan;
    let plan = format_status_overview_line(&app, 120);
    assert_eq!(
        plan.spans.last().and_then(|span| span.style.fg),
        Some(MODE_PURPLE)
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

    set_input(&mut app, "/plan".to_string());
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

    set_input(&mut app, "/plan".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Plan);
    assert_eq!(app.status, "already in plan mode");

    set_input(&mut app, "/build".to_string());
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
async fn prompt_cursor_moves_and_edits_inside_text() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    set_input(&mut app, "abc".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
    )
    .await
    .expect("left");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
    )
    .await
    .expect("left");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT),
    )
    .await
    .expect("insert");
    assert_eq!(app.input, "aXbc");

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
    )
    .await
    .expect("right");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    )
    .await
    .expect("insert");
    assert_eq!(app.input, "aXbYc");

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    )
    .await
    .expect("backspace");
    assert_eq!(app.input, "aXbc");

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
    )
    .await
    .expect("delete");
    assert_eq!(app.input, "aXb");
}

#[tokio::test]
async fn prompt_cursor_moves_on_unicode_boundaries() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    set_input(&mut app, "aéz".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
    )
    .await
    .expect("left before z");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
    )
    .await
    .expect("left before unicode");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    )
    .await
    .expect("delete previous ascii");

    assert_eq!(app.input, "éz");
    assert_eq!(app.input_cursor, 0);
}

#[tokio::test]
async fn prompt_home_end_move_cursor_when_prompt_has_text() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "abc".to_string());
    app.transcript_scroll_from_bottom = 4;

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
    )
    .await
    .expect("home");
    assert_eq!(app.input_cursor, 0);
    assert_eq!(app.transcript_scroll_from_bottom, 4);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
    )
    .await
    .expect("end");
    assert_eq!(app.input_cursor, app.input.len());
    assert_eq!(app.transcript_scroll_from_bottom, 4);
}

#[tokio::test]
async fn prompt_history_uses_plain_up_down_when_prompt_is_empty() {
    let mut agent = test_agent(SessionMode::Build);
    let mut config = test_config(SessionMode::Build);
    config.tui.alternate_screen = TuiAlternateScreen::Never;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    push_input_history(&mut app, "first prompt".to_string());
    push_input_history(&mut app, "second prompt".to_string());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    )
    .await
    .expect("history up");
    assert_eq!(app.input, "second prompt");
    assert!(app.selected_entry.is_none());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    )
    .await
    .expect("history up");
    assert_eq!(app.input, "first prompt");

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("history down");
    assert_eq!(app.input, "second prompt");

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("history down");
    assert!(app.input.is_empty());
}

#[tokio::test]
async fn alternate_screen_arrows_recall_history_when_prompt_is_empty() {
    let mut agent = test_agent(SessionMode::Build);
    let mut config = test_config(SessionMode::Build);
    config.tui.alternate_screen = TuiAlternateScreen::Always;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("first turn".to_string()));
    push_input_history(&mut app, "first prompt".to_string());
    push_input_history(&mut app, "second prompt".to_string());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    )
    .await
    .expect("history up");

    assert_eq!(app.input, "second prompt");
    assert_eq!(app.transcript_scroll_from_bottom, 0);
}

#[tokio::test]
async fn alternate_screen_arrows_scroll_transcript_when_draft_is_not_empty() {
    let mut agent = test_agent(SessionMode::Build);
    let mut config = test_config(SessionMode::Build);
    config.tui.alternate_screen = TuiAlternateScreen::Always;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    set_input(&mut app, "hi".to_string());
    app.push_transcript_item(TranscriptItem::user("first turn".to_string()));
    push_input_history(&mut app, "previous prompt".to_string());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    )
    .await
    .expect("scroll up");

    assert_eq!(app.input, "hi");
    assert_eq!(app.transcript_scroll_from_bottom, 3);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("scroll down");

    assert_eq!(app.input, "hi");
    assert_eq!(app.transcript_scroll_from_bottom, 0);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
    )
    .await
    .expect("history up keeps draft");

    assert_eq!(app.input, "hi");
}

#[tokio::test]
async fn alternate_screen_arrows_keep_scrolling_when_transcript_is_already_scrolled() {
    let mut agent = test_agent(SessionMode::Build);
    let mut config = test_config(SessionMode::Build);
    config.tui.alternate_screen = TuiAlternateScreen::Always;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("first turn".to_string()));
    push_input_history(&mut app, "previous prompt".to_string());
    app.transcript_scroll_from_bottom = 3;

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("scroll down");

    assert!(app.input.is_empty());
    assert_eq!(app.transcript_scroll_from_bottom, 0);
}

#[test]
fn default_mouse_wheel_does_not_touch_prompt_history_or_transcript_scroll() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("first turn".to_string()));
    push_input_history(&mut app, "previous prompt".to_string());

    handle_mouse(&mut app, MouseEventKind::ScrollUp);

    assert_eq!(app.transcript_scroll_from_bottom, 0);
    assert!(app.input.is_empty());
}

#[test]
fn explicit_alternate_screen_mouse_wheel_scrolls_transcript_without_prompt_history() {
    let mut config = test_config(SessionMode::Build);
    config.tui.alternate_screen = TuiAlternateScreen::Always;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("first turn".to_string()));
    push_input_history(&mut app, "previous prompt".to_string());

    handle_mouse(&mut app, MouseEventKind::ScrollUp);

    assert_eq!(app.transcript_scroll_from_bottom, 3);
    assert!(app.input.is_empty());

    handle_mouse(&mut app, MouseEventKind::ScrollDown);

    assert_eq!(app.transcript_scroll_from_bottom, 0);
    assert!(app.input.is_empty());
}

#[tokio::test]
async fn slash_menu_renders_and_completes_selected_command() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/p".to_string());

    let output = render_to_string(&app, 100, 16);
    assert!(output.contains("/permissions"), "{output}");
    assert!(output.contains("/plan"), "{output}");

    for _ in 0..3 {
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        )
        .await
        .expect("menu down");
    }
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("complete command");
    assert_eq!(app.input, "/plan ");
    assert_eq!(app.status, "selected /plan");
}

#[tokio::test]
async fn slash_menu_scrolls_sorted_full_command_list_with_five_visible() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/".to_string());

    let suggestions = slash_suggestions(&app.input);
    let names = suggestions
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    assert!(names.len() > SLASH_MENU_MAX_ITEMS);
    assert_eq!(
        &names[..SLASH_MENU_MAX_ITEMS],
        [
            "/attach",
            "/attachments",
            "/build",
            "/checkpoint",
            "/checkpoints"
        ]
    );
    assert_eq!(slash_suggestion_lines(&app).len(), SLASH_MENU_MAX_ITEMS);

    for _ in 0..5 {
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        )
        .await
        .expect("menu down");
    }

    let visible = visible_slash_suggestions(&suggestions, app.slash_menu_index)
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    assert_eq!(
        visible,
        vec![
            "/attachments",
            "/build",
            "/checkpoint",
            "/checkpoints",
            "/collapse"
        ]
    );

    for _ in 0..100 {
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        )
        .await
        .expect("menu down");
    }
    assert_eq!(app.slash_menu_index, suggestions.len() - 1);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("menu down at end");
    assert_eq!(app.slash_menu_index, suggestions.len() - 1);
}

#[test]
fn unknown_slash_command_does_not_start_model_turn() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/nope".to_string());

    let input = app.input.trim().to_string();
    assert!(reject_unknown_slash_command(&mut app, &input));

    assert_eq!(app.input, "/nope");
    assert!(app.turn_rx.is_none());
    assert!(app.status.contains("unknown command"), "{}", app.status);
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
        output.contains("provider=scripted model=gpt-5.5"),
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
async fn slash_help_lists_topics() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/help").await);

    assert_eq!(app.status, "help index");
    let content = last_message_content(&app).expect("help transcript");
    assert!(content.contains("Supported topics"), "{content}");
    assert!(content.contains("`providers`"), "{content}");
}

#[tokio::test]
async fn slash_help_config_renders_citations_and_config() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/help providers").await);

    assert_eq!(app.status, "help providers");
    let content = last_message_content(&app).expect("help transcript");
    assert!(content.contains("docs/external/PROVIDERS.md"), "{content}");
    assert!(content.contains("[model]"), "{content}");
    assert!(!content.contains("--api-key"), "{content}");
}

#[tokio::test]
async fn slash_help_unsupported_points_to_public_resources() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/help quantum billing").await);

    assert_eq!(app.status, "help topic not covered locally");
    let content = last_message_content(&app).expect("help transcript");
    assert!(content.contains("won't guess"), "{content}");
    assert!(
        content.contains("https://squeezyagent.com/docs/"),
        "{content}"
    );
    assert!(
        content.contains("https://github.com/esqueezy/squeezy"),
        "{content}"
    );
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

    assert_eq!(text, "> hello");
}

#[test]
fn tool_result_entries_collapse_by_default_and_expand_when_toggled() {
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));

    assert!(app.transcript[0].collapsed);
    let collapsed = render_to_string(&app, 100, 12);
    assert!(collapsed.contains("✔ Explored"), "{collapsed}");
    assert!(collapsed.contains("grep"), "{collapsed}");
    assert!(!collapsed.contains("receipt="), "{collapsed}");
    assert!(!collapsed.contains("B receipt"), "{collapsed}");
    assert!(
        !collapsed.contains("needle found"),
        "collapsed view should hide payload: {collapsed}"
    );

    select_previous_transcript_entry(&mut app);
    toggle_selected_transcript_entry(&mut app);

    assert!(!app.transcript[0].collapsed);
    let expanded = render_to_string(&app, 100, 18);
    assert!(expanded.contains("needle found"), "{expanded}");
    assert!(!expanded.contains("receipt output="), "{expanded}");
}

#[test]
fn ctrl_e_without_selection_toggles_latest_transcript_entry() {
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));

    assert!(app.selected_entry.is_none());
    assert!(app.transcript[0].collapsed);

    toggle_selected_transcript_entry(&mut app);
    assert!(!app.transcript[0].collapsed);
    assert_eq!(app.status, "expanded transcript entry 1");

    toggle_selected_transcript_entry(&mut app);
    assert!(app.transcript[0].collapsed);
    assert_eq!(app.status, "collapsed transcript entry 1");
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
fn failed_tool_rows_show_actionable_error_detail() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("shell", "");
    result.status = ToolStatus::Error;
    result.content = serde_json::json!({
        "command": "cargo build --workspace",
        "exit_code": 101,
        "stdout": "",
        "stderr": "",
        "error": null,
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 120, 12);

    assert!(
        output.contains("✖ Failed cargo build --workspace · exit 101"),
        "{output}"
    );
    assert!(!output.contains("shell · error"), "{output}");
}

#[test]
fn missing_cargo_manifest_shell_failure_renders_as_not_run_warning() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("shell", "");
    result.status = ToolStatus::Error;
    result.content = serde_json::json!({
        "command": "cargo check -p sonar-arch-graph",
        "exit_code": 101,
        "stdout": "",
        "stderr": "error: could not find `Cargo.toml` in `/Users/abbassabra/semsitter` or any parent directory",
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 140, 12);

    assert!(
        output.contains("⚠ Not run cargo check -p sonar-arch-graph · no Cargo.toml found"),
        "{output}"
    );
    assert!(!output.contains("✖ Failed cargo check"), "{output}");
}

#[test]
fn shell_tool_rows_show_command_and_highlight_output() {
    let mut app = test_app(SessionMode::Build);
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "cargo test -p squeezy-tui"}),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "cargo test -p squeezy-tui",
        "workdir": ".",
        "exit_code": 0,
        "stdout": "\u{1b}[32mok\u{1b}[0m",
        "stderr": "",
    });
    app.push_tool_result_with_call(result, Some(call));
    select_previous_transcript_entry(&mut app);
    toggle_selected_transcript_entry(&mut app);

    let output = render_to_string(&app, 140, 18);

    assert!(
        output.contains("✔ Ran cargo test -p squeezy-tui"),
        "{output}"
    );
    assert!(
        output.contains("└ cargo test -p squeezy-tui in .:"),
        "{output}"
    );
    assert!(
        !output.contains("• cargo test -p squeezy-tui in .:"),
        "{output}"
    );
    assert!(output.contains("ok"), "{output}");
    assert!(!output.contains("\\u001b"), "{output}");
}

#[test]
fn read_only_shell_rows_render_codex_style_output_block() {
    let mut app = test_app(SessionMode::Build);
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "inspect workspace --details"}),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "inspect workspace --details",
        "workdir": "/tmp/project",
        "exit_code": 0,
        "stdout": "total 8\ndrwxr-xr-x  3 user  staff  96 .\n-rw-r--r--  1 user  staff  10 README.md",
        "stderr": "",
        "policy": {
            "capability": "search"
        }
    });
    app.push_tool_result_with_call(result, Some(call));

    let output = render_to_string(&app, 140, 18);

    assert!(
        output.contains("✔ Explored inspect workspace --details"),
        "{output}"
    );
    assert!(
        output.contains("└ inspect workspace --details in /tmp/project:"),
        "{output}"
    );
    assert!(
        !output.contains("• inspect workspace --details in /tmp/project:"),
        "{output}"
    );
    assert!(output.contains("total 8"), "{output}");
    assert!(output.contains("README.md"), "{output}");
    assert!(!output.contains("stdout"), "{output}");
}

#[test]
fn decl_search_row_summarizes_count_query_without_raw_json() {
    let mut app = test_app(SessionMode::Build);
    let call = ToolCall {
        call_id: "decl-1".to_string(),
        name: "decl_search".to_string(),
        arguments: serde_json::json!({"language": "Java", "kind": "callable"}),
    };
    let mut result = sample_tool_result("decl_search", "");
    result.call_id = "decl-1".to_string();
    result.content = serde_json::json!({
        "query": null,
        "language": "Java",
        "kind": "callable",
        "total_matches": 42,
        "returned_matches": 10,
        "counts_by_language": {"Java": 42},
        "counts_by_kind": {"method": 42},
        "packets": [],
        "truncated": true
    });
    app.push_tool_result_with_call(result, Some(call));

    let output = render_to_string(&app, 140, 14);

    assert!(
        output.contains("✔ Explored Java callable declarations · 42 matches"),
        "{output}"
    );
    assert!(!output.contains("receipt"), "{output}");
    assert!(!output.contains("\"packets\""), "{output}");
}

#[test]
fn tool_rows_summarize_diff_glob_read_and_plan_outputs() {
    let mut app = test_app(SessionMode::Build);

    let mut diff = sample_tool_result("diff_context", "");
    diff.call_id = "diff-1".to_string();
    diff.content = serde_json::json!({
        "mode": "worktree",
        "summary": {"files_changed": 2, "additions": 3, "deletions": 1},
        "files": [{"path": "src/lib.rs"}, {"path": "tests/ui.rs"}],
        "truncated": false,
    });
    app.push_tool_result_with_call(
        diff,
        Some(ToolCall {
            call_id: "diff-1".to_string(),
            name: "diff_context".to_string(),
            arguments: serde_json::json!({}),
        }),
    );

    let mut glob = sample_tool_result("glob", "");
    glob.call_id = "glob-1".to_string();
    glob.content = serde_json::json!({
        "paths": ["src/lib.rs", "src/main.rs"],
        "metadata": {"pattern": "**/*.rs", "path": "."},
    });
    app.push_tool_result_with_call(
        glob,
        Some(ToolCall {
            call_id: "glob-1".to_string(),
            name: "glob".to_string(),
            arguments: serde_json::json!({"pattern": "**/*.rs"}),
        }),
    );

    let mut read = sample_tool_result("read_file", "");
    read.call_id = "read-1".to_string();
    read.content = serde_json::json!({
        "path": "src/lib.rs",
        "bytes_returned": 128,
        "total_bytes": 1024,
        "content": "fn main() {}",
        "truncated": true,
    });
    app.push_tool_result_with_call(
        read,
        Some(ToolCall {
            call_id: "read-1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        }),
    );

    let output = render_to_string(&app, 180, 18);

    assert!(
        output.contains("✔ Explored diff context (worktree) · 2 files · +3 -1"),
        "{output}"
    );
    assert!(
        output.contains("✔ Explored list files matching **/*.rs · 2 paths"),
        "{output}"
    );
    assert!(
        output.contains("✔ Explored read src/lib.rs · 128B · more available"),
        "{output}"
    );
    assert!(!output.contains("plan patch"), "{output}");
    assert!(!output.contains("output shortened"), "{output}");
    assert!(!output.contains("\"paths\""), "{output}");
}

#[test]
fn edit_tool_row_summarizes_checkpoint_diff_and_expands_patch() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("apply_patch", "");
    result.call_id = "patch-1".to_string();
    result.content = serde_json::json!({
        "checkpoint": {
            "files": [{
                "path": "src/lib.rs",
                "additions": 1,
                "deletions": 1,
                "patch": "@@ -1 +1 @@\n-old\n+new\n",
                "patch_truncated": false
            }]
        }
    });
    app.push_tool_result_with_call(
        result,
        Some(ToolCall {
            call_id: "patch-1".to_string(),
            name: "apply_patch".to_string(),
            arguments: serde_json::json!({}),
        }),
    );
    select_previous_transcript_entry(&mut app);
    toggle_selected_transcript_entry(&mut app);

    let output = render_to_string(&app, 120, 16);

    assert!(output.contains("✔ Edited src/lib.rs · +1 -1"), "{output}");
    assert!(output.contains("diff"), "{output}");
    assert!(output.contains("-old"), "{output}");
    assert!(output.contains("+new"), "{output}");
    assert!(!output.contains("\"checkpoint\""), "{output}");
}

#[test]
fn expanded_edit_diff_does_not_claim_ctrl_e_can_expand_further() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("apply_patch", "");
    result.call_id = "patch-1".to_string();
    let patch = (0..40)
        .map(|index| format!("+line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    result.content = serde_json::json!({
        "checkpoint": {
            "files": [{
                "path": "src/lib.rs",
                "additions": 40,
                "deletions": 0,
                "patch": patch,
                "patch_truncated": false
            }]
        }
    });
    app.push_tool_result_with_call(
        result,
        Some(ToolCall {
            call_id: "patch-1".to_string(),
            name: "apply_patch".to_string(),
            arguments: serde_json::json!({}),
        }),
    );
    toggle_selected_transcript_entry(&mut app);

    let lines = format_transcript_entry_with_width(
        &app.transcript[0],
        false,
        ToolOutputVerbosity::Normal,
        MessageOutcome::Normal,
        Some(120),
    );
    let output = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(output.contains("+line 20"), "{output}");
    assert!(!output.contains("Ctrl-E to expand"), "{output}");
}

#[test]
fn collapsed_edit_row_shows_diff_preview() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("apply_patch", "");
    result.content = serde_json::json!({
        "checkpoint": {
            "files": [{
                "path": "src/lib.rs",
                "additions": 1,
                "deletions": 1,
                "patch": "@@ -1 +1 @@\n-old\n+new\n",
                "patch_truncated": false
            }]
        }
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 120, 14);

    assert!(output.contains("✔ Edited src/lib.rs · +1 -1"), "{output}");
    assert!(output.contains("diff src/lib.rs"), "{output}");
    assert!(output.contains("-old"), "{output}");
    assert!(output.contains("+new"), "{output}");
}

#[test]
fn edit_diff_preview_uses_dedicated_diff_colors() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("apply_patch", "");
    result.content = serde_json::json!({
        "checkpoint": {
            "files": [{
                "path": "src/lib.rs",
                "additions": 1,
                "deletions": 1,
                "patch": "diff --git a/src/lib.rs b/src/lib.rs\nindex 123..456 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
                "patch_truncated": false
            }]
        }
    });
    app.push_tool_result(result);

    let lines = format_transcript_entry_with_width(
        &app.transcript[0],
        false,
        ToolOutputVerbosity::Normal,
        MessageOutcome::Normal,
        Some(120),
    );
    let rendered = lines_to_plain_text(&lines);
    assert!(!rendered.contains("diff --git"), "{rendered}");
    assert!(!rendered.contains("index 123"), "{rendered}");

    let add_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "+new")
        .expect("add span");
    assert_eq!(
        add_span.style.fg,
        Some(render::palette::best_color(
            render::palette::rgb_components(DIFF_ADD_FG,)
        ))
    );
    assert_eq!(add_span.style.bg, None);

    let del_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "-old")
        .expect("delete span");
    assert_eq!(
        del_span.style.fg,
        Some(render::palette::best_color(
            render::palette::rgb_components(DIFF_DEL_FG,)
        ))
    );
    assert_eq!(del_span.style.bg, None);
}

#[test]
fn retryable_apply_patch_failure_is_not_rendered_as_final_failure() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("apply_patch", "");
    result.status = ToolStatus::Stale;
    result.content = serde_json::json!({
        "error": "search text matched more than once; narrow the search text or set allow_multiple=true to replace all matches",
        "path": "src/lib.rs",
        "matches": 2
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 140, 12);

    assert!(output.contains("⚠ Retried src/lib.rs"), "{output}");
    assert!(!output.contains("✖ Failed apply patch"), "{output}");
}

#[test]
fn repeated_invalid_tool_arguments_are_coalesced() {
    let mut app = test_app(SessionMode::Build);
    for index in 0..2 {
        let mut result = sample_tool_result("decl_search", "");
        result.call_id = format!("decl-{index}");
        result.status = ToolStatus::Error;
        result.content = serde_json::json!({
            "error": "invalid tool arguments: missing field `query`"
        });
        app.push_tool_result(result);
    }

    let output = render_to_string(&app, 140, 14);

    assert!(output.contains("⚠ Retried decl_search"), "{output}");
    assert!(output.contains("missing field `query`"), "{output}");
    assert!(output.contains("(2x)"), "{output}");
    assert_eq!(app.transcript.len(), 1);
}

#[test]
fn command_and_output_highlighters_style_key_parts() {
    let command = command_spans("cargo test -p squeezy-tui");
    assert_eq!(command[0].style.fg, Some(GOLD));
    assert!(
        command
            .iter()
            .any(|span| span.content.as_ref() == "-p" && span.style.fg == Some(AMBER)),
        "{command:?}"
    );

    let ansi = ansi_spans("\u{1b}[32mok\u{1b}[0m error");
    assert_eq!(ansi[0].style.fg, Some(Color::Green));

    let keyword = keyword_spans("public class Foo { return ok; }");
    assert!(
        keyword
            .iter()
            .any(|span| span.content.as_ref() == "class" && span.style.fg == Some(GOLD)),
        "{keyword:?}"
    );
}

#[test]
fn ansi_passthrough_renders_colors() {
    let line = render::ansi::ansi_to_line("\u{1b}[32mhello\u{1b}[0m world");

    assert_eq!(line.spans[0].content.as_ref(), "hello");
    assert_eq!(line.spans[0].style.fg, Some(Color::Green));
}

#[test]
fn diff_render_colorizes_gutter() {
    let file = squeezy_vcs::DiffFile {
        path: "src/lib.rs".to_string(),
        status: squeezy_vcs::DiffFileStatus::Modified,
        code: "M".to_string(),
        additions: 1,
        deletions: 1,
        binary: false,
        hunks: vec![squeezy_vcs::DiffHunk {
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 2,
            start_line: 1,
            end_line: 4,
        }],
        patch: Some("@@ -1,2 +1,2 @@\n context\n-old\n+new\n".to_string()),
        patch_truncated: false,
    };

    let lines = render::diff::render_diff_file(&file);
    let add = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "+new")
        .expect("add span");
    let del = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "-old")
        .expect("delete span");

    assert_eq!(
        add.style.fg,
        Some(render::palette::best_color(
            render::palette::rgb_components(DIFF_ADD_FG,)
        ))
    );
    assert_eq!(
        del.style.fg,
        Some(render::palette::best_color(
            render::palette::rgb_components(DIFF_DEL_FG,)
        ))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.spans[0].content.as_ref() == "2 ")
    );
}

#[test]
fn highlight_rust_code_block() {
    let lines = render::highlight::highlight_code(Some("rust"), "fn foo() { /* comment */ 42 }");
    let spans = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .collect::<Vec<_>>();

    let keyword = spans
        .iter()
        .find(|span| span.content.as_ref() == "fn")
        .expect("keyword span");
    let comment = spans
        .iter()
        .find(|span| span.content.as_ref() == "/* comment */")
        .expect("comment span");
    let number = spans
        .iter()
        .find(|span| span.content.as_ref() == "42")
        .expect("number span");

    assert_eq!(keyword.style.fg, Some(render::highlight::KEYWORD_COLOR));
    assert_eq!(comment.style.fg, Some(render::highlight::COMMENT_COLOR));
    assert_eq!(number.style.fg, Some(render::highlight::NUMBER_COLOR));
}

#[test]
fn markdown_renders_heading_and_code() {
    let lines = render::markdown::render_markdown("# Heading\n\n```rust\nfn foo() {}\n```");
    let heading = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "Heading")
        .expect("heading span");
    let code_keyword = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "fn")
        .expect("code keyword span");

    assert!(heading.style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(
        code_keyword.style.fg,
        Some(render::highlight::KEYWORD_COLOR)
    );
}

#[test]
fn palette_returns_ansi16_when_unsupported() {
    assert_eq!(
        render::palette::best_color_for_level((255, 0, 0), render::palette::ColorLevel::Ansi16),
        Color::Red
    );
}

#[test]
fn failed_tool_rows_fall_back_to_no_output_when_empty() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("shell", "");
    result.status = ToolStatus::Error;
    result.content = serde_json::json!({
        "command": "cargo build --workspace",
        "exit_code": null,
        "stdout": "",
        "stderr": "",
        "error": null,
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 120, 12);

    assert!(
        output.contains("✖ Failed cargo build --workspace · no output"),
        "{output}"
    );
    assert!(!output.contains("shell · error"), "{output}");
}

#[test]
fn failed_tool_rows_show_missing_exit_status_reason() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("shell", "");
    result.status = ToolStatus::Error;
    result.content = serde_json::json!({
        "command": "cargo build --workspace",
        "exit_code": null,
        "signal": null,
        "stdout": "",
        "stderr": "",
        "error": "shell command ended without an exit code",
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 140, 18);

    assert!(
        output.contains(
            "✖ Failed cargo build --workspace · shell command ended without an exit code"
        ),
        "{output}"
    );
}

#[test]
fn denied_tool_rows_show_denial_reason() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("shell", "");
    result.status = ToolStatus::Denied;
    result.content = serde_json::json!({
        "reason": "required shell sandbox unavailable",
        "permission_denied": true,
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 120, 12);

    assert!(
        output.contains("⚠ Denied shell · required shell sandbox unavailable"),
        "{output}"
    );
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
    assert!(
        !visible.contains("reasoning=3"),
        "compact status hides accounting details: {visible}"
    );

    app.status_verbosity = StatusVerbosity::Verbose;
    let visible = format_status_tokens(&app);
    assert!(visible.contains("reasoning=3"), "{visible}");

    app.show_reasoning_usage = false;
    let hidden = format_status_tokens(&app);
    assert!(!hidden.contains("reasoning=3"), "{hidden}");
}

#[test]
fn approval_prompt_renders_actionable_menu_without_metadata_dump() {
    let request = sample_approval_request();

    let prompt = format_approval_prompt(&request);

    assert!(prompt.contains("Approval needed"), "{prompt}");
    assert!(prompt.contains("cargo test"), "{prompt}");
    assert!(prompt.contains("Approve"), "{prompt}");
    assert!(
        prompt.contains("Always approve this command in this repo"),
        "{prompt}"
    );
    assert!(prompt.contains("Deny"), "{prompt}");
    assert!(!prompt.contains("output_byte_cap"), "{prompt}");
    assert!(!prompt.contains("sandbox_network"), "{prompt}");
    assert!(!prompt.contains("env="), "{prompt}");
    assert!(!prompt.contains("reason="), "{prompt}");
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
    assert!(line.contains("approval needed"));
    assert!(line.contains("risk=high"));
    assert!(line.contains("target=shell:*"));
}

#[tokio::test]
async fn approval_menu_uses_arrows_and_enter_for_repo_rule() {
    let mut app = test_app(SessionMode::Build);
    let request = sample_approval_request();
    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
    app.pending_approval = Some(PendingApproval {
        request,
        decision_tx,
    });

    assert!(handle_approval_key(
        &mut app,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    ));
    assert_eq!(app.approval_selection_index, 1);
    assert!(handle_approval_key(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    ));

    assert_eq!(
        decision_rx.await.expect("approval decision"),
        ToolApprovalDecision::AllowRuleProject
    );
    assert!(app.pending_approval.is_none());
    assert!(app.status.contains("saved repo approval"), "{}", app.status);
}

#[test]
fn approval_menu_renders_below_prompt_without_border_box() {
    let mut app = test_app(SessionMode::Build);
    let request = sample_approval_request();
    let (decision_tx, _decision_rx) = tokio::sync::oneshot::channel();
    app.pending_approval = Some(PendingApproval {
        request,
        decision_tx,
    });
    set_input(&mut app, "approve?".to_string());

    let output = render_to_string(&app, 120, 24);
    let lines = output.lines().collect::<Vec<_>>();
    let prompt = lines
        .iter()
        .position(|line| line.contains("approve?┃"))
        .expect("prompt");
    let approval = lines
        .iter()
        .position(|line| line.contains("Approval needed"))
        .expect("approval menu");

    assert!(approval > prompt, "{output}");
    assert!(output.contains("› Approve"), "{output}");
    assert!(
        output.contains("Always approve this command in this repo"),
        "{output}"
    );
    assert!(!output.contains("Approval required"), "{output}");
    assert!(!output.contains('┌'), "{output}");
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
    assert!(
        status.contains("Build mode (Shift+Tab to cycle)"),
        "{status}"
    );
    assert!(status.contains("dir "), "{status}");
    assert!(status.contains("feature"), "{status}");
    assert!(!status.contains("feature*2"), "{status}");
    assert!(!status.contains("running search"), "{status}");
    assert!(!status.contains("openai:gpt-test"), "{status}");
    assert!(!status.contains("perm="), "{status}");
    assert!(!status.contains("sandbox"), "{status}");
    assert!(!status.contains("telemetry"), "{status}");
    assert!(!status.contains("cost"), "{status}");
    assert!(
        !status.contains("cfg="),
        "compact status should stay calm: {status}"
    );
}

#[test]
fn status_line_omits_job_counts_and_latest_notification() {
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
    assert!(!status.contains("jobs 1/2"), "{status}");
    assert!(!status.contains("job2 completed"), "{status}");
    assert!(
        status.contains("Build mode (Shift+Tab to cycle)"),
        "{status}"
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
    assert!(
        status.contains("Plan mode (Shift+Tab to cycle)"),
        "{status}"
    );
    assert!(status.contains("cost $0.000042"), "{status}");
    assert!(status.contains("tok 10/5"), "{status}");
    assert!(status.contains("tools 2"), "{status}");
    assert!(status.contains("budget denied:1"), "{status}");
    assert!(status.contains("cfg defaults"), "{status}");
    assert!(status.contains("read 1024B"), "{status}");
    assert!(status.contains("receipts 1"), "{status}");
    assert!(status.contains("redactions 4"), "{status}");
    assert!(status.contains("cached 7"), "{status}");
    assert!(status.contains("cache_write 3"), "{status}");
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

    let output = render_to_string(&app, 140, 18);
    assert!(output.contains(">_ Squeezy v"), "{output}");
    assert!(output.contains("openai:gpt-test"), "{output}");
    assert!(output.contains("dir "), "{output}");
    assert!(output.contains("feature"), "{output}");
    assert!(
        output.contains("Build mode (Shift+Tab to cycle)"),
        "{output}"
    );
    assert!(!output.contains("ready"), "{output}");
    assert!(output.contains("Up/Down menu/history"), "{output}");
}

#[test]
fn status_footer_sits_directly_below_prompt_area() {
    let app = test_app(SessionMode::Build);

    let output = render_to_string(&app, 100, 16);
    let lines = output.lines().collect::<Vec<_>>();
    let prompt_line = lines
        .iter()
        .position(|line| line.contains('┃'))
        .expect("prompt cursor");
    let status_line = lines
        .iter()
        .position(|line| line.contains("dir "))
        .expect("status line");
    let help_line = lines
        .iter()
        .position(|line| line.contains("Enter send"))
        .expect("help line");

    assert!(
        status_line > prompt_line && status_line <= prompt_line + PROMPT_MIN_HEIGHT as usize,
        "{output}"
    );
    assert_eq!(help_line, status_line + 1, "{output}");
}

#[test]
fn render_keeps_header_when_transcript_has_content() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("hello"));
    app.push_transcript_item(TranscriptItem::assistant("answer"));

    let output = render_to_string(&app, 120, 18);
    assert!(output.contains(">_ Squeezy v"), "{output}");
    assert!(output.contains("scripted:gpt-test"), "{output}");
    assert!(output.contains("> hello"), "{output}");
    assert!(output.contains("● answer"), "{output}");
    assert!(!output.contains("Answered"), "{output}");
}

#[test]
fn active_prompt_keeps_blank_space_after_last_answer() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("l"));
    app.push_transcript_item(TranscriptItem::assistant(
        "I am not sure what you want with l.",
    ));

    let output = render_to_string(&app, 100, 20);
    let lines = output.lines().collect::<Vec<_>>();
    let answer_line = lines
        .iter()
        .position(|line| line.contains("I am not sure"))
        .expect("answer line");
    let prompt_line = lines
        .iter()
        .position(|line| line.contains('┃'))
        .expect("prompt cursor");
    let blank_rows = lines[answer_line + 1..prompt_line]
        .iter()
        .filter(|line| line.trim().is_empty())
        .count();

    assert!(blank_rows >= 2, "{output}");
}

#[test]
fn startup_card_scrolls_with_transcript_history() {
    let mut app = test_app(SessionMode::Build);
    for index in 0..16 {
        app.push_transcript_item(TranscriptItem::user(format!("prompt {index}")));
        app.push_transcript_item(TranscriptItem::assistant(format!("answer {index}")));
    }

    let at_bottom = render_to_string(&app, 120, 20);
    assert!(!at_bottom.contains(">_ Squeezy v"), "{at_bottom}");

    app.transcript_scroll_from_bottom = u16::MAX;
    let at_top = render_to_string(&app, 120, 20);
    assert!(at_top.contains(">_ Squeezy v"), "{at_top}");
}

#[test]
fn compact_viewport_hides_attachment_panel_before_prompt_footer() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("modify one file in the codebase"));
    app.push_tool_result(sample_tool_result("diff_context", ""));
    app.attachments = vec![sample_attachment("att-0001"), sample_attachment("att-0002")];

    let output = render_to_string(&app, 120, 8);

    assert!(output.contains("diff context"), "{output}");
    assert!(output.contains('┃'), "{output}");
    assert!(output.contains("Enter send"), "{output}");
    assert!(!output.contains("att-0001"), "{output}");
}

#[test]
fn auto_mode_is_default_terminal_model() {
    let config = test_config(SessionMode::Build);

    assert_eq!(config.tui.alternate_screen, TuiAlternateScreen::Auto);
    assert_eq!(
        TerminalMode::from(config.tui.alternate_screen),
        TerminalMode::Inline
    );
    assert_eq!(
        TerminalMode::from(TuiAlternateScreen::Never),
        TerminalMode::Inline
    );
    assert_eq!(
        TerminalMode::from(TuiAlternateScreen::Always),
        TerminalMode::AlternateScreen
    );
}

#[test]
fn alternate_scroll_commands_use_xterm_private_mode() {
    let mut enable = String::new();
    EnableAlternateScroll
        .write_ansi(&mut enable)
        .expect("enable alternate scroll");
    assert_eq!(enable, "\x1b[?1007h");

    let mut disable = String::new();
    DisableAlternateScroll
        .write_ansi(&mut disable)
        .expect("disable alternate scroll");
    assert_eq!(disable, "\x1b[?1007l");
}

#[test]
fn inline_history_flush_contains_startup_and_new_transcript() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("find getFoo"));
    app.push_transcript_item(TranscriptItem::assistant("No definition found."));

    let first = inline_history_lines_for_flush(&app, 100, true, 0);
    let rendered = lines_to_plain_text(&first);

    assert!(rendered.contains(">_ Squeezy v0.1.0"), "{rendered}");
    assert!(rendered.contains("> find getFoo"), "{rendered}");
    assert!(rendered.contains("● No definition found."), "{rendered}");

    let next = inline_history_lines_for_flush(&app, 100, false, app.transcript.len());
    assert!(next.is_empty());
}

#[test]
fn inline_live_viewport_excludes_flushed_history() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("old prompt"));
    app.push_transcript_item(TranscriptItem::assistant("old answer"));
    set_input(&mut app, "new prompt".to_string());

    let output = render_inline_to_string(&app, 100, 12);

    assert!(!output.contains(">_ Squeezy v"), "{output}");
    assert!(!output.contains("old prompt"), "{output}");
    assert!(!output.contains("old answer"), "{output}");
    assert!(output.contains("new prompt┃"), "{output}");
}

#[test]
fn exit_hint_points_to_session_resume_command() {
    assert_eq!(
        exit_hint(Some("session-123")).as_deref(),
        Some("Squeezy session saved. Resume with: squeezy sessions resume session-123")
    );
    assert!(exit_hint(None).is_none());
}

#[test]
fn render_prompt_uses_rotating_coin_and_cursor() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "ship it".to_string());
    app.turn_visual = TurnVisualState::Running;

    let output = render_to_string(&app, 100, 12);
    assert!(output.contains("●  ship it┃"), "{output}");
}

#[test]
fn render_prompt_places_cursor_inside_input_text() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "abcd".to_string());
    app.input_cursor = 2;

    let output = render_to_string(&app, 100, 12);
    assert!(output.contains("●  ab┃cd"), "{output}");
    assert!(!output.contains("●  abcd┃"), "{output}");
}

#[test]
fn active_prompt_keeps_one_blank_line_after_header() {
    let app = test_app(SessionMode::Build);

    let output = render_to_string(&app, 100, 16);
    let lines = output.lines().collect::<Vec<_>>();
    let header_bottom = lines
        .iter()
        .position(|line| line.contains('╯'))
        .expect("header bottom");

    assert!(
        lines
            .get(header_bottom + 1)
            .is_some_and(|line| line.trim().is_empty()),
        "{output}"
    );
    assert!(
        lines
            .iter()
            .skip(header_bottom + 2)
            .take(2)
            .any(|line| line.contains('┃')),
        "{output}"
    );
}

#[test]
fn footer_mentions_expand_collapse_shortcut() {
    let app = test_app(SessionMode::Build);

    let output = render_to_string(&app, 120, 16);

    assert!(output.contains("Ctrl-E expand/collapse"), "{output}");
}

#[test]
fn active_prompt_cursor_is_vertically_centered() {
    let app = test_app(SessionMode::Build);

    let lines = prompt_input_lines(&app, PROMPT_MIN_HEIGHT);

    assert_eq!(lines.len(), 3);
    assert!(!lines[0].spans.iter().any(|span| span.content.contains('┃')));
    assert!(
        lines[1].spans.iter().any(|span| span.content.contains('┃')),
        "{lines:?}"
    );
    assert!(!lines[2].spans.iter().any(|span| span.content.contains('┃')));
}

#[test]
fn assistant_marker_uses_answer_color() {
    let item = TranscriptItem::assistant("done");

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal);

    assert_eq!(lines[0].spans[1].content.as_ref(), "●");
    assert_eq!(lines[0].spans[1].style.fg, Some(SUCCESS_GREEN));
    assert_eq!(lines[0].spans[3].content.as_ref(), "done");
    assert_eq!(
        lines.last().expect("trailing blank").spans.len(),
        0,
        "{lines:?}"
    );
    let text = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(!text.contains("Answered"), "{text}");
}

#[test]
fn failed_assistant_marker_uses_error_color() {
    let item = TranscriptItem::assistant("partial answer");

    let lines = format_message_entry(&item, false, false, MessageOutcome::Failed);

    assert_eq!(lines[0].spans[1].content.as_ref(), "●");
    assert_eq!(lines[0].spans[1].style.fg, Some(ERROR_RED));
}

#[test]
fn pending_assistant_uses_rotating_coin_marker() {
    let mut app = test_app(SessionMode::Build);
    app.pending_assistant = "streaming".to_string();
    app.turn_visual = TurnVisualState::Running;
    app.animation_tick = 4;

    let lines = transcript_lines_for_render(&app, Some(80), false);

    assert_eq!(lines[0].spans[1].content.as_ref(), prompt_coin_frame(&app));
    assert_eq!(
        lines[0].spans[1].style.fg,
        Some(app.turn_visual.color(app.animation_tick))
    );
    assert_eq!(lines[0].spans[3].content.as_ref(), "streaming");
    assert_eq!(
        lines.last().expect("trailing blank").spans.len(),
        0,
        "{lines:?}"
    );
}

#[test]
fn completed_task_panel_is_hidden_after_answer() {
    let mut app = test_app(SessionMode::Build);
    let mut task = sample_task_state();
    task.status = TaskStateStatus::Completed;
    app.task_state = Some(task);
    app.push_transcript_item(TranscriptItem::user("why?"));
    app.push_transcript_item(TranscriptItem::assistant("Because."));

    let output = render_to_string(&app, 120, 18);

    assert!(output.contains("● Because."), "{output}");
    assert!(!output.contains("Answered"), "{output}");
    assert!(
        !output.contains("• Done"),
        "completed task panel should not duplicate the answer: {output}"
    );
    assert!(
        !output.contains("active Start turn"),
        "completed task details should stay hidden: {output}"
    );
}

#[test]
fn running_prompt_keeps_working_line_below_submitted_prompt() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("why?"));
    app.task_state = Some(TaskStateSnapshot {
        task: "why?".to_string(),
        status: TaskStateStatus::Running,
        steps: vec![TaskStateStep {
            title: "Start turn".to_string(),
            status: TaskStepStatus::Active,
            detail: Some("Preparing the first model request".to_string()),
        }],
        next_action: Some("-".to_string()),
        verification: TaskVerificationState::NotStarted,
        ..TaskStateSnapshot::default()
    });

    let output = render_to_string(&app, 120, 18);

    assert!(output.contains("> why?"), "{output}");
    assert!(output.contains("• Working ("), "{output}");
    assert!(output.contains("esc to interrupt"), "{output}");
    assert!(!output.contains("• Done"), "{output}");
    assert!(!output.contains("active Start turn"), "{output}");
}

#[test]
fn completed_turn_shows_worked_duration_divider() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("why?"));
    app.push_transcript_item(TranscriptItem::assistant("Because."));
    app.last_turn_duration = Some(Duration::from_secs(13 * 60 + 23));

    let output = render_to_string(&app, 120, 18);

    assert!(output.contains("─ Worked for 13m 23s"), "{output}");
    assert!(!output.contains("• Working"), "{output}");
    assert!(!output.contains("• Done"), "{output}");
}

#[test]
fn working_shimmer_sweeps_left_to_right() {
    let left = shimmer_word_spans("Working", 1_200);
    let right = shimmer_word_spans("Working", 2_200);
    let repeated_left = shimmer_word_spans("Working", 4_600);
    let left_foregrounds = left.iter().map(|span| span.style.fg).collect::<Vec<_>>();
    let right_foregrounds = right.iter().map(|span| span.style.fg).collect::<Vec<_>>();

    assert!(
        left_foregrounds.contains(&Some(WORKING_SHIMMER_HIGHLIGHT)),
        "{left_foregrounds:?}"
    );
    assert!(
        right_foregrounds.contains(&Some(WORKING_SHIMMER_HIGHLIGHT)),
        "{right_foregrounds:?}"
    );
    assert!(left.iter().all(|span| span.style.bg.is_none()));
    assert!(right.iter().all(|span| span.style.bg.is_none()));
    assert_ne!(left_foregrounds, right_foregrounds);
    assert_eq!(
        left.iter().map(|span| span.style).collect::<Vec<_>>(),
        repeated_left
            .iter()
            .map(|span| span.style)
            .collect::<Vec<_>>()
    );
}

#[test]
fn working_shimmer_changes_rendered_cells_across_ticks() {
    let mut app = test_app(SessionMode::Build);
    app.task_state = Some(TaskStateSnapshot {
        task: "build".to_string(),
        status: TaskStateStatus::Running,
        ..TaskStateSnapshot::default()
    });
    app.turn_visual = TurnVisualState::Running;
    app.animation_tick_rate = Duration::from_millis(100);

    app.animation_tick = 12;
    let first = rendered_word_styles(&app, "Working");
    app.animation_tick = 22;
    let second = rendered_word_styles(&app, "Working");

    assert!(
        first
            .iter()
            .any(|(fg, bg, _)| *fg != AMBER && *bg == Color::Reset),
        "{first:?}"
    );
    assert!(
        second
            .iter()
            .any(|(fg, bg, _)| *fg != AMBER && *bg == Color::Reset),
        "{second:?}"
    );
    assert_ne!(
        first, second,
        "the rendered Working cells must animate between repaint ticks"
    );
}

#[test]
fn active_prompt_content_stays_centered_after_submitted_prompt() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("ship it"));
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);

    let output = render_to_string(&app, 100, 18);
    let lines = output.lines().collect::<Vec<_>>();
    let working_line = lines
        .iter()
        .position(|line| line.contains("• Working"))
        .expect("working line");

    assert!(
        lines
            .iter()
            .skip(working_line + 1)
            .take(3)
            .any(|line| line.contains('┃')),
        "{output}"
    );
}

#[test]
fn submitted_prompt_keeps_prompt_surface_and_working_line() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("ship it"));
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);

    let output = render_to_string(&app, 100, 18);
    let lines = output.lines().collect::<Vec<_>>();
    let prompt_line = lines
        .iter()
        .position(|line| line.contains("ship it"))
        .expect("submitted prompt");
    let working_line = lines
        .iter()
        .position(|line| line.contains("• Working"))
        .expect("working line");

    assert!(!output.contains("Asked ship it"), "{output}");
    assert!(working_line > prompt_line, "{output}");
    assert!(
        lines
            .iter()
            .skip(working_line + 1)
            .any(|line| line.contains('┃')),
        "{output}"
    );
}

#[test]
fn submitted_prompt_surface_extends_to_render_width() {
    let item = TranscriptItem::user("find getFoo");

    let lines =
        format_message_entry_with_width(&item, false, false, MessageOutcome::Normal, Some(40));
    let rendered = lines[1]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert_eq!(lines.len(), PROMPT_MIN_HEIGHT as usize + 1);
    assert_eq!(
        lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .chars()
            .count(),
        40
    );
    assert_eq!(rendered.chars().count(), 40);
    assert!(rendered.starts_with("> find getFoo"), "{rendered}");
    assert_eq!(lines[0].spans[0].style.bg, Some(PROMPT_BG));
    assert_eq!(lines[1].spans[1].style.bg, Some(PROMPT_BG));
    assert_eq!(lines[1].spans[2].style.bg, Some(PROMPT_BG));
    assert_eq!(lines[2].spans[1].style.bg, Some(PROMPT_BG));
    assert_eq!(lines.last().expect("separator").spans.len(), 0);
}

#[test]
fn submitted_prompt_preserves_empty_lines() {
    let item = TranscriptItem::user("one\n\nthree\n");

    let lines =
        format_message_entry_with_width(&item, false, false, MessageOutcome::Normal, Some(30));
    let rendered = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert_eq!(lines.len(), 7);
    assert!(rendered[1].contains("one"), "{rendered:?}");
    assert_eq!(rendered[2].trim(), "");
    assert!(rendered[3].contains("three"), "{rendered:?}");
    assert_eq!(rendered[4].trim(), "");
    assert_eq!(rendered[6], "");
    assert!(lines[..6].iter().all(|line| {
        line.spans
            .iter()
            .filter(|span| !span.content.is_empty())
            .all(|span| span.style.bg == Some(PROMPT_BG))
    }));
}

#[test]
fn failure_log_renders_as_detail_under_user_turn() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("hi"));
    app.push_log("turn failed: provider stream failed".to_string());

    let output = render_to_string(&app, 120, 16);
    assert!(output.contains("> hi"), "{output}");
    assert!(
        output.contains("└ turn failed: provider stream failed"),
        "{output}"
    );
    assert!(!output.contains("chars  turn failed"), "{output}");
}

#[test]
fn active_tool_surfaces_current_work_in_working_line() {
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);
    app.remember_active_tool_call(ToolCall {
        call_id: "call-active".to_string(),
        name: "definition_search".to_string(),
        arguments: serde_json::json!({"query": "getFoo"}),
    });

    let output = render_to_string(&app, 120, 16);

    assert!(output.contains("• Working ("), "{output}");
    assert!(output.contains("esc to interrupt"), "{output}");
    assert!(
        output.contains("Exploring definition search for getFoo"),
        "{output}"
    );
    assert!(!output.contains("Queued"), "{output}");
}

#[tokio::test]
async fn queued_tool_event_updates_visible_working_status_without_transcript_row() {
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::ToolCallQueued {
        turn_id: TurnId::new(1),
        call: ToolCall {
            call_id: "call-1".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({ "query": "getFoo" }),
        },
    })
    .await
    .expect("send queued");
    drop(tx);

    drain_agent_events(&mut app).await;

    assert_eq!(app.active_tool.as_deref(), Some("grep"));
    assert!(app.transcript.is_empty());
    let output = render_to_string(&app, 120, 16);
    assert!(output.contains("• Working ("), "{output}");
    assert!(output.contains("esc to interrupt"), "{output}");
    assert!(output.contains("Exploring grep getFoo"), "{output}");
    assert!(!output.contains("Queued"), "{output}");
    assert!(!output.contains("args="), "{output}");
}

#[tokio::test]
async fn task_state_tool_does_not_replace_visible_active_work() {
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::ToolCallQueued {
        turn_id: TurnId::new(1),
        call: ToolCall {
            call_id: "task-1".to_string(),
            name: "update_task_state".to_string(),
            arguments: serde_json::json!({"status": "running"}),
        },
    })
    .await
    .expect("send task state");
    tx.send(AgentEvent::ToolCallQueued {
        turn_id: TurnId::new(1),
        call: ToolCall {
            call_id: "shell-1".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::json!({"command": "ls"}),
        },
    })
    .await
    .expect("send shell");
    drop(tx);

    drain_agent_events(&mut app).await;

    assert_eq!(app.active_tool.as_deref(), Some("shell"));
    let output = render_to_string(&app, 120, 16);
    assert!(output.contains("Running ls"), "{output}");
    assert!(!output.contains("update_task_state"), "{output}");
}

#[test]
fn failed_user_turn_marks_status_not_prompt_text() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("hi"));
    app.push_log("turn failed: provider stream failed".to_string());

    let user_lines = format_transcript_entry(
        &app.transcript[0],
        false,
        app.tool_output_verbosity,
        message_outcome(&app.transcript, 0),
    );
    assert_eq!(user_lines[1].spans[0].style.bg, Some(PROMPT_BG));
    assert_eq!(user_lines[1].spans[1].content.as_ref(), "hi");
    assert_eq!(user_lines[1].spans[1].style.fg, Some(Color::White));
    assert_eq!(user_lines[1].spans[1].style.bg, Some(PROMPT_BG));

    let log_lines = format_transcript_entry(
        &app.transcript[1],
        false,
        app.tool_output_verbosity,
        message_outcome(&app.transcript, 1),
    );
    assert_eq!(log_lines[0].spans[1].style.fg, Some(ERROR_RED));
    assert_eq!(log_lines[0].spans[2].style.fg, Some(QUIET));
}

#[test]
fn user_prompt_text_is_highlighted_in_transcript() {
    let item = TranscriptItem::user("find getFoo");

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal);
    let text = lines[1]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert_eq!(lines[1].spans[1].content.as_ref(), "find getFoo");
    assert_eq!(lines[1].spans[0].style.bg, Some(PROMPT_BG));
    assert_eq!(lines[1].spans[1].style.bg, Some(PROMPT_BG));
    assert_eq!(lines[1].spans[1].style.fg, Some(Color::White));
    assert!(!text.contains("◐"), "{text}");
}

#[test]
fn prompt_height_grows_for_multiline_input() {
    let mut app = test_app(SessionMode::Build);
    assert_eq!(input_panel_height(&app, 100), 3);

    set_input(&mut app, "one\ntwo\nthree".to_string());
    assert_eq!(input_panel_height(&app, 100), 5);

    set_input(
        &mut app,
        (0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    assert_eq!(input_panel_height(&app, 100), PROMPT_MAX_HEIGHT);
}

#[test]
fn task_panel_keeps_non_running_state_compact() {
    let mut app = test_app(SessionMode::Build);
    app.task_state = Some(sample_task_state());

    let output = render_to_string(&app, 120, 24);
    assert!(output.contains("• Blocked Implement task UX"), "{output}");
    assert!(!output.contains("completed Inspect TUI"), "{output}");
    assert!(!output.contains("active Wire task panel"), "{output}");
    assert!(!output.contains("blocker approval pending"), "{output}");
    assert!(!output.contains("next run focused tests"), "{output}");
    assert!(!output.contains("verify running"), "{output}");
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
    assert!(collapsed.contains("• Blocked"), "{collapsed}");
    assert!(collapsed.contains("Implement task UX"), "{collapsed}");
    assert!(!collapsed.contains("active Wire task panel"), "{collapsed}");

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
async fn esc_cancels_active_turn_and_requires_double_press_to_quit_when_idle() {
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
    assert_eq!(app.status, "interrupting");

    app.turn_rx = None;
    app.cancel = None;
    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("idle esc");
    assert!(!quit);
    assert!(app.exit_armed);
    assert!(format_status_tokens(&app).contains("Esc again to exit"));

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("second idle esc");
    assert!(quit);
}

#[tokio::test]
async fn esc_cancels_pending_approval_without_active_turn() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
    app.pending_approval = Some(PendingApproval {
        request: sample_approval_request(),
        decision_tx,
    });

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("approval esc");

    assert!(!quit);
    assert_eq!(
        decision_rx.await.expect("approval decision"),
        ToolApprovalDecision::Cancelled
    );
    assert!(app.pending_approval.is_none());
    assert_eq!(app.status, "interrupting");
    assert!(!app.exit_armed);
}

#[tokio::test]
async fn ctrl_c_cancels_pending_approval_without_exiting() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
    app.pending_approval = Some(PendingApproval {
        request: sample_approval_request(),
        decision_tx,
    });

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .expect("approval ctrl-c");

    assert!(!quit);
    assert_eq!(
        decision_rx.await.expect("approval decision"),
        ToolApprovalDecision::Cancelled
    );
    assert!(app.pending_approval.is_none());
    assert_eq!(app.status, "interrupting");
}

#[tokio::test]
async fn ctrl_j_and_backslash_enter_insert_prompt_newlines() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    set_input(&mut app, "first".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-j newline");
    assert_eq!(app.input, "first\n");

    insert_input_text(&mut app, "second\\");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("backslash enter newline");
    assert_eq!(app.input, "first\nsecond\n");
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
    assert_eq!(transcript_scroll_offset(20, 10, 0), 10);
    assert_eq!(transcript_scroll_offset(20, 10, 8), 2);
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
async fn completed_event_suppresses_assistant_duplicate_shell_output_fence() {
    let mut app = test_app(SessionMode::Build);
    let stdout = [
        "section alpha status ready score 42 owner team-one",
        "section beta status ready score 84 owner team-two",
        "section gamma status stale score 21 owner team-three",
        "section delta status ready score 63 owner team-four",
    ]
    .join("\n");
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "inspect workspace summary"}),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "inspect workspace summary",
        "workdir": ".",
        "exit_code": 0,
        "stdout": stdout,
        "stderr": "",
    });
    app.push_tool_result_with_call(result, Some(call));

    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant(format!(
            "Here is the summary:\n\n```text\n{stdout}\n```\n\nI can inspect another area next."
        )),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
    })
    .await
    .expect("send completed");
    drop(tx);

    drain_agent_events(&mut app).await;

    let assistant = app.transcript.iter().find_map(|entry| match &entry.kind {
        TranscriptEntryKind::Message(item) if item.role == Role::Assistant => {
            Some(item.content.as_str())
        }
        _ => None,
    });
    assert!(
        assistant.is_none(),
        "filler-only assistant message should be dropped: {assistant:?}",
    );

    let rendered = render_to_string(&app, 140, 24);
    assert_eq!(rendered.matches("section alpha").count(), 1, "{rendered}");
}

#[tokio::test]
async fn completed_event_keeps_substantive_assistant_summary_after_tool_output() {
    let mut app = test_app(SessionMode::Build);
    let stdout = [
        "section alpha status ready score 42 owner team-one",
        "section beta status ready score 84 owner team-two",
        "section gamma status stale score 21 owner team-three",
        "section delta status ready score 63 owner team-four",
    ]
    .join("\n");
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "inspect workspace summary"}),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "inspect workspace summary",
        "workdir": ".",
        "exit_code": 0,
        "stdout": stdout,
        "stderr": "",
    });
    app.push_tool_result_with_call(result, Some(call));

    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("Gamma is stale; the other sections are ready."),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
    })
    .await
    .expect("send completed");
    drop(tx);

    drain_agent_events(&mut app).await;

    let assistant = app
        .transcript
        .iter()
        .find_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message(item) if item.role == Role::Assistant => {
                Some(item.content.as_str())
            }
            _ => None,
        })
        .expect("assistant message");
    assert_eq!(
        assistant, "Gamma is stale; the other sections are ready.",
        "{assistant}",
    );
}

#[tokio::test]
async fn completed_event_suppresses_materially_repeated_shell_output_fence() {
    let mut app = test_app(SessionMode::Build);
    let stdout = [
        "module-alpha owner platform size 704 changed 2026-05-24",
        "module-beta owner runtime size 3488 changed 2026-05-25",
        "module-gamma owner docs size 16326 changed 2026-05-24",
        "module-delta owner tools size 1088 changed 2026-05-23",
        "module-epsilon owner tests size 2048 changed 2026-05-22",
    ]
    .join("\n");
    let repeated_with_small_drift = [
        "module-alpha owner platform size 704 changed 2026-05-24",
        "module-beta owner runtime size 3489 changed 2026-05-25",
        "module-gamma owner docs size 16326 changed 2026-05-24",
        "module-delta owner tooling size 1088 changed 2026-05-23",
        "module-epsilon owner tests size 2048 changed 2026-05-22",
    ]
    .join("\n");
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "summarize module inventory"}),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "summarize module inventory",
        "workdir": ".",
        "exit_code": 0,
        "stdout": stdout,
        "stderr": "",
    });
    app.push_tool_result_with_call(result, Some(call));

    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant(format!("```text\n{repeated_with_small_drift}\n```")),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
    })
    .await
    .expect("send completed");
    drop(tx);

    drain_agent_events(&mut app).await;

    let assistant = app.transcript.iter().find_map(|entry| match &entry.kind {
        TranscriptEntryKind::Message(item) if item.role == Role::Assistant => {
            Some(item.content.as_str())
        }
        _ => None,
    });
    assert!(
        assistant.is_none(),
        "duplicate-only assistant message should be dropped: {assistant:?}",
    );

    let rendered = render_to_string(&app, 140, 24);
    assert_eq!(rendered.matches("module-alpha").count(), 1, "{rendered}");
}

#[tokio::test]
async fn pending_assistant_suppresses_streaming_duplicate_shell_output_fence() {
    let mut app = test_app(SessionMode::Build);
    let stdout = [
        "component-alpha status ready budget 704 owner runtime",
        "component-beta status ready budget 3488 owner platform",
        "component-gamma status stale budget 16326 owner docs",
        "component-delta status ready budget 1088 owner tooling",
    ]
    .join("\n");
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "inspect component report"}),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "inspect component report",
        "workdir": ".",
        "exit_code": 0,
        "stdout": stdout,
        "stderr": "",
    });
    app.push_tool_result_with_call(result, Some(call));
    app.pending_assistant = format!("Here is the report:\n\n```text\n{stdout}");

    let rendered = render_to_string(&app, 140, 24);

    assert!(!rendered.contains("Here is the report"), "{rendered}");
    assert_eq!(rendered.matches("component-alpha").count(), 1, "{rendered}");
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
fn status_footer_stays_context_only_when_scrolled() {
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
        !live.contains("selected transcript entry"),
        "no marker while at bottom: {live}"
    );

    app.transcript_scroll_from_bottom = 4;
    let scrolled = format_status_tokens(&app);
    assert!(
        !scrolled.contains("selected transcript entry"),
        "footer stays calm: {scrolled}"
    );
    assert!(scrolled.contains("Build mode"), "{scrolled}");
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

fn sample_approval_request() -> ToolApprovalRequest {
    ToolApprovalRequest {
        id: 1,
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        scope: PermissionScope::Shell,
        permission: PermissionRequest {
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
        },
        matched_rule: None,
        reason: "default compiler permission is ask".to_string(),
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

fn render_inline_to_string(app: &TuiApp, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_inline(frame, app))
        .expect("draw");
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

fn lines_to_plain_text(lines: &[Line<'_>]) -> String {
    let mut output = String::new();
    for line in lines {
        for span in &line.spans {
            output.push_str(&span.content);
        }
        output.push('\n');
    }
    output
}

fn rendered_word_styles(app: &TuiApp, word: &str) -> Vec<(Color, Color, Modifier)> {
    let width = 120;
    let height = 18;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| render(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width {
            line.push_str(buffer[(x, y)].symbol());
        }
        if let Some(start) = line.find(word) {
            let start = start as u16;
            return (0..word.len() as u16)
                .map(|offset| {
                    let cell = &buffer[(start + offset, y)];
                    (cell.fg, cell.bg, cell.modifier)
                })
                .collect();
        }
    }
    panic!("word {word:?} not rendered");
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

fn sample_attachment(id: &str) -> ContextAttachment {
    ContextAttachment {
        id: id.to_string(),
        source: ContextAttachmentSource::Paste,
        kind: ContextAttachmentKind::Config,
        status: ContextAttachmentStatus::Attached,
        label: "paste".to_string(),
        path: None,
        original_sha256: "original".to_string(),
        redacted_sha256: Some("redacted".to_string()),
        original_bytes: 319,
        stored_bytes: 300,
        preview_bytes: 120,
        redactions: 0,
        preview: "map · output shortened  ✖ Failed decl_search · invalid tool arguments"
            .to_string(),
        truncated: true,
    }
}
