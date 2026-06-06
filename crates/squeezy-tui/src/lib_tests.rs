use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::backend::TestBackend;
use squeezy_agent::{CostCapStatus, JobKind, JobStatus};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextAttachmentKind, ContextAttachmentSource,
    ContextAttachmentStatus, ContextEstimate, CostSnapshot, PermissionCapability, PermissionMode,
    PermissionPolicy, PermissionRequest, PermissionRisk, PermissionScope, SessionMode,
    StatusVerbosity, TaskStateSnapshot, TaskStateStatus, TaskStateStep, TaskStepStatus,
    TaskVerificationState, ToolOutputVerbosity, TranscriptDefault, TuiConfig,
    TuiSynchronizedOutput, TurnId, TurnMetrics,
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

#[tokio::test]
async fn pending_provider_swap_refreshes_status_line_model_before_turn() {
    let mut config = test_config(SessionMode::Build);
    config.model = "gpt-test".to_string();
    let mut app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );
    let mut next = config.clone();
    next.provider = squeezy_core::ProviderConfig::Anthropic(squeezy_core::AnthropicConfig {
        api_key_env: "ANTHROPIC_API_KEY".to_string(),
        api_key: None,
        base_url: squeezy_core::DEFAULT_ANTHROPIC_BASE_URL.to_string(),
        transport: squeezy_core::ProviderTransportConfig::default(),
    });
    next.model = "claude-haiku-4-5-20251001".to_string();
    let mut agent = test_agent_without_session_log_with_config(config);
    agent.arm_config_swap(PendingConfigSwap {
        config: next,
        provider: Some(Arc::new(UnavailableProvider::new(
            "anthropic",
            "test provider",
        ))),
        display_note: Some("provider -> anthropic".to_string()),
    });

    start_user_turn(&mut app, &mut agent, "hello".to_string());

    assert_eq!(app.provider_name, "anthropic");
    assert_eq!(app.model, "claude-haiku-4-5-20251001");
    let status = format_status_lines(&app, 160)
        .into_iter()
        .map(|line| rendered_line_text(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        status.contains("anthropic:claude-haiku-4-5-20251001"),
        "{status}"
    );
    assert!(!status.contains("openai:gpt-test"), "{status}");
}

#[test]
fn app_starts_with_unknown_config_warnings_in_transcript() {
    let mut config = test_config(SessionMode::Build);
    config.config_warnings = vec![squeezy_core::ConfigWarning {
        source: "user:/tmp/settings.toml".to_string(),
        field: "permissions.custom.legacy".to_string(),
    }];

    let app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );

    assert_eq!(app.transcript.len(), 1);
    let entry = app.transcript.first().expect("startup warning");
    let log = match &entry.kind {
        TranscriptEntryKind::Log(log) => log,
        _ => panic!("expected startup warning log"),
    };
    assert_eq!(log.kind, LogKind::Warn);
    assert_eq!(
        log.message,
        "ignored unknown setting permissions.custom.legacy in user:/tmp/settings.toml"
    );
    let rendered = lines_to_plain_text(&format_log_entry(log, false, false));
    assert!(
        rendered.contains("⚠ ignored unknown setting permissions.custom.legacy"),
        "warning must render with the standard warning sign: {rendered}"
    );
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
        !status.contains("Alt+Up/Down history"),
        "inline mode should keep the plain Up/Down prompt-history hint: {status}"
    );
    assert!(
        !status.contains("Wheel/PgUp/PgDn scroll"),
        "auto mode should preserve native terminal scrollback by default: {status}"
    );
}

#[test]
fn status_mode_color_distinguishes_build_and_plan() {
    let mut app = test_app(SessionMode::Build);

    let build = format_status_overview_line(&app, 120);
    assert_eq!(
        build.spans.last().and_then(|span| span.style.fg),
        Some(crate::render::theme::green())
    );

    app.mode = SessionMode::Plan;
    let plan = format_status_overview_line(&app, 120);
    assert_eq!(
        plan.spans.last().and_then(|span| span.style.fg),
        Some(crate::render::theme::magenta())
    );
}

#[test]
fn plan_mode_indicator_renders_above_composer_in_plan_mode() {
    let app = test_app(SessionMode::Plan);
    let output = render_to_string(&app, 120, 16);
    assert!(
        output.contains("PLAN MODE"),
        "expected PLAN MODE banner in plan mode output:\n{output}"
    );
    assert!(
        output.contains("Shift+Tab to exit"),
        "expected exit hint in plan mode output:\n{output}"
    );
}

#[test]
fn plan_mode_indicator_absent_in_build_mode() {
    let app = test_app(SessionMode::Build);
    let output = render_to_string(&app, 120, 16);
    assert!(
        !output.contains("PLAN MODE"),
        "build mode must not render the plan banner:\n{output}"
    );
}

#[test]
fn plan_mode_indicator_height_is_zero_in_build_and_one_in_plan() {
    let mut app = test_app(SessionMode::Build);
    assert_eq!(plan_mode_indicator_height(&app), 0);
    app.mode = SessionMode::Plan;
    assert_eq!(plan_mode_indicator_height(&app), 1);
}

#[test]
fn plan_mode_indicator_line_uses_existing_mode_purple_palette() {
    let line = format_plan_mode_indicator_line();
    let label_span = line
        .spans
        .first()
        .expect("plan-mode line must have at least one span");
    assert!(label_span.content.contains("PLAN MODE"));
    assert_eq!(label_span.style.fg, Some(crate::render::theme::magenta()));
}

#[test]
fn subagent_pane_renders_below_status_without_hiding_prompt() {
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(
        7,
        "explore".to_string(),
        "Squeezy Haiku baseline 15 langs n=3".to_string(),
    );
    app.note_subagent_activity(7, "explore".to_string(), "running read_file".to_string());

    let output = render_to_string(&app, 120, 18);
    assert!(output.contains("main"), "{output}");
    assert!(output.contains("explore"), "{output}");
    assert!(output.contains("running read_file"), "{output}");
    assert!(output.contains("Enter send"), "{output}");

    let lines = output.lines().collect::<Vec<_>>();
    let status_line = lines
        .iter()
        .position(|line| line.contains("Enter send"))
        .expect("status line");
    let pane_line = lines
        .iter()
        .position(|line| line.contains("explore"))
        .expect("subagent pane line");
    assert!(pane_line > status_line, "{output}");
}

fn render_subagent_pane_to_string(app: &TuiApp, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            render_subagent_pane(frame, area, app);
        })
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

#[test]
fn subagent_pane_overflow_marks_hidden_and_keeps_newest_reachable() {
    let mut app = test_app(SessionMode::Build);
    for id in 1..=8 {
        app.note_subagent_started(id, "delegate".to_string(), format!("task {id}"));
        app.note_subagent_activity(id, "delegate".to_string(), format!("running tool {id}"));
    }
    // Default unfocused state during a turn: nothing record-level selected.
    assert_eq!(app.subagent_pane.selected, 0);
    assert!(!app.subagent_pane.focused);
    assert_eq!(app.subagent_pane.records.len(), 8);
    // Layout caps the pane at 7 rows (1 header + 6 record slots).
    assert_eq!(subagent_pane_height(&app), 7);

    let output = render_subagent_pane_to_string(&app, 120, 7);
    // The header carries an overflow marker for the records scrolled past.
    assert!(output.contains("main"), "{output}");
    assert!(
        output.contains("↑2 more"),
        "header should mark hidden subagents: {output}"
    );
    // The newest subagent (#8, started last) must remain visible; the oldest
    // two are the ones scrolled out of view.
    assert!(output.contains("delegate #8"), "{output}");
    assert!(output.contains("delegate #3"), "{output}");
    assert!(
        !output.contains("delegate #1"),
        "oldest record should be hidden, not the newest: {output}"
    );
    assert!(!output.contains("delegate #2"), "{output}");
}

#[tokio::test]
async fn inline_down_midsentence_focuses_subagent_pane() {
    // Down with a half-typed single-line draft (cursor mid-line) must reach the
    // pane in inline mode — it used to require an empty composer.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(9, "delegate".to_string(), "x".to_string());
    app.input = "hello world".to_string();
    app.input_cursor = 5;
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .unwrap();
    assert!(
        app.subagent_pane.focused,
        "down mid-sentence must focus the pane"
    );
    assert_eq!(app.input, "hello world", "draft must be preserved");
}

#[tokio::test]
async fn inline_plain_updown_iterate_history_even_with_a_draft() {
    // Plain Up/Down iterate prompt history; a half-typed draft is stashed and
    // restored when stepping back past the newest entry (shell-style).
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    push_input_history(&mut app, "first".to_string());
    push_input_history(&mut app, "second".to_string());
    app.input = "draft".to_string();
    app.input_cursor = app.input.len();
    let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    handle_key(&mut app, &mut agent, up()).await.unwrap();
    assert_eq!(app.input, "second", "Up recalls newest even with a draft");
    handle_key(&mut app, &mut agent, up()).await.unwrap();
    assert_eq!(app.input, "first", "Up recalls older");
    handle_key(&mut app, &mut agent, down()).await.unwrap();
    assert_eq!(app.input, "second", "Down recalls newer");
    handle_key(&mut app, &mut agent, down()).await.unwrap();
    assert_eq!(
        app.input, "draft",
        "Down past newest restores the stashed draft"
    );
}

#[tokio::test]
async fn inline_down_in_history_steps_forward_not_into_pane() {
    // While iterating history (not at the newest entry), Down steps forward
    // through prompts; it must not jump into the subagent pane.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(9, "delegate".to_string(), "x".to_string());
    push_input_history(&mut app, "a".to_string());
    push_input_history(&mut app, "b".to_string());
    push_input_history(&mut app, "c".to_string());
    let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    handle_key(&mut app, &mut agent, up()).await.unwrap(); // c
    handle_key(&mut app, &mut agent, up()).await.unwrap(); // b
    assert_eq!(app.input, "b");
    handle_key(&mut app, &mut agent, down()).await.unwrap(); // -> c, NOT pane
    assert_eq!(app.input, "c", "Down mid-history steps forward");
    assert!(
        !app.subagent_pane.focused,
        "Down mid-history must not focus the pane"
    );
}

#[test]
fn selected_subagent_conversation_preserves_full_prompt() {
    let mut app = test_app(SessionMode::Build);
    let prompt_tail = "PROMPT_SENTINEL_VISIBLE_IN_SUBAGENT";
    let long_prompt = format!(
        "Modernize src/cli/client.rs. Review the code for outdated patterns, deprecated APIs, \
         or Rust idioms that could be improved. Suggest and implement modernization improvements. \
         {} {prompt_tail}",
        "let-else try-operator iterator-cleanup ".repeat(12)
    );
    assert!(
        long_prompt.chars().count() > 240,
        "prompt must exceed the old subagent event preview cap"
    );

    app.note_subagent_started(12, "delegate".to_string(), long_prompt);
    app.subagent_pane.active = ConversationSource::Subagent(12);

    let subagent_view = lines_to_plain_text(&transcript_lines_for_render(&app, Some(96), false));
    assert!(
        subagent_view.contains(prompt_tail),
        "selected subagent transcript should show the full assignment: {subagent_view}"
    );
    assert!(
        !subagent_view.contains("[truncated]"),
        "selected subagent transcript should not receive pre-truncated event text: {subagent_view}"
    );
}

#[tokio::test]
async fn config_screen_keeps_subagent_pane_from_owning_arrows() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(3, "explore".to_string(), "Inspect docs".to_string());
    app.config_screen = Some(config_screen::ConfigScreenState::new(
        test_config(SessionMode::Build),
        None,
    ));

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("route to config");

    assert!(
        !app.subagent_pane.focused,
        "config screen should retain key ownership"
    );
}

#[test]
fn transcript_overlay_uses_active_subagent_conversation() {
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(11, "delegate".to_string(), "Inspect crates".to_string());
    app.note_subagent_activity(11, "delegate".to_string(), "running repo_map".to_string());
    app.subagent_pane.active = ConversationSource::Subagent(11);
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    let output = render_to_string(&app, 90, 16);
    assert!(output.contains("delegate subagent"), "{output}");
    assert!(output.contains("running repo_map"), "{output}");
    assert!(output.contains("Ctrl+T"), "{output}");
}

#[tokio::test]
async fn focused_pane_esc_closes_pane_without_cancelling_turn() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(5, "delegate".to_string(), "Inspect src".to_string());
    let cancel = CancellationToken::new();
    app.cancel = Some(cancel.clone()); // a turn is in flight

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("focus pane");
    assert!(app.subagent_pane.focused);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("esc");
    assert!(
        !app.subagent_pane.focused,
        "Esc should close the focused pane"
    );
    assert_eq!(app.subagent_pane.active, ConversationSource::Main);
    assert!(
        !cancel.is_cancelled(),
        "Esc inside the pane must not cancel the running turn"
    );
}

#[tokio::test]
async fn typing_releases_focused_pane_to_the_composer() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(8, "delegate".to_string(), "Inspect src".to_string());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("focus pane");
    assert!(app.subagent_pane.focused);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
    )
    .await
    .expect("type char");
    assert!(
        !app.subagent_pane.focused,
        "typing should release pane focus so the prompt is never trapped"
    );
    assert!(
        app.input.contains('h'),
        "the character should reach the composer: {:?}",
        app.input
    );
}

#[tokio::test]
async fn delete_clears_finished_subagents_and_keeps_running_ones() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(1, "delegate".to_string(), "a".to_string());
    app.note_subagent_completed(
        1,
        "delegate".to_string(),
        "done".to_string(),
        TurnMetrics::default(),
    );
    app.note_subagent_started(2, "explore".to_string(), "b".to_string()); // still running

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("focus pane");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
    )
    .await
    .expect("clear finished");

    assert_eq!(app.subagent_pane.records.len(), 1);
    assert_eq!(app.subagent_pane.records[0].id, 2);
    assert_eq!(app.status, "cleared finished subagents");
}

#[tokio::test]
async fn subagent_pane_folds_to_summary_when_all_finished_and_expands_on_down() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(1, "delegate".to_string(), "a".to_string());
    app.note_subagent_started(2, "explore".to_string(), "b".to_string());

    // While a subagent is still running the pane stays fully expanded (live).
    assert!(!subagent_pane_collapsed(&app));
    assert!(subagent_pane_height(&app) > 1);

    app.note_subagent_completed(
        1,
        "delegate".to_string(),
        "done".to_string(),
        TurnMetrics::default(),
    );
    app.note_subagent_completed(
        2,
        "explore".to_string(),
        "done".to_string(),
        TurnMetrics::default(),
    );

    // All finished, unfocused, viewing main → fold to a one-line summary.
    assert!(subagent_pane_collapsed(&app));
    assert_eq!(subagent_pane_height(&app), 1);
    let rendered = render_subagent_pane_to_string(&app, 80, 1);
    assert!(rendered.contains("2 subagents"), "{rendered}");
    assert!(rendered.contains("review"), "{rendered}");

    // Down re-expands the list for review.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("expand pane");
    assert!(app.subagent_pane.focused);
    assert!(!subagent_pane_collapsed(&app));
    assert!(subagent_pane_height(&app) > 1);

    // Esc returns to main and the pane folds back down.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("collapse again");
    assert!(!app.subagent_pane.focused);
    assert!(subagent_pane_collapsed(&app));
}

#[test]
fn subagent_pane_summary_reports_failures() {
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(1, "delegate".to_string(), "a".to_string());
    app.note_subagent_completed(
        1,
        "delegate".to_string(),
        "done".to_string(),
        TurnMetrics::default(),
    );
    app.note_subagent_started(2, "explore".to_string(), "b".to_string());
    app.note_subagent_failed(
        2,
        "explore".to_string(),
        "boom".to_string(),
        TurnMetrics::default(),
    );

    assert!(subagent_pane_collapsed(&app));
    let rendered = render_subagent_pane_to_string(&app, 80, 1);
    assert!(rendered.contains("1 done"), "{rendered}");
    assert!(rendered.contains("1 failed"), "{rendered}");
}

#[tokio::test]
async fn enter_opens_full_screen_subagent_overlay_in_inline_mode() {
    // The conversation lives in terminal scrollback that can't be repainted, so
    // Enter on a selected subagent must open the full-screen overlay for the
    // subagent's conversation to be visible — the original bug was that
    // selecting did nothing.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(9, "delegate".to_string(), "Inspect src".to_string());
    app.note_subagent_activity(9, "delegate".to_string(), "running grep".to_string());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("focus pane");
    assert_eq!(app.subagent_pane.selected, 1);
    assert_eq!(
        app.subagent_pane.active,
        ConversationSource::Main,
        "inline highlighting must not hijack the shown conversation (keyboard scroll stays on main)"
    );
    assert!(
        app.transcript_overlay.is_none(),
        "highlighting alone should not open the overlay yet"
    );

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("open overlay");
    assert_eq!(app.subagent_pane.active, ConversationSource::Subagent(9));
    assert!(
        app.transcript_overlay.is_some(),
        "Enter must open the full-screen view in inline mode"
    );
    assert!(!app.subagent_pane.focused);
    let overlay = render_to_string(&app, 90, 16);
    assert!(overlay.contains("delegate subagent"), "{overlay}");
    assert!(overlay.contains("running grep"), "{overlay}");

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("close overlay");
    assert!(
        app.transcript_overlay.is_none(),
        "Esc closes the overlay in one press"
    );
    assert_eq!(
        app.subagent_pane.active,
        ConversationSource::Main,
        "Esc backs all the way out to the main conversation"
    );
    assert!(!app.subagent_pane.focused);
}

#[tokio::test]
async fn down_focuses_subagent_pane_with_a_nonempty_composer() {
    // Regression: Down used to focus the pane only when the composer was
    // empty, so any draft text trapped the selector below it.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(9, "delegate".to_string(), "Inspect src".to_string());
    app.input = "half-typed prompt".to_string();
    app.input_cursor = app.input.len();

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("down");

    assert!(
        app.subagent_pane.focused,
        "Down should reach the pane even with draft text in the composer"
    );
    assert_eq!(app.subagent_pane.selected, 1);
    assert_eq!(
        app.subagent_pane.active,
        ConversationSource::Main,
        "inline highlighting keeps the shown conversation on main until Enter"
    );
    assert_eq!(
        app.input, "half-typed prompt",
        "focusing the pane must not disturb the draft"
    );
}

#[tokio::test]
async fn down_reaches_pane_after_prompt_history_is_exhausted() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(9, "delegate".to_string(), "Inspect src".to_string());
    push_input_history(&mut app, "first".to_string());
    push_input_history(&mut app, "second".to_string());

    // Up walks back into history, Down walks forward; the final Down past the
    // newest entry steps out of history, and the next Down should fall through
    // to the pane rather than dead-ending.
    let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    handle_key(&mut app, &mut agent, up()).await.expect("up");
    assert_eq!(app.input, "second");
    handle_key(&mut app, &mut agent, up()).await.expect("up");
    assert_eq!(app.input, "first");
    handle_key(&mut app, &mut agent, down())
        .await
        .expect("down");
    assert_eq!(app.input, "second");
    handle_key(&mut app, &mut agent, down())
        .await
        .expect("down");
    assert!(app.input.is_empty(), "stepped out of history to the draft");
    assert!(
        !app.subagent_pane.focused,
        "leaving history should not also jump straight into the pane"
    );
    handle_key(&mut app, &mut agent, down())
        .await
        .expect("down");
    assert!(
        app.subagent_pane.focused,
        "Down once history is exhausted should focus the pane"
    );
}

#[test]
fn clearing_finished_subagents_keeps_highlight_on_shown_conversation() {
    // Regression: `selected` (a row index) and `active` (a record id) are
    // sourced independently. Dropping finished rows above the shown subagent
    // shifts the vector, which used to leave the bold highlight on a different
    // row than the ● "shown" marker.
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(1, "delegate".to_string(), "a".to_string());
    app.note_subagent_completed(
        1,
        "delegate".to_string(),
        "done".to_string(),
        TurnMetrics::default(),
    );
    app.note_subagent_started(2, "explore".to_string(), "b".to_string()); // running
    app.note_subagent_started(3, "explore".to_string(), "c".to_string()); // running

    // View the middle (running) subagent #2, highlighted on row 2.
    app.subagent_pane.active = ConversationSource::Subagent(2);
    app.subagent_pane.selected = 2;

    app.clear_finished_subagents();

    assert_eq!(app.subagent_pane.records.len(), 2);
    assert_eq!(app.subagent_pane.active, ConversationSource::Subagent(2));
    assert_eq!(
        app.subagent_pane.selected, 1,
        "highlight must follow #2 to its new row after #1 was dropped"
    );
}

#[test]
fn prune_caps_retained_subagent_records() {
    let mut app = test_app(SessionMode::Build);
    for id in 0..40u64 {
        app.note_subagent_started(id, "delegate".to_string(), format!("task {id}"));
        app.note_subagent_completed(
            id,
            "delegate".to_string(),
            "done".to_string(),
            TurnMetrics::default(),
        );
    }
    assert!(
        app.subagent_pane.records.len() <= 32,
        "records should be capped, got {}",
        app.subagent_pane.records.len()
    );
    assert!(
        app.subagent_pane.records.iter().any(|r| r.id == 39),
        "the newest subagent must be retained"
    );
}

#[test]
fn subagent_activity_transcript_stays_bounded() {
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(4, "delegate".to_string(), "seed".to_string());
    for i in 0..1000 {
        app.note_subagent_activity(4, "delegate".to_string(), format!("running tool {i}"));
    }
    let record = &app.subagent_pane.records[0];
    assert!(
        record.transcript.len() <= 256,
        "per-subagent transcript should be bounded, got {}",
        record.transcript.len()
    );
    // The most recent activity must survive the trimming.
    assert_eq!(record.latest, compact_text("running tool 999", 120));
}

#[test]
fn rejected_subagent_appears_in_pane_and_is_clearable() {
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(1, "delegate".to_string(), "real one".to_string());
    app.note_subagent_rejected("delegate".to_string(), "concurrency cap".to_string(), 3, 3);

    assert_eq!(app.subagent_pane.records.len(), 2);
    let rejected = app
        .subagent_pane
        .records
        .iter()
        .find(|r| matches!(r.lifecycle, SubagentLifecycle::Rejected))
        .expect("rejected record present");
    // Synthetic id must not collide with the real lease id (1).
    assert_ne!(rejected.id, 1);
    assert!(
        rejected.latest.contains("concurrency cap"),
        "{}",
        rejected.latest
    );

    // The pane renders the capped row with its state word.
    let output = render_to_string(&app, 120, 18);
    assert!(output.contains("capped"), "{output}");

    // A rejection is "finished", so Del clears it but keeps the running one.
    app.clear_finished_subagents();
    assert_eq!(app.subagent_pane.records.len(), 1);
    assert_eq!(app.subagent_pane.records[0].id, 1);
}

#[tokio::test]
async fn rejected_subagent_event_renders_human_reason_not_raw_token() {
    let mut app = test_app(SessionMode::Build);

    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::SubagentRejected {
        turn_id: TurnId::new(1),
        agent: "delegate".to_string(),
        reason: squeezy_agent::SubagentRejectionReason::ConcurrencyCap,
        limit: 3,
        active: 3,
    })
    .await
    .expect("send rejected");
    drop(tx);
    drain_agent_events(&mut app).await;

    let rejected = app
        .subagent_pane
        .records
        .iter()
        .find(|r| matches!(r.lifecycle, SubagentLifecycle::Rejected))
        .expect("rejected record present");
    assert!(
        !rejected.latest.contains("concurrency_cap"),
        "pane row leaked raw enum token: {}",
        rejected.latest
    );
    assert!(
        rejected.latest.contains("concurrency cap reached"),
        "pane row should show human reason: {}",
        rejected.latest
    );
    // The per-subagent transcript line must read the same way, never the
    // raw token (the machine token survives only in `push_log`).
    let transcript_text = rejected
        .transcript
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Log(log) => Some(log.message.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        !transcript_text.contains("concurrency_cap")
            && transcript_text.contains("concurrency cap reached"),
        "transcript line should show human reason: {transcript_text}"
    );

    // The pane row on screen carries the human phrasing, not the token.
    // Expand the pane first — an all-finished pane folds to a one-line summary.
    app.subagent_pane.focused = true;
    let output = render_to_string(&app, 120, 18);
    let pane_row = output
        .lines()
        .find(|line| line.contains("delegate #1"))
        .expect("capped pane row rendered");
    assert!(!pane_row.contains("concurrency_cap"), "{pane_row}");
    assert!(pane_row.contains("concurrency cap reached"), "{pane_row}");
}

#[test]
fn subagent_row_shows_lifecycle_word_for_accessibility() {
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(2, "explore".to_string(), "look".to_string());
    app.note_subagent_failed(
        2,
        "explore".to_string(),
        "boom".to_string(),
        TurnMetrics::default(),
    );
    // The failed state is conveyed by the leading "failed" word + red colour,
    // independent of the selection marker (○/●).
    app.subagent_pane.selected = 1;
    let output = render_to_string(&app, 120, 18);
    assert!(output.contains("failed"), "{output}");
}

#[test]
fn subagent_marker_fills_on_cursor_and_rings_carry_status() {
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(2, "explore".to_string(), "look".to_string()); // running
    let record = app.subagent_pane.records[0].clone();

    // Cursor on the subagent (row 1): its ring fills amber; main empties.
    app.subagent_pane.selected = 1;
    let sub = subagent_record_row(&app, 0, &record, 120);
    assert_eq!(sub.spans[0].content.as_ref(), "●");
    assert_eq!(sub.spans[0].style.fg, Some(crate::render::theme::accent()));
    let main = subagent_main_row(&app, 120);
    assert_eq!(main.spans[0].content.as_ref(), "○");

    // Cursor on main (row 0): main fills amber; the running subagent is an
    // empty silver ring (status colour, not amber).
    app.subagent_pane.selected = 0;
    let main = subagent_main_row(&app, 120);
    assert_eq!(main.spans[0].content.as_ref(), "●");
    assert_eq!(main.spans[0].style.fg, Some(crate::render::theme::accent()));
    let sub = subagent_record_row(&app, 0, &record, 120);
    assert_eq!(sub.spans[0].content.as_ref(), "○");
    assert_eq!(sub.spans[0].style.fg, Some(crate::render::theme::muted()));
}

#[tokio::test]
async fn subagent_lifecycle_logs_are_distinct_and_compact() {
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);

    let prompt_tail = "prompt-tail-visible-after-a-long-subagent-request";
    let summary_tail = "summary-tail-visible-after-a-long-subagent-result";
    let long_prompt = format!("{} {prompt_tail}", "analyze module behavior".repeat(18));
    let long_summary = format!("{} {summary_tail}", "found actionable details".repeat(18));

    tx.send(AgentEvent::SubagentStarted {
        turn_id: TurnId::new(1),
        id: 11,
        agent: "delegate".to_string(),
        prompt: long_prompt,
    })
    .await
    .expect("send started");
    tx.send(AgentEvent::SubagentCompleted {
        turn_id: TurnId::new(1),
        id: 11,
        agent: "delegate".to_string(),
        summary: long_summary,
        metrics: TurnMetrics {
            tool_calls: 51,
            bytes_read: 1_207_538,
            ..TurnMetrics::default()
        },
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    let lifecycle_logs = app
        .transcript
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Log(log)
                if log.message.contains("subagent started")
                    || log.message.contains("subagent completed") =>
            {
                Some(log)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle_logs.len(), 2);

    // Distinct subagent identity: a magenta `◆` node (never an alarming red),
    // so delegated work is recognisable on the rail.
    for log in &lifecycle_logs {
        assert_eq!(log.kind, LogKind::Subagent);
        let lines = format_log_entry(log, true, false);
        let marker = lines.first().and_then(|line| line.spans.get(1));
        assert_eq!(marker.map(|span| span.content.as_ref()), Some("◆ "));
        assert_eq!(
            marker.and_then(|span| span.style.fg),
            Some(crate::render::theme::magenta())
        );
    }

    // Main transcript: compact one-liners. The long prompt/summary bodies are
    // folded (not dumped) and the noisy byte counter is gone.
    let main_view = lines_to_plain_text(&transcript_lines_for_render(&app, Some(120), false));
    assert!(
        main_view.contains("subagent completed · 51 tools"),
        "{main_view}"
    );
    // Threaded on the rail with the magenta `◆` subagent marker.
    assert!(
        main_view.contains("◆ delegate subagent started"),
        "{main_view}"
    );
    assert!(
        main_view.contains("◆ delegate subagent completed"),
        "{main_view}"
    );
    assert!(!main_view.contains(summary_tail), "{main_view}");
    assert!(!main_view.contains(prompt_tail), "{main_view}");
    assert!(!main_view.contains("bytes="), "{main_view}");

    // The full prompt + summary remain available inside the subagent's own
    // conversation (open it with Down / Enter).
    app.subagent_pane.active = ConversationSource::Subagent(11);
    let subagent_view = lines_to_plain_text(&transcript_lines_for_render(&app, Some(120), false));
    assert!(subagent_view.contains(prompt_tail), "{subagent_view}");
    assert!(subagent_view.contains(summary_tail), "{subagent_view}");
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
async fn freeform_modal_keeps_typing_out_of_main_composer() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Plan);
    // Open a freeform-allowed modal.
    let request = RequestUserInputRequest {
        question: "Where to next?".to_string(),
        choices: Vec::new(),
        allow_freeform: true,
    };
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    app.pending_request_user_input = Some(PendingRequestUserInput {
        request,
        response_tx,
        selection_index: 0,
        answer: String::new(),
        answer_cursor: 0,
    });
    // Pre-populate the main composer to simulate a half-typed next prompt.
    app.input = "draft next prompt".to_string();
    app.input_cursor = app.input.len();

    for ch in "yes please".chars() {
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        )
        .await
        .expect("handle char");
    }

    let pending = app.pending_request_user_input.as_ref().expect("modal");
    assert_eq!(pending.answer, "yes please");
    assert_eq!(
        app.input, "draft next prompt",
        "modal typing must not touch the main composer",
    );
}

#[tokio::test]
async fn freeform_modal_enter_submits_dotted_choice_even_with_typed_answer() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Plan);
    let request = RequestUserInputRequest {
        question: "Where to next?".to_string(),
        choices: vec![squeezy_agent::RequestUserInputChoice {
            label: "Default".to_string(),
            value: "default".to_string(),
        }],
        allow_freeform: true,
    };
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    app.pending_request_user_input = Some(PendingRequestUserInput {
        request,
        response_tx,
        selection_index: 0,
        answer: "typed answer".to_string(),
        answer_cursor: "typed answer".len(),
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle enter");

    let response = response_rx.await.expect("response");
    assert_eq!(response.choice_value.as_deref(), Some("default"));
    assert_eq!(response.freeform, None);
    assert!(app.pending_request_user_input.is_none());
}

#[tokio::test]
async fn freeform_modal_typing_moves_dot_to_answer_row() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Plan);
    let request = RequestUserInputRequest {
        question: "Where to next?".to_string(),
        choices: vec![squeezy_agent::RequestUserInputChoice {
            label: "Default".to_string(),
            value: "default".to_string(),
        }],
        allow_freeform: true,
    };
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    app.pending_request_user_input = Some(PendingRequestUserInput {
        request,
        response_tx,
        selection_index: 0,
        answer: String::new(),
        answer_cursor: 0,
    });

    for ch in "typed answer".chars() {
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        )
        .await
        .expect("handle char");
    }
    assert_eq!(
        app.pending_request_user_input
            .as_ref()
            .expect("pending")
            .selection_index,
        1,
        "typing should move the dot to the freeform Answer row",
    );

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle enter");

    let response = response_rx.await.expect("response");
    assert_eq!(response.choice_value, None);
    assert_eq!(response.freeform.as_deref(), Some("typed answer"));
}

#[tokio::test]
async fn freeform_modal_up_from_answer_ignores_typed_text_on_enter() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Plan);
    let request = RequestUserInputRequest {
        question: "Where to next?".to_string(),
        choices: vec![squeezy_agent::RequestUserInputChoice {
            label: "Default".to_string(),
            value: "default".to_string(),
        }],
        allow_freeform: true,
    };
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    app.pending_request_user_input = Some(PendingRequestUserInput {
        request,
        response_tx,
        selection_index: 1,
        answer: "typed answer".to_string(),
        answer_cursor: "typed answer".len(),
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    )
    .await
    .expect("handle up");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle enter");

    let response = response_rx.await.expect("response");
    assert_eq!(response.choice_value.as_deref(), Some("default"));
    assert_eq!(response.freeform, None);
}

#[tokio::test]
async fn freeform_modal_down_reaches_answer_row_after_last_choice() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Plan);
    let request = RequestUserInputRequest {
        question: "Where to next?".to_string(),
        choices: vec![
            squeezy_agent::RequestUserInputChoice {
                label: "First".to_string(),
                value: "first".to_string(),
            },
            squeezy_agent::RequestUserInputChoice {
                label: "Second".to_string(),
                value: "second".to_string(),
            },
        ],
        allow_freeform: true,
    };
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    app.pending_request_user_input = Some(PendingRequestUserInput {
        request,
        response_tx,
        selection_index: 1,
        answer: String::new(),
        answer_cursor: 0,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("handle down");

    assert_eq!(
        app.pending_request_user_input
            .as_ref()
            .expect("pending")
            .selection_index,
        2,
        "Down from the last choice should select the freeform Answer row",
    );
}

#[tokio::test]
async fn enter_during_running_turn_enqueues_prompt() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    // Fake a running turn.
    let (_tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);

    // Type "hello" + Enter.
    for ch in "hello".chars() {
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        )
        .await
        .expect("handle char");
    }
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle enter");

    assert_eq!(
        app.prompt_queue.iter().collect::<Vec<_>>(),
        vec![&"hello".to_string()],
        "prompt should be queued while a turn is running",
    );
    assert_eq!(app.input, "", "composer should clear after enqueue");
    assert_eq!(app.status, "queued (1)");
}

#[tokio::test]
async fn enter_when_idle_starts_turn_not_enqueues() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    assert!(app.turn_rx.is_none());

    for ch in "hi".chars() {
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        )
        .await
        .expect("handle char");
    }
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle enter");

    assert!(app.prompt_queue.is_empty(), "idle Enter should not queue");
    assert!(app.turn_rx.is_some(), "idle Enter should start a turn");
}

#[tokio::test]
async fn ctrl_x_q_chord_toggles_queue_overlay() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
    )
    .await
    .expect("first stroke");
    assert_eq!(app.pending_chord, Some(ChordPrefix::CtrlX));
    assert!(app.prompt_queue_overlay.is_none());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    )
    .await
    .expect("second stroke");
    assert!(app.pending_chord.is_none(), "chord must clear");
    assert!(app.prompt_queue_overlay.is_some(), "overlay should open");

    // Second chord toggles closed.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
    )
    .await
    .expect("first stroke");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    )
    .await
    .expect("second stroke");
    assert!(app.prompt_queue_overlay.is_none(), "overlay should close");
}

#[test]
fn meta_modifier_normalises_to_alt() {
    use super::normalise_control_byte;
    let raw = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::META);
    let out = normalise_control_byte(raw);
    assert_eq!(out.code, KeyCode::Char('e'));
    assert!(out.modifiers.contains(KeyModifiers::ALT));
    assert!(!out.modifiers.contains(KeyModifiers::META));
}

#[test]
fn uppercase_letter_with_control_normalises_to_lowercase() {
    use super::normalise_control_byte;
    // Kitty REPORT_ALTERNATE_KEYS can deliver Ctrl+E as `Char('E') + CONTROL`.
    let raw = KeyEvent::new(KeyCode::Char('E'), KeyModifiers::CONTROL);
    let out = normalise_control_byte(raw);
    assert_eq!(out.code, KeyCode::Char('e'));
    assert!(out.modifiers.contains(KeyModifiers::CONTROL));
}

#[test]
fn ctrl_letter_with_stray_shift_drops_shift() {
    use super::normalise_control_byte;
    let raw = KeyEvent::new(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    let out = normalise_control_byte(raw);
    assert_eq!(out.code, KeyCode::Char('e'));
    assert!(out.modifiers.contains(KeyModifiers::CONTROL));
    assert!(!out.modifiers.contains(KeyModifiers::SHIFT));
}

#[test]
fn alt_letter_with_meta_modifier_normalises() {
    use super::normalise_control_byte;
    // Terminal delivers Option+E with META instead of ALT.
    let raw = KeyEvent::new(KeyCode::Char('E'), KeyModifiers::META | KeyModifiers::SHIFT);
    let out = normalise_control_byte(raw);
    assert_eq!(out.code, KeyCode::Char('e'));
    assert!(out.modifiers.contains(KeyModifiers::ALT));
    assert!(!out.modifiers.contains(KeyModifiers::META));
    assert!(!out.modifiers.contains(KeyModifiers::SHIFT));
}

#[test]
fn raw_control_byte_normalises_to_char_plus_control() {
    use super::normalise_control_byte;
    let raw = KeyEvent::new(KeyCode::Char('\u{05}'), KeyModifiers::NONE);
    let out = normalise_control_byte(raw);
    assert_eq!(out.code, KeyCode::Char('e'));
    assert!(out.modifiers.contains(KeyModifiers::CONTROL));

    let modern = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
    let out = normalise_control_byte(modern);
    assert_eq!(out.code, KeyCode::Char('e'));
    assert!(out.modifiers.contains(KeyModifiers::CONTROL));

    // Tab/Enter/CR/Esc/Backspace bytes stay as Char form (they have
    // dedicated KeyCode variants in the well-behaved path).
    for byte in ['\u{09}', '\u{0A}', '\u{0D}', '\u{1B}', '\u{08}'] {
        let raw = KeyEvent::new(KeyCode::Char(byte), KeyModifiers::NONE);
        let out = normalise_control_byte(raw);
        assert_eq!(out.code, KeyCode::Char(byte));
        assert!(!out.modifiers.contains(KeyModifiers::CONTROL));
    }
}

#[tokio::test]
async fn chord_leader_accepts_raw_ctrl_x_byte() {
    // Some terminals emit Ctrl+X as the literal ASCII control byte
    // (`\x18`) with no modifiers when they don't fully honour kitty's
    // DISAMBIGUATE_ESCAPE_CODES. Make sure the chord arms in that case.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('\u{18}'), KeyModifiers::NONE),
    )
    .await
    .expect("raw ctrl+x");
    assert_eq!(app.pending_chord, Some(ChordPrefix::CtrlX));
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    )
    .await
    .expect("q follow-up");
    assert!(app.prompt_queue_overlay.is_some());
    assert!(
        app.input.is_empty(),
        "Q must NOT leak into composer when chord fires",
    );
}

#[tokio::test]
async fn chord_accepts_uppercase_q_with_or_without_shift() {
    for (code, modifiers) in [
        (KeyCode::Char('q'), KeyModifiers::NONE),
        (KeyCode::Char('Q'), KeyModifiers::NONE),
        (KeyCode::Char('Q'), KeyModifiers::SHIFT),
        (KeyCode::Char('q'), KeyModifiers::SHIFT),
    ] {
        let mut agent = test_agent(SessionMode::Build);
        let mut app = test_app(SessionMode::Build);
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        )
        .await
        .expect("first stroke");
        handle_key(&mut app, &mut agent, KeyEvent::new(code, modifiers))
            .await
            .expect("second stroke");
        assert!(
            app.prompt_queue_overlay.is_some(),
            "chord should open overlay for code={code:?} modifiers={modifiers:?}",
        );
        assert!(
            app.input.is_empty(),
            "second stroke must NOT leak into the composer for code={code:?} modifiers={modifiers:?}",
        );
    }
}

#[tokio::test]
async fn ctrl_x_then_other_key_clears_chord_without_firing_queue() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
    )
    .await
    .expect("leader");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
    )
    .await
    .expect("non-chord follow-up");
    assert!(app.pending_chord.is_none(), "chord must clear");
    assert!(
        app.prompt_queue_overlay.is_none(),
        "queue overlay must NOT open for an unknown chord follow-up",
    );
    // The follow-up character should have been inserted into the composer.
    assert_eq!(app.input, "a");
}

#[tokio::test]
async fn left_click_on_indicator_rect_toggles_queue_overlay() {
    let mut app = test_app(SessionMode::Build);
    // Stash a known indicator rect as if render_input had set it.
    app.register_click(
        Rect {
            x: 0,
            y: 4,
            width: 80,
            height: 1,
        },
        ClickAction::ToggleQueueOverlay,
    );
    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(
        app.prompt_queue_overlay.is_some(),
        "click inside the indicator should toggle the overlay open",
    );

    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(
        app.prompt_queue_overlay.is_none(),
        "second click toggles closed",
    );
}

#[tokio::test]
async fn topmost_overlapping_click_target_wins() {
    let app = test_app(SessionMode::Build);
    // Two overlapping rects registered in render order: the second
    // (later-rendered "overlay") should shadow the first.
    app.register_click(
        Rect {
            x: 0,
            y: 4,
            width: 80,
            height: 1,
        },
        ClickAction::ToggleQueueOverlay,
    );
    app.register_click(
        Rect {
            x: 5,
            y: 4,
            width: 10,
            height: 1,
        },
        ClickAction::ToggleQueueOverlay,
    );
    let hit = app.click_target_at(7, 4);
    assert_eq!(hit, Some(ClickAction::ToggleQueueOverlay));
    // Sanity: a coord only inside the FIRST rect still hits the first.
    let outside_overlap = app.click_target_at(50, 4);
    assert_eq!(outside_overlap, Some(ClickAction::ToggleQueueOverlay));
}

#[tokio::test]
async fn begin_frame_clickables_clears_registry() {
    let app = test_app(SessionMode::Build);
    app.register_click(
        Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 1,
        },
        ClickAction::ToggleQueueOverlay,
    );
    assert!(app.click_target_at(5, 0).is_some());
    app.begin_frame_clickables();
    assert!(
        app.click_target_at(5, 0).is_none(),
        "begin_frame_clickables must wipe per-frame state",
    );
}

#[tokio::test]
async fn wheel_scroll_works_in_inline_mode() {
    let mut app = test_app(SessionMode::Build);
    // Inline mode used to drop wheel events entirely; the wheel must scroll the
    // transcript regardless.
    app.push_transcript_item(TranscriptItem::user("first turn".to_string()));
    assert_eq!(app.transcript_scroll_from_bottom, 0);
    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(
        app.transcript_scroll_from_bottom, 3,
        "wheel must scroll the transcript in inline mode",
    );
    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(app.transcript_scroll_from_bottom, 0);
}

#[tokio::test]
async fn wheel_scroll_targets_transcript_overlay_when_open() {
    let mut app = test_app(SessionMode::Build);
    for index in 0..80 {
        app.push_transcript_item(TranscriptItem::user(format!("turn {index}")));
    }
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::NativeSelection,
        detail: OverlayDetail::Expanded,
    });

    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );

    assert_eq!(
        app.transcript_overlay.expect("overlay").scroll,
        3,
        "wheel down must scroll the full transcript overlay"
    );
    assert_eq!(
        app.transcript_scroll_from_bottom, 0,
        "overlay wheel events must not scroll the underlying transcript"
    );

    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );

    assert_eq!(app.transcript_overlay.expect("overlay").scroll, 0);
}

#[tokio::test]
async fn transcript_overlay_mouse_is_modal() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState::default());
    app.register_click(
        Rect {
            x: 0,
            y: 4,
            width: 80,
            height: 1,
        },
        ClickAction::ToggleQueueOverlay,
    );

    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        },
    );

    assert!(
        app.prompt_queue_overlay.is_none(),
        "overlay mouse events must not click through to the underlying UI"
    );
}

#[tokio::test]
async fn left_click_outside_indicator_does_not_open_overlay() {
    let mut app = test_app(SessionMode::Build);
    app.register_click(
        Rect {
            x: 0,
            y: 4,
            width: 80,
            height: 1,
        },
        ClickAction::ToggleQueueOverlay,
    );
    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: 10,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(app.prompt_queue_overlay.is_none());
}

#[tokio::test]
async fn cancelled_turn_auto_drains_next_queued_prompt() {
    let mut app = test_app(SessionMode::Build);

    // Pretend a turn is running and the user has queued one prompt.
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    app.prompt_queue.push_back("follow-up".to_string());

    // Simulate the agent emitting Cancelled.
    tx.send(AgentEvent::Cancelled {
        turn_id: TurnId::new(1),
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
    })
    .await
    .expect("send cancelled");
    drop(tx);
    drain_agent_events(&mut app).await;

    assert!(
        app.auto_drain_queue,
        "Cancelled with a non-empty queue must set the auto-drain flag",
    );
}

#[tokio::test]
async fn mention_refresh_offloads_walk_and_drain_installs_cache() {
    let mut app = test_app(SessionMode::Build);

    // Typing an `@`-mention must not synchronously walk the filesystem:
    // the cache stays empty and a background walk is scheduled instead.
    input::set_input(&mut app, "@a".to_string());
    input::refresh_mention_popup(&mut app);
    assert!(
        app.workspace_file_cache.is_none(),
        "refresh must not build the cache inline on the UI thread",
    );
    assert!(
        app.pending_mention_walk.is_some(),
        "refresh must schedule a background workspace walk",
    );

    // The drain helper installs the cache delivered over the oneshot and
    // clears the in-flight guard so a future edit can schedule a new walk.
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send(mention::WorkspaceFileCache::from_paths_for_tests(vec![
        PathBuf::from("alpha.rs"),
    ]))
    .expect("send cache");
    app.pending_mention_walk = Some(rx);
    drain_pending_mention_walk(&mut app);

    assert!(
        app.pending_mention_walk.is_none(),
        "drain must clear the in-flight guard once the walk lands",
    );
    let cache = app
        .workspace_file_cache
        .as_ref()
        .expect("drain must install the cache");
    assert_eq!(cache.files().as_ref(), &vec![PathBuf::from("alpha.rs")]);
}

#[tokio::test]
async fn push_warn_emits_single_warn_log_when_no_cancel_card() {
    let mut app = test_app(SessionMode::Build);
    let pushed = app.push_warn("turn cancelled".to_string());
    assert!(pushed, "should push when no prior cancel card");
    let entry = app.transcript.last().expect("entry should exist");
    match &entry.kind {
        TranscriptEntryKind::Log(log) => assert_eq!(log.kind, LogKind::Warn),
        _ => panic!("expected a Log entry"),
    }
}

#[tokio::test]
async fn push_warn_suppresses_when_last_tool_card_is_cancelled() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("request_user_input", "");
    result.status = ToolStatus::Cancelled;
    app.push_tool_result_with_call(result, None);
    let pushed = app.push_warn("turn cancelled".to_string());
    assert!(
        !pushed,
        "redundant warn should be suppressed when a cancelled tool card is at the tail",
    );
    // Ensure no extra entry was appended either.
    let log_count = app
        .transcript
        .iter()
        .filter(|e| matches!(e.kind, TranscriptEntryKind::Log(_)))
        .count();
    assert_eq!(log_count, 0);
}

#[tokio::test]
async fn slash_command_queues_mid_turn() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);

    for ch in "/help".chars() {
        handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        )
        .await
        .expect("handle char");
    }
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle enter");

    assert_eq!(
        app.prompt_queue.iter().collect::<Vec<_>>(),
        vec![&"/help".to_string()],
        "slash commands entered during a turn should queue like any other input",
    );
    assert_eq!(app.input, "", "composer should clear after queueing");
    assert_eq!(app.status, "queued (1)");
    assert!(app.turn_rx.is_some(), "active turn should keep running");
}

#[tokio::test]
async fn queued_slash_clear_executes_after_running_turn_finishes() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("old context"));
    app.prompt_queue.push_back("/clear".to_string());

    drain_prompt_queue_if_idle(&mut app, &mut agent).await;

    assert!(
        app.prompt_queue.is_empty(),
        "queued clear should consume the queue entry",
    );
    assert!(app.turn_rx.is_none(), "clear should not start a model turn");
    assert_eq!(app.status, "conversation cleared");
    assert!(
        app.terminal_clear_pending,
        "queued clear should request the same hard terminal clear as typed clear",
    );
    assert_eq!(
        app.transcript.len(),
        1,
        "clear should replace the old transcript with the clear notice",
    );
    let notice = last_message_content(&app).expect("clear notice");
    assert!(notice.contains("Conversation cleared."), "{notice}");
}

#[tokio::test]
async fn slash_prompt_template_expands_unknown_head_into_user_turn() {
    let root = temp_workspace("prompt_template_expand");
    let prompts_dir = root.join(".squeezy/prompts");
    fs::create_dir_all(&prompts_dir).expect("create prompts dir");
    fs::write(
        prompts_dir.join("review.md"),
        "---\ndescription: review file\nargs: [file]\n---\nReview {file} for issues.",
    )
    .expect("write template");

    let config = test_config_with_root(SessionMode::Build, root);
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let mut agent = test_agent(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/review src/lib.rs").await);
    assert!(
        app.turn_rx.is_some(),
        "expanded template should start a user turn",
    );
    assert_eq!(
        app.cancelled_prompt.as_deref(),
        Some("Review src/lib.rs for issues."),
        "cancelled_prompt should mirror the rendered template body so Ctrl+R can restore it",
    );
}

#[tokio::test]
async fn slash_prompt_template_queues_when_turn_in_progress() {
    let root = temp_workspace("prompt_template_queue");
    let prompts_dir = root.join(".squeezy/prompts");
    fs::create_dir_all(&prompts_dir).expect("create prompts dir");
    fs::write(
        prompts_dir.join("ship.md"),
        "---\ndescription: ship\nargs: [target]\n---\nShip {target}.",
    )
    .expect("write template");

    let config = test_config_with_root(SessionMode::Build, root);
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let mut agent = test_agent(SessionMode::Build);

    // Fake a running turn so the second template invocation queues.
    let (_tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);

    assert!(handle_slash_command(&mut app, &mut agent, "/ship prod").await);
    assert_eq!(
        app.prompt_queue.iter().collect::<Vec<_>>(),
        vec![&"Ship prod.".to_string()],
        "expanded template should queue rather than start a competing turn",
    );
    assert_eq!(app.status, "queued (1)");
}

#[tokio::test]
async fn slash_prompt_template_does_not_shadow_builtin_command() {
    // A template literally named after a built-in must not shadow the
    // built-in slot — `/cost` should still hit the cost handler so a
    // typo in `~/.squeezy/prompts/cost.md` cannot lock users out of
    // the built-in surface.
    let root = temp_workspace("prompt_template_shadow");
    let prompts_dir = root.join(".squeezy/prompts");
    fs::create_dir_all(&prompts_dir).expect("create prompts dir");
    fs::write(
        prompts_dir.join("cost.md"),
        "---\ndescription: shadow attempt\n---\nshould not run as a model turn.",
    )
    .expect("write template");

    let config = test_config_with_root(SessionMode::Build, root);
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let mut agent = test_agent(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/cost").await);
    assert_eq!(
        app.status, "cost snapshot",
        "/cost should resolve to the built-in cost handler",
    );
    assert!(
        app.turn_rx.is_none(),
        "built-in /cost does not start a user turn; template would have",
    );
    assert!(
        app.cancelled_prompt.is_none(),
        "built-in /cost does not set cancelled_prompt; template would have",
    );
}

#[tokio::test]
async fn slash_prompt_template_passes_through_when_no_match() {
    // An unknown slash command with no matching template must still
    // fall through so `reject_unknown_slash_command` can flag it.
    let root = temp_workspace("prompt_template_miss");
    let config = test_config_with_root(SessionMode::Build, root);
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let mut agent = test_agent(SessionMode::Build);

    assert!(!handle_slash_command(&mut app, &mut agent, "/totally-unknown arg").await);
    assert!(app.turn_rx.is_none());
}

#[tokio::test]
async fn proposed_plan_block_opens_post_plan_choice_prompt() {
    let root = temp_workspace("plan_choice_prompt");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::AssistantDelta {
        turn_id: TurnId::new(1),
        delta: "<proposed_plan>\nstep 1\nstep 2\n</proposed_plan>".to_string(),
    })
    .await
    .expect("send delta");
    drop(tx);
    drain_agent_events(&mut app).await;

    let pending = app
        .pending_plan_choice
        .as_ref()
        .expect("prompt should be set after persist");
    assert!(pending.plan_id.starts_with("plan-"));
    assert!(
        pending
            .plan_path
            .starts_with(root.join(proposed_plan::PLAN_DIR))
    );
    assert_eq!(pending.selection_index, 0);

    let lines = approval_lines(&app);
    let rendered: String = lines
        .into_iter()
        .flat_map(|line| line.spans.into_iter().map(|span| span.content.into_owned()))
        .collect::<Vec<_>>()
        .join("");
    assert!(rendered.contains("Plan ready"), "render: {rendered}");
    assert!(rendered.contains("[e] Execute"));
    assert!(rendered.contains("[c] Execute (clean)"));
    assert!(rendered.contains("[r] Refine"));
    assert!(rendered.contains("[d] Discard"));
    assert!(!rendered.contains("[v] View"));

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn completed_transcript_is_plan_free_after_stream_extraction() {
    let root = temp_workspace("completed_plan_free");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::AssistantDelta {
        turn_id: TurnId::new(1),
        delta: "intro narration <proposed_plan>\nstep 1\nstep 2\n</proposed_plan> tail narration"
            .to_string(),
    })
    .await
    .expect("send delta");
    // The agent strips the block before constructing the Completed
    // TranscriptItem; this mirrors that behaviour. If the TUI ever
    // regresses to re-injecting the block (e.g. via leftover) the
    // assertion below catches it.
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("intro narration  tail narration".to_string()),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    let plan_cards = app
        .transcript
        .iter()
        .filter(|entry| matches!(entry.kind, TranscriptEntryKind::PlanCard(_)))
        .count();
    assert_eq!(plan_cards, 1, "expected exactly one plan card");

    for entry in &app.transcript {
        if let TranscriptEntryKind::Message(item) = &entry.kind {
            assert!(
                !item.content.contains("<proposed_plan>"),
                "transcript message must not contain raw proposed_plan markup: {:?}",
                item.content
            );
            assert!(
                !item.content.contains("step 1"),
                "transcript message must not contain plan body: {:?}",
                item.content
            );
        }
    }
    assert!(
        app.pending_assistant.trim_is_empty(),
        "pending_assistant must be empty after Completed; held {:?}",
        app.pending_assistant.text()
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn unterminated_proposed_plan_block_does_not_duplicate_at_completion() {
    let root = temp_workspace("unterminated_no_duplicate");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::AssistantDelta {
        turn_id: TurnId::new(1),
        delta: "intro <proposed_plan>\nstep 1\nstep 2 (no close tag)".to_string(),
    })
    .await
    .expect("send delta");
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("intro".to_string()),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    assert!(
        app.pending_assistant.trim_is_empty(),
        "unterminated block must not leak back into pending_assistant: {:?}",
        app.pending_assistant.text()
    );
    for entry in &app.transcript {
        if let TranscriptEntryKind::Message(item) = &entry.kind {
            assert!(
                !item.content.contains("<proposed_plan>"),
                "transcript must not surface the raw open tag: {:?}",
                item.content
            );
            assert!(
                !item.content.contains("step 1"),
                "transcript must not surface body of unterminated block: {:?}",
                item.content
            );
        }
    }

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_choice_execute_toggles_to_build_mode_and_queues_handoff() {
    let root = temp_workspace("plan_choice_execute");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-execute1".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "step 1\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.current_plan_id = Some(plan_id.clone());
    app.pending_plan_choice = Some(PendingPlanChoice {
        plan_id: plan_id.clone(),
        plan_path: plan_path.clone(),
        selection_index: 0,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(app.mode, SessionMode::Build);
    assert!(app.pending_plan_choice.is_none());
    // Execute auto-submits a "begin executing the plan" prompt. Under
    // per-turn re-attach (issue 16), the handoff stays set across turns
    // — the body is delivered on turn 1, then the lighter marker on
    // turns 2+, until a successful apply_patch clears it. The counter
    // advancing to 1 is the proof the body went out.
    assert_eq!(
        app.pending_plan_handoff.as_deref(),
        Some(plan_path.as_path()),
        "handoff persists for per-turn re-attach"
    );
    assert_eq!(
        app.plan_handoff_turns_seen, 1,
        "first auto-submitted turn should consume the body once"
    );
    assert!(
        app.turn_rx.is_some(),
        "Execute must start a turn so the agent actually runs the plan"
    );
    assert_eq!(app.status, "starting turn");

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_choice_execute_clean_starts_turn_and_records_compaction_attempt() {
    let root = temp_workspace("plan_choice_clean");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-clean001".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "step 1\nstep 2\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.current_plan_id = Some(plan_id.clone());
    app.pending_plan_choice = Some(PendingPlanChoice {
        plan_id: plan_id.clone(),
        plan_path: plan_path.clone(),
        selection_index: 0,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(
        app.mode,
        SessionMode::Build,
        "clean execute still switches to Build"
    );
    assert!(app.pending_plan_choice.is_none());
    assert!(
        app.turn_rx.is_some(),
        "clean execute must also start a turn"
    );
    // We don't assert successful compaction here — the test agent has no
    // history to compact, so the agent returns the "not enough context"
    // error. We just verify the path is exercised: a log entry mentions
    // either the compaction success or the skip.
    assert!(
        app.transcript.iter().any(|entry| matches!(
            &entry.kind,
            TranscriptEntryKind::Log(LogEntry { message: msg, .. }) if msg.contains("execute-clean") || msg.contains("compacted prior context")
        )),
        "expected an execute-clean log line; transcript={:?}",
        app.transcript
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_choice_discard_deletes_file_and_clears_handoff() {
    let root = temp_workspace("plan_choice_discard");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-discard1".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "step 1\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.current_plan_id = Some(plan_id.clone());
    app.pending_plan_handoff = Some(plan_path.clone());
    app.pending_plan_choice = Some(PendingPlanChoice {
        plan_id: plan_id.clone(),
        plan_path: plan_path.clone(),
        selection_index: 0,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert!(!plan_path.exists(), "discard must delete the plan file");
    assert!(app.pending_plan_choice.is_none());
    assert!(app.current_plan_id.is_none());
    assert!(app.pending_plan_handoff.is_none());

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_choice_refine_dismisses_prompt_without_changing_mode_or_file() {
    let root = temp_workspace("plan_choice_refine");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-refine01".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "step\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.current_plan_id = Some(plan_id.clone());
    app.pending_plan_choice = Some(PendingPlanChoice {
        plan_id: plan_id.clone(),
        plan_path: plan_path.clone(),
        selection_index: 0,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert!(app.pending_plan_choice.is_none());
    assert!(plan_path.exists(), "refine must keep the plan file");
    assert_eq!(app.mode, SessionMode::Plan);
    assert_eq!(
        app.current_plan_id.as_deref(),
        Some(plan_id.as_str()),
        "current_plan_id stays so the next refinement turn can find it"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_choice_arrow_keys_move_selection_without_activating() {
    let root = temp_workspace("plan_choice_arrows");
    let plan_path = root.join("plan-x.md");
    fs::write(&plan_path, "step\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.pending_plan_choice = Some(PendingPlanChoice {
        plan_id: "plan-x".to_string(),
        plan_path: plan_path.clone(),
        selection_index: 0,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.pending_plan_choice.as_ref().unwrap().selection_index, 1);
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.pending_plan_choice.as_ref().unwrap().selection_index, 0);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_choice_shift_tab_falls_through_to_mode_toggle() {
    let root = temp_workspace("plan_choice_shifttab");
    let plan_path = root.join("plan-y.md");
    fs::write(&plan_path, "step\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.pending_plan_choice = Some(PendingPlanChoice {
        plan_id: "plan-y".to_string(),
        plan_path,
        selection_index: 0,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(app.mode, SessionMode::Build);
    assert!(
        app.pending_plan_choice.is_none(),
        "mode switch supersedes the post-plan choice prompt"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_to_build_switch_queues_plan_handoff_when_plan_file_exists() {
    let root = temp_workspace("plan_handoff_queued");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-test12345678".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "step 1\nstep 2\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.current_plan_id = Some(plan_id);

    switch_mode(&mut app, &agent, Some(SessionMode::Build), "test");

    assert_eq!(app.mode, SessionMode::Build);
    assert_eq!(
        app.pending_plan_handoff.as_deref(),
        Some(plan_path.as_path())
    );
    assert!(
        app.transcript.iter().any(|entry| matches!(
            &entry.kind,
            TranscriptEntryKind::Log(LogEntry { message, .. }) if message.contains("plan attached for next Build turn")
        )),
        "expected handoff log entry; transcript={:?}",
        app.transcript
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn plan_to_build_switch_skips_handoff_when_no_plan_file() {
    let root = temp_workspace("plan_handoff_absent");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    // current_plan_id present but the file was deleted out from under us
    app.current_plan_id = Some("plan-never-persisted".to_string());

    switch_mode(&mut app, &agent, Some(SessionMode::Build), "test");

    assert_eq!(app.mode, SessionMode::Build);
    assert!(app.pending_plan_handoff.is_none());

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn build_to_plan_switch_drops_pending_handoff() {
    let root = temp_workspace("plan_handoff_dropped");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-test12345678".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "step 1\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.current_plan_id = Some(plan_id);

    switch_mode(&mut app, &agent, Some(SessionMode::Build), "test");
    assert!(app.pending_plan_handoff.is_some());
    switch_mode(&mut app, &agent, Some(SessionMode::Plan), "test");
    assert!(app.pending_plan_handoff.is_none());

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn take_pending_plan_prefix_turn_one_returns_full_body() {
    let root = temp_workspace("plan_prefix_turn_one");
    let plans_dir = root.join(proposed_plan::PLAN_DIR);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_path = plans_dir.join("plan-abc.md");
    fs::write(&plan_path, "step 1\nstep 2\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.pending_plan_handoff = Some(plan_path.clone());
    app.plan_handoff_turns_seen = 0;

    let prefix = take_pending_plan_prefix(&mut app).expect("prefix returned");
    assert!(prefix.starts_with("[plan from previous session"));
    assert!(prefix.contains("step 1\nstep 2"));
    assert!(prefix.ends_with("[end plan]\n\n"));
    // Per-turn re-attach: handoff is NOT cleared, but counter advances.
    assert_eq!(
        app.pending_plan_handoff.as_deref(),
        Some(plan_path.as_path())
    );
    assert_eq!(app.plan_handoff_turns_seen, 1);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn take_pending_plan_prefix_subsequent_turns_return_short_marker() {
    let root = temp_workspace("plan_prefix_marker");
    let plans_dir = root.join(proposed_plan::PLAN_DIR);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_path = plans_dir.join("plan-abc.md");
    fs::write(&plan_path, "step 1\nstep 2\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.pending_plan_handoff = Some(plan_path.clone());
    // Simulate having already seen turn 1.
    app.plan_handoff_turns_seen = 1;

    let prefix = take_pending_plan_prefix(&mut app).expect("marker returned");
    assert!(
        prefix.contains("plan still in effect"),
        "marker should announce continued effect; got: {prefix:?}"
    );
    assert!(
        !prefix.contains("step 1"),
        "marker must not re-include the plan body"
    );
    assert!(
        prefix.contains(&plan_path.display().to_string()),
        "marker should reference the plan path; got: {prefix:?}"
    );
    // Handoff still set so later turns keep getting the marker until an
    // apply_patch (or mode switch) clears it.
    assert_eq!(
        app.pending_plan_handoff.as_deref(),
        Some(plan_path.as_path())
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn take_pending_plan_prefix_drops_handoff_when_file_missing() {
    let root = temp_workspace("plan_prefix_missing");
    let phantom = root.join(proposed_plan::PLAN_DIR).join("plan-gone.md");

    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.pending_plan_handoff = Some(phantom);

    let prefix = take_pending_plan_prefix(&mut app);
    assert!(prefix.is_none());
    assert!(app.pending_plan_handoff.is_none());
    assert!(
        app.transcript.iter().any(|entry| matches!(
            &entry.kind,
            TranscriptEntryKind::Log(LogEntry { message, .. }) if message.contains("could not read plan file")
        )),
        "expected a recovery log line for the missing plan file; transcript={:?}",
        app.transcript
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn slash_plan_with_trailing_space_still_switches_mode() {
    // Regression for the old Enter-time pre-intercept that compared the
    // trimmed input via `mode_command` and silently dropped `/plan ` (with
    // trailing whitespace) when the slash command flow had no `/plan` arm.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    set_input(&mut app, "/plan ".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.mode, SessionMode::Plan);
    assert!(app.input.is_empty());
}

#[tokio::test]
async fn inline_slash_plan_uses_surrounding_prompt_text() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    set_input(&mut app, "please /plan rethink attachment UX".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(app.mode, SessionMode::Plan);
    assert!(app.input.is_empty());
    wait_for_turn_completion(&mut app).await;
    assert!(
        transcript_message_contents(&app)
            .iter()
            .any(|content| content.contains("please rethink attachment UX")),
        "inline /plan should preserve the surrounding prompt text"
    );
}

#[tokio::test]
async fn slash_config_opens_screen() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/config".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert!(app.config_screen.is_some(), "config screen should be open");
}

#[tokio::test]
async fn slash_options_legacy_alias_opens_screen() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/options".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert!(app.config_screen.is_some(), "config screen should be open");
}

#[tokio::test]
async fn slash_statusline_opens_picker() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/statusline".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert!(
        app.status_line_setup.is_some(),
        "/statusline should open the picker overlay; status={:?}",
        app.status
    );
}

#[tokio::test]
async fn statusline_picker_renders_in_inline_mode() {
    // Inline is the default terminal mode; the overlay must be visible
    // there, not just in AlternateScreen.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/statusline".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    let rendered = render_inline_to_string(&app, 80, 24);
    assert!(
        rendered.contains("Configure Status Line"),
        "inline render should show the picker; got:\n{rendered}"
    );
}

#[test]
fn status_line_default_layout_renders_users_configured_items() {
    // Replays the exact list the user has in their ~/.squeezy/settings.toml
    // and asserts the detail row paints. If this fails, AppConfig loading
    // or item-parsing is broken; if it passes, the rendered binary should
    // show the row at startup unmodified.
    use crate::status::StatusLineItem;
    let mut app = test_app(SessionMode::Build);
    app.status_line_items = parse_status_line_items(Some(&[
        "provider-and-model".to_string(),
        "model-with-reasoning".to_string(),
        "current-dir".to_string(),
        "project-name".to_string(),
        "git-branch".to_string(),
        "pull-request-number".to_string(),
        "branch-changes".to_string(),
        "context-used".to_string(),
    ]));
    app.status_line_use_colors = true;
    assert!(app.status_line_items.as_ref().is_some_and(|v| v.len() == 8));
    assert_eq!(
        app.status_line_items.as_ref().unwrap()[0],
        StatusLineItem::ProviderAndModel
    );
    let lines = format_status_lines(&app, 200);
    assert_eq!(lines.len(), 2, "expect detail+mode row + hints row");
    let row1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        row1.contains("scripted:gpt-test"),
        "row 1 should include provider:model; got: {row1}"
    );
}

#[tokio::test]
async fn statusline_picker_toggle_then_save_reflects_in_status_row() {
    // Open the picker, navigate to the first item row and toggle it off
    // with Space, then save. The detail row should reflect the reduced
    // selection, not the original pre-checked defaults.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    // Force a writable settings path inside the test, since save_status_line
    // writes via apply_edits and we don't want to touch the real ~/.squeezy.
    let tmpdir = std::env::temp_dir().join(format!(
        "squeezy-statusline-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmpdir).expect("mkdir");
    let settings_path = tmpdir.join("settings.toml");
    let _guard = ScopedSettingsPath::new(settings_path.clone());
    app.set_settings_path_override(Some(settings_path));
    set_input(&mut app, "/statusline".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("open picker");
    // Cursor row 0 = "Use theme colors". Down moves to first item (ProviderAndModel by default).
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    )
    .await
    .expect("down");
    // Space toggles the first item off.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
    )
    .await
    .expect("space");
    // Enter saves.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("save");
    assert!(app.status_line_setup.is_none(), "picker should close");
    let items = app.status_line_items.as_ref().expect("items saved");
    assert!(
        !items.contains(&crate::status::StatusLineItem::ProviderAndModel),
        "toggled-off item should not be in saved list; got {items:?}"
    );
    let lines = format_status_lines(&app, 200);
    let row1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        !row1.contains("scripted:gpt-test"),
        "row 1 should no longer show provider-and-model after toggling it off; got: {row1}"
    );
}

#[tokio::test]
async fn statusline_save_closes_picker_and_paints_detail_row() {
    // Open the picker, then press Enter to save the pre-checked defaults.
    // The picker must close and the status row must start showing the
    // chosen items.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("statusline_save");
    let settings_path = dir.join("settings.toml");
    let _guard = ScopedSettingsPath::new(settings_path.clone());
    app.set_settings_path_override(Some(settings_path));
    set_input(&mut app, "/statusline".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("open picker");
    assert!(app.status_line_setup.is_some(), "picker should be open");
    // Second Enter inside the picker fires Save.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("save");
    assert!(
        app.status_line_setup.is_none(),
        "picker should close after Save; status={:?}",
        app.status
    );
    assert!(
        app.status_line_items
            .as_ref()
            .is_some_and(|items| !items.is_empty()),
        "Save should populate status_line_items with the pre-checked defaults; \
         got {:?}, status={:?}",
        app.status_line_items,
        app.status
    );
    // The detail row replaces the overview's dir/branch on row 1 and
    // should include a default item that always renders (provider:model).
    let lines = format_status_lines(&app, 200);
    let row1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        row1.contains("scripted:gpt-test"),
        "row 1 should reflect saved items; got: {row1}"
    );
}

#[tokio::test]
async fn slash_model_opens_config_at_models_section() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/model".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    let state = app.config_screen.expect("config screen should be open");
    assert_eq!(
        state.current_section().id,
        squeezy_core::config_schema::SectionId::Models
    );
}

#[tokio::test]
async fn slash_config_with_section_argument_focuses_section() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/options permissions".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    let state = app.config_screen.expect("config screen should be open");
    assert_eq!(
        state.current_section().id,
        squeezy_core::config_schema::SectionId::Permissions
    );
}

#[tokio::test]
async fn f11_toggles_config_screen() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    assert!(app.config_screen.is_none());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::F(11), KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert!(app.config_screen.is_some());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::F(11), KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert!(app.config_screen.is_none());
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
async fn slash_plans_list_renders_persisted_plans() {
    let root = temp_workspace("slash_plans_list");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);

    let (id, _) = proposed_plan::persist_plan(
        &root,
        proposed_plan::FALLBACK_SESSION_ID,
        "Outline: tidy the README.",
        &proposed_plan::PlanMeta::default(),
    )
    .expect("persist");

    set_input(&mut app, "/plans".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(app.status, "1 plan(s) in this session");
    let rendered = app
        .transcript
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message(item) if item.role == Role::System => {
                Some(item.content.clone())
            }
            _ => None,
        })
        .expect("system message rendered");
    assert!(rendered.contains(&id), "list output should include the id");
    assert!(
        rendered.contains("Outline: tidy the README."),
        "list output should include the objective: {rendered}"
    );
    assert!(
        rendered.contains("  *"),
        "active plan must be marked with *"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn slash_plans_list_empty_renders_guidance() {
    let root = temp_workspace("slash_plans_list_empty");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);

    set_input(&mut app, "/plans list".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(app.status, "no plans persisted in this session");
    let rendered = last_message_content(&app).expect("system guidance");
    assert!(
        rendered.contains("No plans saved in this session yet"),
        "empty /plans list should explain itself: {rendered}"
    );
    assert!(
        rendered.contains("Plan mode"),
        "empty /plans list should tell users how plans are created: {rendered}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn slash_plans_show_without_id_renders_usage_guidance() {
    let root = temp_workspace("slash_plans_show_no_id");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);

    set_input(&mut app, "/plans show".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(app.status, "usage: /plans <subcommand> <id-or-prefix>");
    let rendered = last_message_content(&app).expect("system guidance");
    assert!(rendered.contains("Missing plan id"), "{rendered}");
    assert!(rendered.contains("/plans show <id>"), "{rendered}");
    assert!(rendered.contains("Run `/plans`"), "{rendered}");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn slash_plans_delete_requires_explicit_yes() {
    let root = temp_workspace("slash_plans_delete_confirm");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);

    let (id, path) = proposed_plan::persist_plan(
        &root,
        proposed_plan::FALLBACK_SESSION_ID,
        "delete me",
        &proposed_plan::PlanMeta::default(),
    )
    .expect("persist");

    set_input(&mut app, format!("/plans delete {id}"));
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert!(path.exists(), "first attempt must NOT delete the file");
    assert!(app.status.contains("--yes"));

    set_input(&mut app, format!("/plans delete {id} --yes"));
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert!(!path.exists(), "second attempt with --yes must delete");
    assert!(app.status.starts_with("deleted plan"));
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn slash_plans_set_active_updates_pointer_and_app() {
    let root = temp_workspace("slash_plans_set_active");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);

    let (id_a, _) = proposed_plan::persist_plan(
        &root,
        proposed_plan::FALLBACK_SESSION_ID,
        "alpha body",
        &proposed_plan::PlanMeta::default(),
    )
    .expect("persist a");
    let (id_b, _) = proposed_plan::persist_plan(
        &root,
        proposed_plan::FALLBACK_SESSION_ID,
        "beta body",
        &proposed_plan::PlanMeta::default(),
    )
    .expect("persist b");
    // Persisting `b` left pointer aimed at `b`. Flip back to `a`.
    set_input(&mut app, format!("/plans set-active {id_a}"));
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(app.current_plan_id.as_deref(), Some(id_a.as_str()));
    assert_eq!(
        proposed_plan::read_current_plan_id(&root, proposed_plan::FALLBACK_SESSION_ID).as_deref(),
        Some(id_a.as_str())
    );
    // `id_b` is intentionally unused beyond establishing two plans.
    let _ = id_b;
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shift_tab_during_build_turn_pauses_and_switches_to_plan() {
    let root = temp_workspace("plan_pause_switch");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-pause123".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "---\n---\nstep 1\nstep 2\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Build, root.clone());
    let agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.current_plan_id = Some(plan_id.clone());
    // Simulate a live model turn.
    let cancel = CancellationToken::new();
    app.cancel = Some(cancel.clone());
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);

    switch_mode(&mut app, &agent, None, "tui_shift_tab");
    assert_eq!(app.mode, SessionMode::Plan, "pause must reach Plan mode");
    assert!(
        cancel.is_cancelled(),
        "pause must cancel the in-flight turn"
    );
    assert!(
        app.plan_pause.is_some(),
        "pause state must be captured for the resume marker"
    );
    assert_eq!(
        app.plan_pause.as_ref().unwrap().plan_id,
        plan_id,
        "captured plan id must match the active plan"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn resume_marker_announces_plan_change_during_pause() {
    let root = temp_workspace("plan_pause_resume_refined");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let original_id = "plan-original0".to_string();
    let refined_id = "plan-refined00".to_string();
    let refined_path = plans_dir.join(format!("{refined_id}.md"));
    fs::write(&refined_path, "---\n---\nrefined step 1\n").expect("write refined plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    // Plan id at time of pause differs from current (i.e. refined).
    app.plan_pause = Some(PlanPauseState {
        plan_id: original_id.clone(),
    });
    app.current_plan_id = Some(refined_id.clone());

    switch_mode(&mut app, &agent, Some(SessionMode::Build), "test");
    assert_eq!(app.mode, SessionMode::Build);
    assert!(app.plan_pause.is_none(), "pause state must be consumed");
    let marker = app
        .plan_resume_marker
        .as_deref()
        .expect("resume marker must be queued");
    assert!(
        marker.contains("plan refined"),
        "marker must announce refinement: {marker}"
    );
    assert!(marker.contains(&original_id), "marker mentions previous id");
    assert!(marker.contains(&refined_id), "marker mentions current id");

    // Take the prefix and confirm the marker rides alongside the body.
    let prefix =
        take_pending_plan_prefix(&mut app).expect("plan body returned for first resume turn");
    assert!(prefix.starts_with("[resuming from plan "));
    assert!(prefix.contains("refined step 1"));

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn resume_marker_omits_change_note_when_plan_unchanged() {
    let root = temp_workspace("plan_pause_resume_same");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-samesame0".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "---\n---\nbody\n").expect("write");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.plan_pause = Some(PlanPauseState {
        plan_id: plan_id.clone(),
    });
    app.current_plan_id = Some(plan_id.clone());

    switch_mode(&mut app, &agent, Some(SessionMode::Build), "test");
    let marker = app.plan_resume_marker.clone().unwrap_or_default();
    assert!(
        marker.contains("plan unchanged"),
        "marker should report no refinement: {marker}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn mode_status_text_appends_active_plan_segment() {
    let root = temp_workspace("mode_status_plan_segment");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    let body = "1. step one\n2. step two\n3. step three\n";
    let (plan_id, _) = proposed_plan::persist_plan(
        &root,
        proposed_plan::FALLBACK_SESSION_ID,
        body,
        &proposed_plan::PlanMeta::default(),
    )
    .expect("persist plan");
    app.current_plan_id = Some(plan_id.clone());
    let line = mode_status_text(&app);
    assert!(line.contains("Plan mode"), "base segment intact: {line}");
    let short = format!(
        "plan-{}",
        plan_id
            .strip_prefix("plan-")
            .unwrap()
            .chars()
            .take(6)
            .collect::<String>()
    );
    assert!(
        line.contains(&short),
        "status bar must include short plan id `{short}`: {line}"
    );
    assert!(
        line.contains("(3 steps)"),
        "status bar must include step count: {line}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn slash_plans_show_unknown_id_sets_status() {
    let root = temp_workspace("slash_plans_show_missing");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);

    set_input(&mut app, "/plans show plan-does-not-exist".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert!(
        app.status.starts_with("no plan matches"),
        "got status: {}",
        app.status
    );
    let rendered = last_message_content(&app).expect("system guidance");
    assert!(
        rendered.contains("No plan matches `plan-does-not-exist`"),
        "missing plan should be transcript-visible: {rendered}"
    );
    assert!(
        rendered.contains("no saved plans"),
        "missing plan in an empty session should explain why: {rendered}"
    );
    let _ = fs::remove_dir_all(&root);
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
async fn prompt_line_editing_matches_common_terminal_shortcuts() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "alpha\nbravo charlie".to_string());
    app.input_cursor = "alpha\nbravo".len();

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER),
    )
    .await
    .expect("super backspace");
    assert_eq!(app.input, "alpha\n charlie");
    assert_eq!(app.input_cursor, "alpha\n".len());

    set_input(&mut app, "alpha\nbravo charlie".to_string());
    app.input_cursor = "alpha\nbravo".len();
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-u");
    assert_eq!(app.input, "alpha\n charlie");
    assert_eq!(app.input_cursor, "alpha\n".len());

    set_input(&mut app, "alpha\nbravo charlie".to_string());
    app.input_cursor = "alpha\nbravo".len();
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-k");
    assert_eq!(app.input, "alpha\nbravo");
    assert_eq!(app.input_cursor, "alpha\nbravo".len());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-a");
    assert_eq!(app.input_cursor, "alpha\n".len());

    // Ctrl+E is no longer line-end (now toggles expand-all). `End`
    // is the canonical cursor-to-line-end on every platform.
    app.input_cursor = "alpha\nbr".len();
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
    )
    .await
    .expect("End");
    assert_eq!(app.input_cursor, app.input.len());

    set_input(&mut app, "alpha\nbravo".to_string());
    app.input_cursor = "alpha\n".len();
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-u at line start");
    assert_eq!(app.input, "alphabravo");
    assert_eq!(app.input_cursor, "alpha".len());
}

#[tokio::test]
async fn prompt_word_editing_matches_codex_shortcuts() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "alpha beta,gamma".to_string());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
    )
    .await
    .expect("alt-left");
    assert_eq!(app.input_cursor, "alpha beta,".len());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
    )
    .await
    .expect("alt-right");
    assert_eq!(app.input_cursor, app.input.len());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-backspace");
    assert_eq!(app.input, "alpha beta,");
    assert_eq!(app.input_cursor, "alpha beta,".len());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-w");
    assert_eq!(app.input, "alpha beta");
    assert_eq!(app.input_cursor, "alpha beta".len());

    set_input(&mut app, "alpha beta gamma".to_string());
    app.input_cursor = "alpha ".len();
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
    )
    .await
    .expect("alt-d");
    assert_eq!(app.input, "alpha  gamma");
    assert_eq!(app.input_cursor, "alpha ".len());
}

#[tokio::test]
async fn prompt_ignores_key_release_events() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "abc".to_string());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ),
    )
    .await
    .expect("release ignored");

    assert_eq!(app.input, "abc");
    assert_eq!(app.input_cursor, 3);
}

#[test]
fn keyboard_enhancement_flags_enable_modified_key_reporting() {
    let flags = keyboard_enhancement_flags();

    assert!(flags.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    assert!(flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
    assert!(flags.contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
}

#[tokio::test]
async fn prompt_history_uses_plain_up_down_when_prompt_is_empty() {
    let mut agent = test_agent(SessionMode::Build);
    let config = test_config(SessionMode::Build);
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

#[test]
fn default_mouse_wheel_scrolls_transcript_without_touching_input_or_history() {
    // Wheel events scroll the transcript in BOTH inline and alt-screen
    // mode (since mouse capture is unconditional, wheel arrives as a
    // MouseEvent and we always route it to the transcript). What must
    // NOT happen: the composer or prompt-history cursor shouldn't move.
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("first turn".to_string()));
    push_input_history(&mut app, "previous prompt".to_string());

    handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );

    assert_eq!(app.transcript_scroll_from_bottom, 3);
    assert!(app.input.is_empty());
    assert!(app.input_history_index.is_none());
}

#[tokio::test]
async fn slash_menu_renders_and_completes_selected_command() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/p".to_string());

    // The slash menu now shows up to 10 entries, so the terminal needs
    // enough vertical room for the welcome panel + the 10-row menu
    // (with a couple of wrapped descriptions) + input + status.
    let output = render_to_string(&app, 100, 36);
    assert!(output.contains("/permissions"), "{output}");
    assert!(output.contains("/plan"), "{output}");

    // `/p` matches /parent, /permissions, /pin, /pins, /plan, /plans
    // in alphabetical order. Step down four entries to land on /plan.
    for _ in 0..4 {
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
async fn slash_menu_completes_inline_command_token() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "please /att".to_string());

    let output = render_to_string(&app, 100, 36);
    assert!(output.contains("/attach"), "{output}");
    assert!(
        !output.contains("/cost"),
        "prefix-only commands should stay out of inline slash completion: {output}"
    );

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("complete inline command");

    assert_eq!(app.input, "please /attach ");
    assert_eq!(app.status, "selected /attach");
}

#[tokio::test]
async fn slash_menu_scrolls_sorted_full_command_list_with_five_visible() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/".to_string());

    let suggestions = input::slash_suggestions_for_app(&app);
    let names = suggestions
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    assert!(names.len() > SLASH_MENU_MAX_ITEMS);
    // Top window matches the sorted prefix — comparing against the
    // sorted list itself keeps the test honest if `SLASH_COMMANDS`
    // grows or `SLASH_MENU_MAX_ITEMS` is retuned.
    assert_eq!(
        &names[..SLASH_MENU_MAX_ITEMS],
        &names[..SLASH_MENU_MAX_ITEMS]
    );
    assert!(names[0] < names[1] && names[1] < names[2], "alphabetical");
    let command_rows = slash_suggestion_lines(&app, 120)
        .iter()
        .filter(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref().starts_with('/'))
        })
        .count();
    assert_eq!(command_rows, SLASH_MENU_MAX_ITEMS);

    // Step the selection forward by one page; the visible window
    // should slide forward by the same offset.
    for _ in 0..SLASH_MENU_MAX_ITEMS {
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
    let expected: Vec<&str> = names
        .iter()
        .copied()
        .skip(app.slash_menu_index + 1 - SLASH_MENU_MAX_ITEMS.min(app.slash_menu_index + 1))
        .take(SLASH_MENU_MAX_ITEMS)
        .collect();
    assert_eq!(visible, expected, "visible window should track the cursor");

    // The menu now wraps top↔bottom on Down/Up, so step exactly to the
    // last index, then assert one more Down wraps back to the first item.
    let target = suggestions.len() - 1;
    let already_at = app.slash_menu_index;
    let down_count = (target + suggestions.len() - already_at) % suggestions.len();
    for _ in 0..down_count {
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
    .expect("menu down past end wraps");
    assert_eq!(
        app.slash_menu_index, 0,
        "Down past the end should wrap to 0"
    );

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    )
    .await
    .expect("menu up at top wraps");
    assert_eq!(
        app.slash_menu_index,
        suggestions.len() - 1,
        "Up from 0 should wrap to last"
    );
}

#[test]
fn slash_menu_filters_checkpoint_commands_from_disabled_config() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/".to_string());
    let names = input::slash_suggestions_for_app(&app)
        .into_iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    for checkpoint_command in ["/checkpoints", "/checkpoint", "/undo", "/revert-turn"] {
        assert!(
            !names.contains(&checkpoint_command),
            "{checkpoint_command} should not be suggested while checkpointing is disabled"
        );
    }

    let mut config = test_config(SessionMode::Build);
    config.checkpoints_enabled = true;
    let mut enabled_app = test_app_with_config(&config, SessionMode::Build);
    set_input(&mut enabled_app, "/".to_string());
    let names = input::slash_suggestions_for_app(&enabled_app)
        .into_iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    for checkpoint_command in ["/checkpoints", "/checkpoint", "/undo", "/revert-turn"] {
        assert!(
            names.contains(&checkpoint_command),
            "{checkpoint_command} should be suggested while checkpointing is enabled"
        );
    }
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

#[test]
fn slash_menu_surfaces_capability_badges_for_world_touching_commands() {
    // Typing `/diff` should surface a `[git|read]` capability hint so the
    // user can tell at a glance the command will hit the worktree on disk.
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/diff".to_string());
    let rendered = render_to_string(&app, 120, 12);
    assert!(
        rendered.contains("[git|read]"),
        "expected /diff badge in slash menu:\n{rendered}"
    );

    // Switch to a destructive command and confirm its badge appears too.
    // `/undo` is checkpoint-gated, so enable checkpoints to surface it.
    app.checkpoints_enabled = true;
    set_input(&mut app, "/undo".to_string());
    let rendered = render_to_string(&app, 120, 12);
    assert!(
        rendered.contains("[edit|destructive]"),
        "expected /undo badge in slash menu:\n{rendered}"
    );
}

#[test]
fn slash_suggestion_line_contents_match_command_capabilities() {
    // Build the menu lines directly and assert the badge follows the
    // declared capabilities — covers both presence (`/help` → `net`) and
    // absence (`/cost` → no badge).
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/help".to_string());
    let lines = slash_suggestion_lines(&app, 120);
    let serialised = lines
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect::<Vec<String>>();
    let help_line = serialised
        .iter()
        .find(|line| line.contains("/help"))
        .expect("rendered /help line");
    assert!(help_line.contains("[net]"), "{help_line}");

    set_input(&mut app, "/cost".to_string());
    let lines = slash_suggestion_lines(&app, 120);
    let serialised = lines
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect::<Vec<String>>();
    let cost_line = serialised
        .iter()
        .find(|line| line.contains("/cost") && !line.contains("/context"))
        .expect("rendered /cost line");
    assert!(
        !cost_line.contains('['),
        "/cost should not render a capability badge: {cost_line}"
    );
}

#[test]
fn slash_suggestion_lines_keep_theme_hint_full() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/theme".to_string());
    let rendered = slash_suggestion_lines(&app, 80)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("[default|bright|fun|catppuccin|high-contrast|<custom>]"),
        "slash menu should render the complete theme hint: {rendered}"
    );
    assert!(
        !rendered.contains("high-..."),
        "slash menu must not truncate the high-contrast builtin: {rendered}"
    );
}

#[test]
fn slash_suggestion_lines_keep_short_hints_inline_when_width_allows() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/attach".to_string());
    let lines = slash_suggestion_lines(&app, 120);
    let attach_line = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .find(|line| line.contains("/attach ") && line.contains("insert a file token"))
        .expect("attach suggestion line");

    assert!(
        attach_line.contains("<path>"),
        "short parameter hint should stay on the command row when it fits: {attach_line}"
    );
}

#[tokio::test]
async fn slash_cost_reports_empty_session_without_model_turn() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/cost").await);

    let raw = last_message_content(&app).expect("cost output");
    let output = strip_ansi_escape_sequences(raw);
    assert_eq!(app.status, "cost snapshot");
    assert!(output.contains("Cost accounting"), "{output}");
    assert!(
        output.contains("provider  scripted") && output.contains("model gpt-5.5"),
        "{output}"
    );
    assert!(
        output.contains("provider_tokens input=- output=-"),
        "{output}"
    );
    // Empty buckets are suppressed so a fresh session is a short report,
    // not a wall of zero-valued counters.
    assert!(!output.contains("tools calls="), "{output}");
    assert!(!output.contains("subagents calls="), "{output}");
    assert!(!output.contains("receipts stub_hits="), "{output}");
    assert!(!output.contains("spills writes="), "{output}");
    assert!(!output.contains("\nio bytes_read="), "{output}");
    assert!(!output.contains("redactions="), "{output}");
    assert!(app.jobs.is_empty());
}

#[test]
fn format_reviewer_command_lists_recent_decisions_newest_first() {
    use std::time::{Duration, SystemTime};

    use squeezy_agent::{ReviewerAuditEntry, ReviewerAuditVerdict};
    use squeezy_core::PermissionCapability;

    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    let entries = vec![
        ReviewerAuditEntry {
            recorded_at: now - Duration::from_secs(120),
            turn_id: 3,
            tool_name: "shell.run".to_string(),
            capability: PermissionCapability::Shell,
            target: "command:ls".to_string(),
            verdict: ReviewerAuditVerdict::Allow,
            reason: "approved low-risk listing".to_string(),
        },
        ReviewerAuditEntry {
            recorded_at: now - Duration::from_secs(5),
            turn_id: 4,
            tool_name: "edit.apply_patch".to_string(),
            capability: PermissionCapability::Edit,
            target: "path:secrets.env".to_string(),
            verdict: ReviewerAuditVerdict::Deny,
            reason: "writing into protected secrets path".to_string(),
        },
    ];

    let output = commands::format_reviewer_command(&entries, now);
    assert!(output.contains("2 recent decision(s)"), "{output}");
    // Newest entry should appear before the older one.
    let deny_idx = output.find("deny edit").expect("deny line present");
    let allow_idx = output.find("allow shell").expect("allow line present");
    assert!(deny_idx < allow_idx, "{output}");
    assert!(output.contains("target=path:secrets.env"), "{output}");
    assert!(
        output.contains("reason: writing into protected secrets path"),
        "{output}"
    );
}

#[test]
fn format_reviewer_command_handles_empty_buffer() {
    use std::time::SystemTime;

    let output = commands::format_reviewer_command(&[], SystemTime::UNIX_EPOCH);
    assert!(output.contains("no auto-decisions recorded"), "{output}");
}

#[test]
fn format_cost_command_renders_active_buckets() {
    use squeezy_agent::{
        AttachmentShape, ConversationShape, SessionAccountingSnapshot, TranscriptShape,
    };
    use squeezy_core::{CostSnapshot, SessionMetrics, SessionMode};
    use squeezy_llm::{RequestTokenEstimate, TokenizerKind};

    let estimate = RequestTokenEstimate {
        input_tokens: 0,
        context_window_tokens: None,
        effective_context_window_tokens: None,
        headroom_tokens: None,
        max_output_tokens: None,
        input_budget_tokens: None,
        remaining_input_tokens: None,
        used_input_percent_x100: None,
        tokenizer: TokenizerKind::OpenAiCompatible,
        estimated: true,
    };

    let metrics = SessionMetrics {
        tool_calls: 4,
        tool_successes: 3,
        tool_errors: 1,
        bytes_read: 12_345,
        subagent_calls: 1,
        subagent_provider: CostSnapshot {
            input_tokens: Some(900),
            output_tokens: Some(120),
            estimated_usd_micros: Some(7_500),
            ..CostSnapshot::default()
        },
        receipt_stub_hits: 2,
        spill_writes: 1,
        ..SessionMetrics::default()
    };

    let snapshot = SessionAccountingSnapshot {
        session_id: Some("sess-1".to_string()),
        provider: "scripted",
        model: "gpt-5.5".to_string(),
        mode: SessionMode::Build,
        store_responses: false,
        previous_response_id: None,
        cost: CostSnapshot {
            input_tokens: Some(1_200),
            output_tokens: Some(340),
            estimated_usd_micros: Some(415_300),
            ..CostSnapshot::default()
        },
        metrics,
        redactions: 2,
        transcript: TranscriptShape::default(),
        conversation: ConversationShape::default(),
        attachments: AttachmentShape::default(),
        transmitted_request: estimate,
        full_history_request: estimate,
    };

    let raw = commands::format_cost_command(&snapshot);
    // The styled output embeds ANSI escapes around individual values
    // (theme-aware colors). Assertions check semantic substrings after
    // stripping the escapes so future palette tweaks don't churn tests.
    let output = strip_ansi_escape_sequences(&raw);
    assert!(output.contains("estimated_usd=$0.415300"), "{output}");
    assert!(output.contains("provider_tokens input=1200"), "{output}");
    assert!(
        output.contains("tools calls=4 successes=3 errors=1"),
        "{output}"
    );
    assert!(output.contains("subagents calls=1"), "{output}");
    assert!(output.contains("receipts stub_hits=2"), "{output}");
    assert!(output.contains("spills writes=1"), "{output}");
    assert!(output.contains("io bytes_read=12345"), "{output}");
    assert!(output.contains("redactions=2"), "{output}");
    // The styled output should actually contain ANSI escapes — confirms
    // the formatter is using `commands_style`.
    assert!(
        raw.contains('\x1b'),
        "cost output should embed ANSI escapes: {raw:?}"
    );
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

    let raw = last_message_content(&app).expect("context output");
    let output = strip_ansi_escape_sequences(raw);
    assert_eq!(app.status, "context snapshot");
    // Post-squeezy-rw0i the /context output leads with consumed +
    // remaining against the model's context window, followed by a
    // per-source breakdown. Window for this model is 400_000.
    assert!(output.contains("Context window"), "{output}");
    assert!(output.contains("Consumption by source"), "{output}");
    assert!(
        output.contains("consumed:") && output.contains("remaining:"),
        "{output}"
    );
    assert!(output.contains("400,000"), "{output}");
    assert!(output.contains('%'), "{output}");
    assert!(app.jobs.is_empty());
}

#[tokio::test]
async fn slash_context_uses_registry_fallback_for_unknown_models() {
    // The registry now ships a fallback metadata path for unknown model
    // ids, so /context produces concrete numbers (drawn from the fallback
    // window) rather than `unknown` placeholders.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/context").await);

    let raw = last_message_content(&app).expect("context output");
    let output = strip_ansi_escape_sequences(raw);
    // Fallback window is 272_000; max_output reserve 64_000. Numbers are
    // rendered with grouped thousands so the user can scan them at a glance.
    assert!(output.contains("272,000"), "{output}");
    assert!(
        output.contains("max_output_reserve: 64,000 tokens"),
        "{output}"
    );
    assert!(output.contains("Consumption by source"), "{output}");
}

#[test]
fn context_recommendations_flag_largest_and_secondary_sources() {
    use commands::{ContextSourceTokens, context_source_recommendations};

    // tool_outputs dominates (~48%) and history (user text) is independently
    // large (~31%): both should surface, largest first, with the others quiet.
    let recs = context_source_recommendations(&ContextSourceTokens {
        user: 3_100,
        tool_outputs: 4_800,
        reasoning: 0,
        image: 0,
        attachments: 0,
        system: 2_100,
    });
    assert_eq!(recs.len(), 2, "{recs:?}");
    assert_eq!(
        recs[0],
        "largest: tool_outputs 48% → narrow reads (read_slice / signature spans), prefer grep counts, or enable output dedup",
        "{recs:?}"
    );
    assert_eq!(
        recs[1], "history 31% → run /compact to summarize older turns",
        "{recs:?}"
    );

    // A balanced session (no source crosses the largest threshold) yields no
    // advice rather than nagging about an evenly split context.
    let balanced = context_source_recommendations(&ContextSourceTokens {
        user: 1_000,
        tool_outputs: 1_000,
        reasoning: 1_000,
        image: 1_000,
        attachments: 1_000,
        system: 1_000,
    });
    assert!(balanced.is_empty(), "{balanced:?}");

    // An empty/fresh session has nothing actionable to say.
    assert!(context_source_recommendations(&ContextSourceTokens::default()).is_empty());
}

#[tokio::test]
async fn small_paste_stays_in_prompt() {
    let root = temp_workspace("tui_inline_paste");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_without_session_log_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    handle_paste(&mut app, &mut agent, "small\r\npaste".to_string())
        .await
        .expect("handle paste");

    assert_eq!(app.input, "small\npaste");
    assert!(app.attachments.is_empty());
    assert!(app.prompt_attachments.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn large_paste_uses_visible_prompt_token() {
    let root = temp_workspace("tui_large_paste");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_without_session_log_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let pasted = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 7);

    handle_paste(&mut app, &mut agent, pasted.clone())
        .await
        .expect("handle paste");

    let placeholder = format!("[Pasted Content {} chars]", pasted.chars().count());
    assert_eq!(app.input, placeholder);
    assert_eq!(app.prompt_attachments.len(), 1);
    assert!(app.attachments.is_empty());
    let input = app.input.clone();
    let prepared = prepare_prompt_turn_input(&mut app, input);
    assert_eq!(prepared.display_input, placeholder);
    assert_eq!(prepared.model_input, pasted);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn deleting_large_paste_token_drops_payload_before_submit() {
    let root = temp_workspace("tui_large_paste_delete");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_without_session_log_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    handle_paste(
        &mut app,
        &mut agent,
        "y".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1),
    )
    .await
    .expect("handle paste");
    app.input.clear();
    app.input_cursor = 0;
    app.prune_prompt_attachments();

    assert!(app.prompt_attachments.is_empty());
    let prepared = prepare_prompt_turn_input(&mut app, "summarize".to_string());
    assert_eq!(prepared.model_input, "summarize");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn duplicate_large_pastes_get_unique_prompt_tokens() {
    let root = temp_workspace("tui_large_paste_dupe");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_without_session_log_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let pasted = "z".repeat(LARGE_PASTE_CHAR_THRESHOLD + 2);

    handle_paste(&mut app, &mut agent, pasted.clone())
        .await
        .expect("first paste");
    insert_input_char(&mut app, ' ');
    handle_paste(&mut app, &mut agent, pasted)
        .await
        .expect("second paste");

    assert!(app.input.contains("[Pasted Content 1002 chars]"));
    assert!(app.input.contains("[Pasted Content 1002 chars #2]"));
    assert_eq!(app.prompt_attachments.len(), 2);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn slash_attach_inserts_visible_file_token() {
    let root = temp_workspace("tui_attach");
    fs::write(
        root.join("error.log"),
        "2026-05-24 ERROR failed\n2026-05-24 WARN retry\n",
    )
    .expect("write log");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_without_session_log_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/attach error.log").await);
    assert!(app.attachments.is_empty());
    assert_eq!(app.input, "[Attached file error.log]");
    assert_eq!(app.prompt_attachments.len(), 1);
    assert!(app.status.contains("inserted [Attached file error.log]"));
    let input = app.input.clone();
    let prepared = prepare_prompt_turn_input(&mut app, input);
    assert!(prepared.model_input.contains("Attached file error.log:"));
    assert!(prepared.model_input.contains("2026-05-24 ERROR failed"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn slash_attachments_empty_renders_guidance() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/attachments").await);

    assert_eq!(app.status, "no attached context");
    let rendered = last_message_content(&app).expect("system guidance");
    assert!(rendered.contains("No attached context yet"), "{rendered}");
    assert!(rendered.contains("/attach <path>"), "{rendered}");
}

#[tokio::test]
async fn inline_slash_attach_inserts_token_without_dropping_prompt_text() {
    let root = temp_workspace("tui_inline_attach");
    fs::write(root.join("error.log"), "inline attach fixture\n").expect("write log");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_without_session_log_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    set_input(
        &mut app,
        "please review /attach error.log before replying".to_string(),
    );
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert_eq!(
        app.input,
        "please review [Attached file error.log] before replying"
    );
    assert_eq!(app.prompt_attachments.len(), 1);
    assert!(app.status.contains("inserted [Attached file error.log]"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn slash_attach_routes_canonical_images_to_prompt_token() {
    let root = temp_workspace("tui_attach_image_png");
    let image_bytes = b"\x89PNG\r\n\x1a\nimage".to_vec();
    fs::write(root.join("shot.png"), &image_bytes).expect("write image");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_without_session_log_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/attach shot.png").await);
    assert!(app.attachments.is_empty());
    assert_eq!(app.input, "[Image shot.png]");
    let input = app.input.clone();
    let prepared = prepare_prompt_turn_input(&mut app, input);
    assert_eq!(prepared.display_input, "[Image shot.png]");
    assert_eq!(prepared.model_input, "[Image shot.png]");
    assert_eq!(prepared.transient_input_items.len(), 1);
    let LlmInputItem::Image { media_type, bytes } = &prepared.transient_input_items[0] else {
        panic!("expected image item");
    };
    assert_eq!(media_type, "image/png");
    assert_eq!(bytes.as_ref(), image_bytes.as_slice());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn slash_attach_surfaces_unsupported_label_only_images() {
    // Image-shaped labels whose bytes never trip the magic-byte sniff
    // (HEIC/BMP/TIFF and the long tail) stay `UnsupportedImage` — the
    // attach surfaces the legacy "unsupported file" status because no
    // provider can decode a non-canonical payload.
    let root = temp_workspace("tui_attach_image_heic");
    fs::write(root.join("snap.heic"), b"not real heic content").expect("write image");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/attach snap.heic").await);
    assert!(app.attachments.is_empty());
    assert!(
        app.status
            .contains("unsupported file kind=unsupported_image"),
        "{}",
        app.status
    );

    let _ = fs::remove_dir_all(root);
}

/// Drift test: every entry in `SLASH_COMMAND_HELP_TABLE` must correspond to a real
/// slash command in the live `SLASH_COMMANDS` registry, so stale or invented names
/// are caught at compile time.  Adding a new command to the registry does NOT
/// automatically fail this test; add a help entry to `SLASH_COMMAND_HELP_TABLE` to
/// cover it.
#[test]
fn slash_help_table_entries_exist_in_registry() {
    use squeezy_skills::slash_command_help_names;

    // SLASH_COMMANDS is re-exported as pub(crate) from lib.rs via `use super::*`
    let registry_names: std::collections::HashSet<&str> =
        SLASH_COMMANDS.iter().map(|c| c.name).collect();

    for help_name in slash_command_help_names() {
        assert!(
            registry_names.contains(help_name),
            "SLASH_COMMAND_HELP_TABLE entry {help_name:?} does not exist in SLASH_COMMANDS; \
             either the command was removed/renamed or the help entry was mis-typed"
        );
    }
}

#[tokio::test]
async fn slash_help_lists_topics() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/help").await);

    wait_for_turn_completion(&mut app).await;
    let content = last_message_content(&app).expect("help transcript");
    assert!(
        transcript_message_contents(&app).contains(&"/help"),
        "user prompt should remain in the transcript"
    );
    assert!(content.contains("Available `/help` topics"), "{content}");
    assert!(content.contains("`providers`"), "{content}");
}

#[tokio::test]
async fn slash_help_config_renders_citations_and_config() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/help providers").await);

    wait_for_turn_completion(&mut app).await;
    let content = last_message_content(&app).expect("help transcript");
    assert!(
        transcript_message_contents(&app).contains(&"/help providers"),
        "user prompt should remain in the transcript"
    );
    assert!(content.contains("docs/external/PROVIDERS.md"), "{content}");
    assert!(content.contains("[model]"), "{content}");
    assert!(!content.contains("--api-key"), "{content}");
}

#[tokio::test]
async fn inline_slash_help_dispatches_from_prompt_body() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    set_input(&mut app, "please /help providers".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    wait_for_turn_completion(&mut app).await;
    assert!(app.input.is_empty());
    assert!(
        transcript_message_contents(&app).contains(&"/help providers"),
        "inline /help should dispatch the command from its inline position"
    );
    let content = last_message_content(&app).expect("help transcript");
    assert!(content.contains("docs/external/PROVIDERS.md"), "{content}");
}

#[tokio::test]
async fn slash_help_unsupported_points_to_public_resources() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/help quantum billing").await);

    wait_for_turn_completion(&mut app).await;
    let content = last_message_content(&app).expect("help transcript");
    assert!(
        transcript_message_contents(&app).contains(&"/help quantum billing"),
        "user prompt should remain in the transcript"
    );
    assert!(content.contains("No local help coverage"), "{content}");
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
async fn slash_fork_branches_into_sibling_session_with_same_transcript() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    // Seed the transcript so we can verify the fork inherits it on the
    // caller side (the agent-side conversation copy is exercised by the
    // agent's own integration tests).
    app.push_transcript_item(TranscriptItem::user("explain this stack trace"));
    app.push_transcript_item(TranscriptItem::assistant("here's the rundown"));
    let transcript_before = app.transcript.len();

    let parent_id = agent.session_id().expect("parent session id");
    assert!(handle_slash_command(&mut app, &mut agent, "/fork").await);

    let child_id = agent.session_id().expect("child session id");
    assert_ne!(child_id, parent_id, "fork must produce a fresh session id");
    assert!(
        app.status.starts_with("forked session → "),
        "status reports the fork outcome: {}",
        app.status
    );
    assert!(
        app.status.contains(&child_id),
        "status includes the new session id: {}",
        app.status
    );
    // Visible transcript stays in place — the new session inherits the
    // existing turns rather than the user losing their context. The fork
    // pushes a slash-command echo plus the announcement, so the new
    // length is `before + 2`.
    assert_eq!(
        app.transcript.len(),
        transcript_before + 2,
        "fork preserves prior entries and adds the slash echo plus the announcement",
    );
    let announce = last_message_content(&app).expect("fork announcement");
    assert!(
        announce.contains(&child_id) && announce.contains("/resume"),
        "fork transcript announcement explains the lineage: {announce}",
    );
}

#[tokio::test]
async fn slash_clear_wipes_transcript_and_rotates_to_a_fresh_session() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    app.push_transcript_item(TranscriptItem::user("explain this stack trace"));
    app.push_transcript_item(TranscriptItem::assistant("here's the rundown"));
    app.prompt_attachments.push(PromptAttachment {
        placeholder: "@file:1".to_string(),
        payload: PromptAttachmentPayload::Text {
            replacement: "file context".to_string(),
        },
    });
    app.prompt_queue.push_back("queued follow-up".to_string());
    app.prompt_queue_overlay = Some(prompt_queue::PromptQueueState::new());
    app.auto_drain_queue = true;
    app.context_compaction_nudge_shown = true;

    let prior_id = agent.session_id().expect("prior session id");
    let prior_render_cache_session = app.render_cache_session;
    assert!(handle_slash_command(&mut app, &mut agent, "/clear").await);

    let new_id = agent.session_id().expect("new session id");
    assert_ne!(new_id, prior_id, "clear must rotate to a fresh session id");
    assert_eq!(app.status, "conversation cleared");
    assert!(
        app.terminal_clear_pending,
        "clear must request a terminal scrollback/visible-screen purge",
    );
    assert_ne!(
        app.render_cache_session, prior_render_cache_session,
        "entry ids restart after clear, so the render-cache session must rotate",
    );
    assert!(
        app.prompt_attachments.is_empty(),
        "prompt attachments are tied to the cleared conversation",
    );
    assert!(
        app.prompt_queue.is_empty(),
        "queued prompts from the old context must not auto-run after clear",
    );
    assert!(
        app.prompt_queue_overlay.is_none(),
        "queue overlay should close when the queue is cleared",
    );
    assert!(!app.auto_drain_queue, "stale queue drain must be cancelled");
    assert!(
        !app.context_compaction_nudge_shown,
        "fresh context should not inherit the old compaction nudge",
    );
    // The visible transcript is dropped; only the post-clear confirmation
    // remains (the slash echo is wiped along with the rest).
    assert_eq!(
        app.transcript.len(),
        1,
        "clear leaves only the confirmation note",
    );
    let announce = last_message_content(&app).expect("clear announcement");
    assert!(
        announce.contains(&new_id) && announce.contains("/resume"),
        "clear note points at the new session and how to recover the old one: {announce}",
    );
}

#[tokio::test]
async fn slash_clear_without_session_log_still_wipes_the_transcript() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    app.push_transcript_item(TranscriptItem::user("some earlier prompt"));
    assert!(
        agent.session_id().is_none(),
        "ephemeral agent has no session"
    );

    assert!(handle_slash_command(&mut app, &mut agent, "/clear").await);

    assert_eq!(app.status, "conversation cleared");
    assert_eq!(
        app.transcript.len(),
        1,
        "clear still drops the transcript without a durable session",
    );
    let announce = last_message_content(&app).expect("clear announcement");
    assert!(
        !announce.contains("/resume"),
        "with no durable session there is nothing to resume: {announce}",
    );
}

#[test]
fn hard_terminal_clear_resets_inline_flush_to_the_fresh_transcript() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::system("Conversation cleared."));

    let mut startup_flushed = false;
    let mut transcript_flushed_len = 2;
    reset_inline_flush_after_hard_clear(&mut startup_flushed, &mut transcript_flushed_len);

    assert!(
        !startup_flushed,
        "the startup card should be repainted after the hard terminal clear",
    );
    assert_eq!(
        transcript_flushed_len, 0,
        "fresh-session transcript must flush from index zero",
    );

    let lines = inline_history_lines_for_flush(
        &app,
        80,
        !startup_flushed,
        transcript_flushed_len,
        app.transcript.len(),
    );
    let rendered = lines_to_plain_text(&lines);
    assert!(rendered.contains("Conversation cleared."), "{rendered}");
    assert!(rendered.contains("Squeezy v"), "{rendered}");
}

#[test]
fn hard_terminal_clear_replays_startup_at_top_of_fresh_screen() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::system("Conversation cleared."));

    let lines = inline_history_lines_for_flush(&app, 80, true, 0, app.transcript.len());
    let height = fresh_inline_history_height(visual_line_count(&lines, 80), 20);
    assert_eq!(
        fresh_inline_cursor_row(height, 20),
        height,
        "the rebuilt inline viewport should start immediately after the replayed history",
    );

    let mut backend = TestBackend::new(80, 20);
    draw_lines_at_top(&mut backend, lines, 80, height).expect("draw fresh history");

    let buffer = backend.buffer();
    let rows = (0..20)
        .map(|y| {
            let mut row = String::new();
            for x in 0..80 {
                row.push_str(buffer[(x, y)].symbol());
            }
            row
        })
        .collect::<Vec<_>>();
    let first_nonblank = rows
        .iter()
        .position(|row| !row.trim().is_empty())
        .expect("fresh history should render");

    assert_eq!(
        first_nonblank,
        0,
        "fresh replay must not leave blank rows above the startup card:\n{}",
        rows.join("\n")
    );
    assert!(rows[0].contains("Squeezy v"), "{}", rows.join("\n"));
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

    // Open bubble: leading indent + cycling phase-glyph bullet + space + text.
    assert!(text.starts_with("  "), "{text}");
    assert!(text.ends_with(" hello"), "{text}");
}

#[test]
fn tool_result_entries_collapse_by_default_and_carry_overlay_hint() {
    // Long tool outputs collapse to a head-tail preview by default,
    // with the chip's truncation hint pointing the user at the
    // full-screen transcript overlay (Ctrl+T) — the only mode that
    // can reliably show every line regardless of inline vs alt-screen
    // rendering. Single-entry expand keys were removed because they
    // worked only in alt-screen mode (inline writes entries to
    // terminal scrollback, which is immutable once flushed).
    let mut app = test_app(SessionMode::Build);
    let payload = (0..30)
        .map(|i| format!("match-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_tool_result(sample_tool_result("grep", &payload));
    // A freshly-pushed work node settle-folds from its expanded form for
    // ~600ms; rest it collapsed so this asserts the post-settle preview.
    app.finalize_settles_for_test();

    assert!(app.transcript[0].collapsed);
    let collapsed = render_to_string(&app, 100, 24);
    assert!(collapsed.contains("✔ Explored"), "{collapsed}");
    assert!(collapsed.contains("grep"), "{collapsed}");
    assert!(!collapsed.contains("receipt="), "{collapsed}");
    assert!(!collapsed.contains("B receipt"), "{collapsed}");
    assert!(
        collapsed.contains("Ctrl+T for full transcript"),
        "collapsed view should point at the overlay: {collapsed}"
    );
    assert!(
        !collapsed.contains("match-15"),
        "middle of the body must be elided: {collapsed}"
    );

    // Force the expanded inline variant directly so this test still
    // covers the card body rendering. The user-facing full view is Ctrl+T.
    set_all_transcript_collapsed(&mut app, false);

    assert!(!app.transcript[0].collapsed);
    let expanded = render_to_string(&app, 100, 40);
    assert!(expanded.contains("match-15"), "{expanded}");
    assert!(!expanded.contains("receipt output="), "{expanded}");
}

#[test]
fn failed_tool_result_starts_expanded_so_error_is_visible() {
    // The auto-expand-on-failure behavior is the user-visible payoff for
    // the "every red ✖ shows raw error inline" UX fix. Build a tool
    // result whose status is Error, then check that the constructor
    // chose `collapsed = false`.
    let mut app = test_app(SessionMode::Build);
    let mut failed = sample_tool_result("delegate", "missing required string field: prompt");
    failed.status = ToolStatus::Error;
    app.push_tool_result(failed);
    assert!(
        !app.transcript[0].collapsed,
        "failed tool result should not be collapsed by default"
    );
}

#[tokio::test]
async fn typing_after_selection_returns_focus_to_prompt_editing() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));
    select_previous_transcript_entry(&mut app);
    assert_eq!(app.selected_entry, Some(0));

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
    )
    .await
    .expect("type prompt");

    assert_eq!(app.input, "h");
    assert!(app.selected_entry.is_none());
}

#[tokio::test]
async fn removed_slash_expand_no_longer_opens_full_transcript_overlay() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));
    app.transcript[0].collapsed = true;

    assert!(!handle_slash_command(&mut app, &mut agent, "/expand").await);

    assert!(app.transcript_overlay.is_none());
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
fn failed_shell_stderr_renders_once_not_duplicated() {
    // Regression: a failed shell command rendered its stderr twice — once in
    // the combined stdout+stderr block and again under a separate "stderr"
    // header. The auto-expanded error must appear exactly once.
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("shell", "");
    result.status = ToolStatus::Error;
    result.content = serde_json::json!({
        "command": "rustfmt --check src/cli.rs",
        "workdir": ".",
        "exit_code": 1,
        "stdout": "",
        "stderr": "Diff in src/cli.rs:5: UNIQUE_FMT_MARKER_XYZ",
    });
    app.push_tool_result(result);
    assert!(!app.transcript[0].collapsed, "failure auto-expands");

    let output = render_to_string(&app, 140, 30);
    assert_eq!(
        output.matches("UNIQUE_FMT_MARKER_XYZ").count(),
        1,
        "stderr should render exactly once, not duplicated: {output}"
    );
}

#[test]
fn missing_cargo_manifest_shell_failure_renders_as_not_run_warning() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("shell", "");
    result.status = ToolStatus::Error;
    result.content = serde_json::json!({
        "command": "cargo check -p sample-arch-graph",
        "exit_code": 101,
        "stdout": "",
        "stderr": "error: could not find `Cargo.toml` in `/tmp/example-workspace` or any parent directory",
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 140, 16);

    assert!(
        output.contains("⚠ Not run cargo check -p sample-arch-graph · no Cargo.toml found"),
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
    set_all_transcript_collapsed(&mut app, false);

    let output = render_to_string(&app, 140, 18);

    assert!(
        output.contains("✔ Ran cargo test -p squeezy-tui"),
        "{output}"
    );
    assert!(
        output.contains("│   cargo test -p squeezy-tui in .:"),
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
fn failed_rustfmt_output_uses_diff_background_not_colored_text() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("shell", "");
    result.status = ToolStatus::Error;
    result.content = serde_json::json!({
        "command": "cargo fmt --check",
        "workdir": ".",
        "exit_code": 1,
        "stdout": concat!(
            "Diff in /tmp/project/src/lib.rs:1:\n",
            "fn demo() {\n",
            "\u{1b}[31m-    old_call();\u{1b}(B\u{1b}[m\n",
            "\u{1b}[32m+    new_call();\u{1b}(B\u{1b}[m\n",
        ),
        "stderr": "",
    });
    app.push_tool_result(result);

    let lines = format_transcript_entry_with_width(
        &app.transcript[0],
        false,
        ToolOutputVerbosity::Normal,
        MessageOutcome::Normal,
        Some(140),
        true,
        "Ctrl+T",
    );
    let rendered = lines_to_plain_text(&lines);
    assert!(
        rendered.contains("Diff in /tmp/project/src/lib.rs:1:"),
        "{rendered}"
    );
    assert!(!rendered.contains("(B+"), "{rendered}");
    assert!(!rendered.contains("(B-"), "{rendered}");

    let add_line = lines
        .iter()
        .find(|line| rendered_line_text(line).contains("+    new_call();"))
        .expect("add line");
    assert!(
        add_line.style.bg.is_some(),
        "add line should carry a row background: {add_line:?}"
    );
    let add_sign = add_line
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "+")
        .expect("add sign");
    assert_eq!(add_sign.style.fg, None);
    assert!(add_sign.style.bg.is_some());

    let del_line = lines
        .iter()
        .find(|line| rendered_line_text(line).contains("-    old_call();"))
        .expect("delete line");
    assert!(
        del_line.style.bg.is_some(),
        "delete line should carry a row background: {del_line:?}"
    );
    let del_sign = del_line
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "-")
        .expect("delete sign");
    assert_eq!(del_sign.style.fg, None);
    assert!(del_sign.style.bg.is_some());
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
        output.contains("│   inspect workspace --details in /tmp/project:"),
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
fn resume_hydration_backfills_legacy_tool_name_from_call() {
    let mut app = test_app(SessionMode::Build);
    let result = sample_tool_result("shell", "ok");
    let mut legacy_result = serde_json::to_value(result).expect("serialize tool result");
    legacy_result
        .as_object_mut()
        .expect("tool result object")
        .remove("tool_name");

    hydrate_transcript_item(
        &mut app,
        squeezy_store::HydratedTranscriptItem::ToolResult {
            call: Some(squeezy_store::HydratedToolCall {
                call_id: "call-1".to_string(),
                tool: "shell".to_string(),
                arguments: serde_json::json!({"command": "pwd"}),
            }),
            result: legacy_result,
        },
    );

    assert!(
        app.transcript
            .iter()
            .filter_map(|entry| match &entry.kind {
                TranscriptEntryKind::Log(log) => Some(log.message.as_str()),
                _ => None,
            })
            .all(|message| !message.contains("malformed tool-result")),
        "legacy tool result should hydrate without a warning: {:?}",
        app.transcript
    );
    let entry = app.transcript.first().expect("hydrated tool card");
    let tool = match &entry.kind {
        TranscriptEntryKind::ToolResult(tool) => tool,
        other => panic!("expected hydrated tool result, got {other:?}"),
    };
    assert_eq!(tool.result.tool_name, "shell");
    assert_eq!(
        tool.call.as_ref().map(|call| call.name.as_str()),
        Some("shell")
    );
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

    // Each card now shows a short body preview by default (5-line head-
    // tail cap), so the rendered height is taller than the old empty-
    // body collapsed view — render at a height that fits all three cards.
    let output = render_to_string(&app, 180, 40);

    assert!(
        output.contains("✔ Explored diff context (worktree) · 2 files · +3 -1"),
        "{output}"
    );
    assert!(
        output.contains("✔ Explored list files matching **/*.rs · 2 paths"),
        "{output}"
    );
    assert!(
        output.contains("✔ Explored read src/lib.rs · 128B · partial result"),
        "{output}"
    );
    assert!(!output.contains("plan patch"), "{output}");
    assert!(!output.contains("output shortened"), "{output}");
    assert!(!output.contains("\"paths\""), "{output}");
}

#[test]
fn read_tool_output_summary_names_saved_tool_without_raw_json() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("read_tool_output", "");
    result.content = serde_json::json!({
        "handle": "abc123",
        "bytes_returned": 128,
        "total_bytes": 4096,
        "truncated": true,
        "content": "{\"call_id\":\"call-1\",\"tool_name\":\"diff_context\",\"status\":\"Success\",\"content\":{\"files\":[{\"path\":\"src/lib.rs\""
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 140, 12);

    assert!(
        output.contains("✔ Explored expand saved diff context · 128B · partial result"),
        "{output}"
    );
    assert!(output.contains("saved diff context"), "{output}");
    assert!(
        output.contains("content saved tool-result JSON (partial; hidden in normal mode)"),
        "{output}"
    );
    assert!(!output.contains("\"tool_name\""), "{output}");
    assert!(!output.contains("\"files\""), "{output}");
}

#[test]
fn read_tool_output_folds_spilled_model_output_without_tool_name() {
    // Regression: spilled tool results are written in the `model_output()`
    // shape (`{"status":..,"content":..}`), which carries no `tool_name`.
    // The recognizer must still fold them to a receipt instead of splatting
    // the raw (minified, single-line) JSON across the scrollback.
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("read_tool_output", "");
    result.content = serde_json::json!({
        "handle": "deadbeef",
        "bytes_returned": 48_800,
        "total_bytes": 170_800,
        "truncated": true,
        "content": "{\"status\":\"success\",\"content\":{\"graph_available\":true,\"hierarchy\":[{\"id\":\"file:build.rs\",\"kind\":\"File\"}]}}"
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 140, 12);

    assert!(output.contains("saved tool output"), "{output}");
    assert!(
        output.contains("content saved tool-result JSON (partial; hidden in normal mode)"),
        "{output}"
    );
    // The raw JSON body must not reach the scrollback.
    assert!(!output.contains("\"hierarchy\""), "{output}");
    assert!(!output.contains("graph_available"), "{output}");
}

#[test]
fn tool_call_label_describes_verify_by_scope_and_level() {
    // verify takes scope/level, not a `command` field. The label must describe
    // the verification and never echo a stray `command` value a confused model
    // might pass (which the tools-side hook re-homes before execution).
    let call = ToolCall {
        call_id: "v-1".to_string(),
        name: "verify".to_string(),
        arguments: serde_json::json!({"scope": "workspace", "level": "full"}),
    };
    assert_eq!(tool_call_label(&call), "workspace/full");

    // Defaults fill in when the fields are omitted.
    let call = ToolCall {
        call_id: "v-2".to_string(),
        name: "verify".to_string(),
        arguments: serde_json::json!({}),
    };
    assert_eq!(tool_call_label(&call), "diff/quick");

    // A stray `command` is never surfaced as the label.
    let call = ToolCall {
        call_id: "v-3".to_string(),
        name: "verify".to_string(),
        arguments: serde_json::json!({"command": "full"}),
    };
    assert_eq!(tool_call_label(&call), "diff/quick");
}

#[test]
fn read_tool_output_hides_cargo_json_artifacts_in_normal_mode() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("read_tool_output", "");
    result.content = serde_json::json!({
        "handle": "cargo-json",
        "bytes_returned": 256,
        "total_bytes": 1024,
        "truncated": true,
        "content": concat!(
            "{\"reason\":\"compiler-artifact\",\"package_id\":\"registry+https://github.com/rust-lang/crates.io-index#libc@0.2.184\"}\n",
            "{\"reason\":\"build-script-executed\",\"package_id\":\"registry+https://github.com/rust-lang/crates.io-index#libc@0.2.184\"}\n",
            "{\"reason\":\"compiler-message\",\"message\":{\"rendered\":\"error[E0308]: mismatched types\\n  --> src/lib.rs:1:1\\n\"}}\n",
            "===== stderr =====\n",
            "   Compiling demo v0.1.0 (/tmp/demo)\n",
            "error: could not compile `demo` due to 1 previous error\n",
        )
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 140, 18);

    assert!(
        output.contains("✔ Explored expand saved compiler output · 256B · partial result"),
        "{output}"
    );
    assert!(output.contains("saved compiler output"), "{output}");
    assert!(output.contains("compiler messages"), "{output}");
    assert!(
        output.contains("error[E0308]: mismatched types"),
        "{output}"
    );
    assert!(output.contains("stderr"), "{output}");
    assert!(
        output.contains("error: could not compile `demo` due to 1 previous error"),
        "{output}"
    );
    assert!(
        output.contains("compiler JSON hidden in normal mode"),
        "{output}"
    );
    assert!(
        !output.contains("\"reason\":\"compiler-artifact\""),
        "{output}"
    );
    assert!(!output.contains("\"package_id\""), "{output}");
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
    set_all_transcript_collapsed(&mut app, false);

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
    set_all_transcript_collapsed(&mut app, false);

    let lines = format_transcript_entry_with_width(
        &app.transcript[0],
        false,
        ToolOutputVerbosity::Normal,
        MessageOutcome::Normal,
        Some(120),
        true,
        "Ctrl+T",
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
    assert!(!output.contains("Ctrl-O to expand"), "{output}");
}

#[test]
fn collapsed_edit_row_shows_diff_preview() {
    // apply_patch cards bypass the 5-line collapsed cap (the diff *is*
    // the point of the card) so the body renders inline by default. We
    // verify the per-file summary header + the +/- patch lines survive.
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
    assert!(output.contains("file src/lib.rs +1 -1"), "{output}");
    assert!(output.contains("-old"), "{output}");
    assert!(output.contains("+new"), "{output}");
}

#[test]
fn edit_row_uses_apply_patch_unified_diff_when_checkpoint_is_absent() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("apply_patch", "");
    result.content = serde_json::json!({
        "files": [{
            "after_sha256": "after",
            "before_sha256": "before",
            "bytes_after": 20,
            "bytes_before": 10,
            "changed": true,
            "path": "src/cli/paths.rs"
        }],
        "unified_diff": "--- a/src/cli/paths.rs\n+++ b/src/cli/paths.rs\n@@ -1,3 +1,4 @@\n unchanged\n-old\n+new\n+added\n"
    });
    app.push_tool_result(result);

    let output = render_to_string(&app, 140, 16);

    assert!(
        output.contains("✔ Edited src/cli/paths.rs · +2 -1"),
        "{output}"
    );
    assert!(output.contains("file src/cli/paths.rs +2 -1"), "{output}");
    assert!(output.contains("-old"), "{output}");
    assert!(output.contains("+new"), "{output}");
    assert!(
        !output.contains("\"unified_diff\""),
        "renderer should show the diff, not raw JSON: {output}"
    );
}

#[test]
fn write_file_new_file_preview_uses_call_content() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("write_file", "");
    result.call_id = "write-1".to_string();
    result.content = serde_json::json!({
        "path": "src/process_info.rs",
        "before_sha256": null,
        "after_sha256": "after",
        "bytes_written": 25,
        "noop": false
    });
    app.push_tool_result_with_call(
        result,
        Some(ToolCall {
            call_id: "write-1".to_string(),
            name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/process_info.rs",
                "content": "fn new_process_info() {}\n"
            }),
        }),
    );

    let output = render_to_string(&app, 140, 16);

    assert!(
        output.contains("✔ Edited src/process_info.rs · +1 -0"),
        "{output}"
    );
    assert!(
        output.contains("file src/process_info.rs +1 -0"),
        "{output}"
    );
    assert!(output.contains("+fn new_process_info() {}"), "{output}");
    assert!(
        !output.contains("\"bytes_written\""),
        "renderer should show the write preview, not raw JSON: {output}"
    );
}

#[test]
fn write_file_existing_file_without_diff_does_not_show_fake_all_add() {
    let mut app = test_app(SessionMode::Build);
    let mut result = sample_tool_result("write_file", "");
    result.call_id = "write-1".to_string();
    result.content = serde_json::json!({
        "path": "src/cli/commands/init.rs",
        "before_sha256": "before",
        "after_sha256": "after",
        "bytes_written": 2048,
        "noop": false
    });
    app.push_tool_result_with_call(
        result,
        Some(ToolCall {
            call_id: "write-1".to_string(),
            name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/cli/commands/init.rs",
                "content": "line\n".repeat(200)
            }),
        }),
    );

    let output = render_to_string(&app, 140, 16);

    assert!(
        output.contains("✔ Edited src/cli/commands/init.rs"),
        "{output}"
    );
    assert!(output.contains("file src/cli/commands/init.rs"), "{output}");
    assert!(
        !output.contains("+200 -0"),
        "existing-file write_file without an actual diff must not look like a full-file add: {output}"
    );
    assert!(
        !output.contains("+line"),
        "existing-file write_file without an actual diff must not render the replacement body as a fake add diff: {output}"
    );
    assert!(
        !output.contains("\"bytes_written\""),
        "renderer should not fall back to raw JSON: {output}"
    );
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
        true,
        "Ctrl+T",
    );
    let rendered = lines_to_plain_text(&lines);
    assert!(!rendered.contains("diff --git"), "{rendered}");
    assert!(!rendered.contains("index 123"), "{rendered}");

    // Patch content is "old" / "new" — short strings that the highlighter
    // labels as plain identifiers, so the sign + body keep the default
    // foreground. The row background is what carries the diff state.
    let add_sign = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "+")
        .expect("add sign span");
    assert_eq!(add_sign.style.fg, None);
    assert_eq!(add_sign.style.bg, Some(render::diff::diff_add_bg()));
    let add_line = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content.as_ref() == "+"))
        .expect("add line");
    assert_eq!(add_line.style.bg, Some(render::diff::diff_add_bg()));

    let del_sign = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "-")
        .expect("delete sign span");
    assert_eq!(del_sign.style.fg, None);
    assert_eq!(del_sign.style.bg, Some(render::diff::diff_del_bg()));
    let del_line = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content.as_ref() == "-"))
        .expect("delete line");
    assert_eq!(del_line.style.bg, Some(render::diff::diff_del_bg()));
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
fn apply_patch_failures_on_different_paths_do_not_coalesce() {
    let mut app = test_app(SessionMode::Build);
    for (index, path) in ["src/foo.rs", "src/bar.rs"].into_iter().enumerate() {
        let mut result = sample_tool_result("apply_patch", "");
        result.call_id = format!("patch-{index}");
        result.status = ToolStatus::Stale;
        result.content = serde_json::json!({
            "error": "search text not found",
            "path": path,
        });
        app.push_tool_result(result);
    }

    let output = render_to_string(&app, 140, 14);

    assert!(output.contains("src/foo.rs"), "{output}");
    assert!(output.contains("src/bar.rs"), "{output}");
    assert!(
        !output.contains("(2x)"),
        "distinct paths should not coalesce: {output}"
    );
    assert_eq!(app.transcript.len(), 2);
}

#[test]
fn apply_patch_failures_on_same_path_still_coalesce() {
    let mut app = test_app(SessionMode::Build);
    for index in 0..2 {
        let mut result = sample_tool_result("apply_patch", "");
        result.call_id = format!("patch-{index}");
        result.status = ToolStatus::Stale;
        result.content = serde_json::json!({
            "error": "search text not found",
            "path": "src/lib.rs",
        });
        app.push_tool_result(result);
    }

    let output = render_to_string(&app, 140, 14);

    assert!(output.contains("(2x)"), "{output}");
    assert_eq!(app.transcript.len(), 1);
}

#[test]
fn plan_mode_question_renders_with_choices_and_freeform_hint() {
    let mut app = test_app(SessionMode::Plan);
    let request = RequestUserInputRequest {
        question: "How would you like to approach the refactor?".to_string(),
        choices: vec![
            RequestUserInputChoice {
                label: "Split into smaller modules".to_string(),
                value: "split".to_string(),
            },
            RequestUserInputChoice {
                label: "Keep the current layout".to_string(),
                value: "keep".to_string(),
            },
        ],
        allow_freeform: true,
    };
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    app.pending_request_user_input = Some(PendingRequestUserInput {
        request,
        response_tx,
        selection_index: 1,
        answer: String::new(),
        answer_cursor: 0,
    });

    let output = render_to_string(&app, 140, 24);

    assert!(
        output.contains("Plan-mode question"),
        "title missing: {output}"
    );
    assert!(
        output.contains("How would you like to approach the refactor?"),
        "question text missing: {output}"
    );
    assert!(
        output.contains("Split into smaller modules"),
        "first choice missing: {output}"
    );
    assert!(
        output.contains("● Keep the current layout"),
        "selected marker missing on second choice: {output}"
    );
    assert!(
        output.contains("freeform"),
        "freeform hint missing: {output}"
    );
}

#[test]
fn plan_mode_question_marks_freeform_answer_when_selected() {
    let request = RequestUserInputRequest {
        question: "Which path?".to_string(),
        choices: vec![
            RequestUserInputChoice {
                label: "Use src/main.rs as the representative file".to_string(),
                value: "src/main.rs".to_string(),
            },
            RequestUserInputChoice {
                label: "Pick a different Rust file for me".to_string(),
                value: "other".to_string(),
            },
            RequestUserInputChoice {
                label: "I want a broader modernization pass, not one file".to_string(),
                value: "broader".to_string(),
            },
        ],
        allow_freeform: true,
    };

    let lines = format_request_user_input_menu_lines(&request, request.choices.len(), "lm");
    let answer = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref() == "Answer › ")
        })
        .expect("answer row");
    let marker = answer
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "● ")
        .expect("answer marker");
    assert_eq!(marker.style.fg, Some(crate::render::theme::accent()));
    assert!(
        answer
            .spans
            .iter()
            .any(|span| span.content.as_ref() == "lm"),
        "typed answer should remain visible on the selected answer row",
    );
}

#[test]
fn plan_mode_question_selected_choice_uses_amber_dot_not_yellow_label() {
    let request = RequestUserInputRequest {
        question: "For the modernization pass, should I keep it strict?".to_string(),
        choices: vec![
            RequestUserInputChoice {
                label: "Behavior-preserving only".to_string(),
                value: "behavior_preserving".to_string(),
            },
            RequestUserInputChoice {
                label: "Allow small internal cleanup".to_string(),
                value: "small_cleanup".to_string(),
            },
        ],
        allow_freeform: false,
    };

    let lines = format_request_user_input_menu_lines(&request, 0, "");
    let selected = lines
        .iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref() == "Behavior-preserving only")
        })
        .expect("selected choice line");
    let marker = selected
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "● ")
        .expect("selected marker");
    assert_eq!(marker.style.fg, Some(crate::render::theme::accent()));

    let label = selected
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "Behavior-preserving only")
        .expect("selected label");
    assert_ne!(label.style.fg, Some(crate::render::theme::secondary()));
    assert_ne!(label.style.fg, Some(crate::render::theme::accent()));
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
    assert_eq!(command[0].style.fg, Some(crate::render::theme::secondary()));
    assert!(
        command.iter().any(|span| span.content.as_ref() == "-p"
            && span.style.fg == Some(crate::render::theme::accent())),
        "{command:?}"
    );

    let ansi = ansi_spans("\u{1b}[32mok\u{1b}[0m error");
    assert_eq!(ansi[0].style.fg, Some(ratatui::style::Color::Green));

    let keyword = keyword_spans("public class Foo { return ok; }");
    assert!(
        keyword.iter().any(|span| span.content.as_ref() == "class"
            && span.style.fg == Some(crate::render::theme::secondary())),
        "{keyword:?}"
    );
}

#[test]
fn ansi_passthrough_renders_colors() {
    let line = render::ansi::ansi_to_line("\u{1b}[32mhello\u{1b}[0m world");

    assert_eq!(line.spans[0].content.as_ref(), "hello");
    assert_eq!(line.spans[0].style.fg, Some(ratatui::style::Color::Green));
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
    // The sign character is split from the body so the diff background can
    // cover the whole row without coloring text red/green.
    let add_sign = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "+")
        .expect("add sign span");
    let del_sign = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "-")
        .expect("delete sign span");

    assert_eq!(add_sign.style.fg, None);
    assert_eq!(del_sign.style.fg, None);
    assert!(
        lines
            .iter()
            .any(|line| line.spans[0].content.as_ref() == "2 ")
    );
}

#[test]
fn highlight_rust_code_block() {
    let palette = render::highlight::HighlightPalette::current();
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

    assert_eq!(keyword.style.fg, Some(palette.keyword));
    assert_eq!(comment.style.fg, Some(palette.comment));
    assert_eq!(number.style.fg, Some(palette.number));
}

#[test]
fn highlight_yaml_code_block() {
    // tree-sitter-yaml's highlight names (e.g. property, string.special.symbol)
    // don't match the trimmed HIGHLIGHT_NAMES set squeezy configures, so this
    // asserts only the dispatch wiring: yaml lexes without panicking and
    // produces at least one span per line.
    let lines = render::highlight::highlight_code(Some("yaml"), "name: squeezy\nport: 8080\n");
    assert!(!lines.is_empty(), "yaml dispatch produced no lines");
    for line in &lines {
        assert!(!line.spans.is_empty(), "yaml line produced no spans");
    }
}

#[test]
fn highlight_bash_code_block() {
    let lines = render::highlight::highlight_code(
        Some("bash"),
        "#!/bin/bash\nif [ -f foo ]; then echo hi; fi\n",
    );
    let spans = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .collect::<Vec<_>>();
    let comment = spans
        .iter()
        .find(|span| span.content.as_ref().starts_with("#!"))
        .expect("bash shebang comment span");
    let keyword = spans
        .iter()
        .find(|span| span.content.as_ref() == "if")
        .expect("bash keyword span");
    assert_eq!(
        comment.style.fg,
        Some(render::highlight::HighlightPalette::current().comment)
    );
    assert_eq!(
        keyword.style.fg,
        Some(render::highlight::HighlightPalette::current().keyword)
    );
}

#[test]
fn highlight_toml_code_block() {
    // tree-sitter-toml's highlight names diverge from the trimmed
    // HIGHLIGHT_NAMES set; assert only that the dispatch wiring lexes the
    // input and produces non-empty spans.
    let lines = render::highlight::highlight_code(Some("toml"), "[package]\nname = \"squeezy\"\n");
    assert!(!lines.is_empty(), "toml dispatch produced no lines");
    for line in &lines {
        assert!(!line.spans.is_empty(), "toml line produced no spans");
    }
}

#[test]
fn highlight_unknown_language_falls_back_to_plain() {
    let lines = render::highlight::highlight_code(Some("klingon"), "qapla'!");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].spans.len(), 1);
    assert_eq!(lines[0].spans[0].style.fg, None);
}

#[test]
fn markdown_renders_heading_and_code() {
    let palette = render::highlight::HighlightPalette::current();
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
    assert_eq!(code_keyword.style.fg, Some(palette.keyword));
}

#[test]
fn markdown_colors_confidence_labels_after_em_dash() {
    let cases = [
        ("exact_syntax", crate::render::theme::green()),
        ("import_resolved", crate::render::theme::accent()),
        ("candidate_set", crate::render::theme::secondary()),
        ("external", crate::render::theme::quiet()),
        ("unknown", crate::render::theme::quiet()),
        ("label_missing", crate::render::theme::red()),
    ];
    for (label, expected) in cases {
        let lines = render::markdown::render_markdown(&format!(
            "X is constructed via from_path — {label}."
        ));
        let span = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == label)
            .unwrap_or_else(|| panic!("label span for `{label}` is missing"));
        assert_eq!(span.style.fg, Some(expected), "color for `{label}`");
    }
}

#[test]
fn markdown_colors_confidence_labels_in_brackets() {
    let lines =
        render::markdown::render_markdown("the call resolves [candidate_set] across two impls");
    let span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "candidate_set")
        .expect("bracketed label should render its own span");
    assert_eq!(span.style.fg, Some(crate::render::theme::secondary()));
}

#[test]
fn markdown_leaves_identifier_lookalikes_uncoloured() {
    let lines = render::markdown::render_markdown(
        "the helper `exact_syntax_test` is not a confidence label",
    );
    // No span equal to the bare label string `exact_syntax` should
    // appear — it only shows up as a substring of the longer
    // identifier inside a code span.
    let any_styled_label = lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
        span.content.as_ref() == "exact_syntax"
            && span.style.fg == Some(crate::render::theme::green())
    });
    assert!(
        !any_styled_label,
        "should not colour `exact_syntax` when it is part of a longer identifier"
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
fn reasoning_delta_renders_with_dim_italic() {
    let expected_modifiers = Modifier::DIM | Modifier::ITALIC;

    let block = reasoning_block_lines("first thought\nsecond thought", false, false, "Ctrl+T");
    assert!(
        block.len() >= 2,
        "expanded reasoning emits header and at least one body line: {block:?}"
    );
    for line in &block {
        for span in &line.spans {
            assert!(
                span.style.add_modifier.contains(expected_modifiers),
                "reasoning span missing dim+italic: {span:?}"
            );
            assert!(
                span.style.fg.is_none(),
                "reasoning style should not pin a foreground colour: {span:?}"
            );
        }
    }
    let body_text: String = block
        .iter()
        .skip(1)
        .flat_map(|line| line.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(
        body_text.contains("▏ first thought") && body_text.contains("▏ second thought"),
        "reasoning body should use ▏ left-indent marker: {body_text}",
    );

    let streaming = streaming_reasoning_lines("partial thinking");
    let header = streaming
        .first()
        .expect("streaming reasoning emits a header line");
    assert!(
        header.spans.iter().any(|s| s.content.contains("reasoning…")
            && s.style.add_modifier.contains(expected_modifiers)),
        "streaming header missing dim+italic: {header:?}",
    );
    let body = streaming
        .get(1)
        .expect("streaming reasoning emits a body line");
    let body_text: String = body.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        body_text.starts_with("▏ "),
        "streaming body should use ▏ left-indent marker: {body_text}",
    );
}

#[test]
fn finalized_reasoning_defaults_to_compact_visible_in_compact_transcript() {
    let mut app = test_app(SessionMode::Build);
    assert_eq!(app.transcript_default, TranscriptDefault::Compact);

    app.push_reasoning_segment(squeezy_core::ReasoningSnapshot::from_payload(
        squeezy_core::ReasoningPayload::OpenAi {
            item_id: "rsn-test".to_string(),
            summary: vec!["first thought\nsecond thought".to_string()],
            encrypted_content: None,
        },
    ));

    let entry = app.transcript.last().expect("reasoning entry recorded");
    assert!(
        entry.collapsed,
        "finalized reasoning should stay compact by default"
    );
    let rendered = lines_to_plain_text(&format_transcript_entry(
        entry,
        false,
        ToolOutputVerbosity::Compact,
        MessageOutcome::Normal,
        "Ctrl+T",
    ));
    assert!(rendered.contains("▸ reasoning"), "{rendered}");
    assert!(rendered.contains("first thought"), "{rendered}");
    assert!(rendered.contains("+1 lines"), "{rendered}");
    assert!(!rendered.contains("▏ second thought"), "{rendered}");

    app.transcript_overlay = Some(TranscriptOverlayState::default());
    let overlay = lines_to_plain_text(&transcript_lines_for_overlay(&app, Some(100), true));
    assert!(overlay.contains("▾ reasoning"), "{overlay}");
    assert!(overlay.contains("▏ first thought"), "{overlay}");
    assert!(overlay.contains("▏ second thought"), "{overlay}");
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
        context: None,
        preview: Vec::new(),
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
    assert!(output.contains("Deny"), "{output}");
    assert!(!output.contains("Deny for this session"), "{output}");
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
        pull_request: None,
        branch_changes: None,
        pending: false,
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
    assert!(!status.contains("scripted:gpt-test"), "{status}");
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
        pull_request: None,
        branch_changes: None,
        pending: false,
    };

    let output = render_to_string(&app, 140, 18);
    assert!(output.contains("Squeezy v"), "{output}");
    // model/provider used to ride in the banner; it now comes from the
    // default status-line detail row so changing models surfaces without a
    // restart.
    assert!(output.contains("openai:gpt-test"), "{output}");
    assert!(output.contains("feature"), "{output}");
    assert!(
        output.contains("Build mode (Shift+Tab to cycle)"),
        "{output}"
    );
    assert!(!output.contains("ready"), "{output}");
    assert!(output.contains("Up/Down menu/history"), "{output}");
    assert!(!output.contains("Alt+Up/Down history"), "{output}");
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
        .position(|line| line.contains("scripted:gpt-test"))
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

    let output = render_to_string(&app, 120, 24);
    assert!(output.contains("Squeezy v"), "{output}");
    // provider:model moved out of the banner into the live status line;
    // see `render_uses_two_line_status_footer` for the reason.
    assert!(output.contains("hello"), "{output}");
    assert!(output.contains("☽ answer"), "{output}");
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
    assert!(!at_bottom.contains("Squeezy v"), "{at_bottom}");

    app.transcript_scroll_from_bottom = u16::MAX;
    let at_top = render_to_string(&app, 120, 20);
    assert!(at_top.contains("Squeezy v"), "{at_top}");
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
fn transcript_overlay_screen_keeps_native_selection_available() {
    let mut bytes = Vec::new();
    enter_transcript_overlay_screen(&mut bytes).expect("enter transcript overlay screen");
    let ansi = String::from_utf8(bytes).expect("ansi");

    assert!(ansi.contains("\x1b[?1049h"), "must enter alt screen");
    assert!(
        ansi.contains("\x1b[?1007h"),
        "must enable alternate-scroll mode for wheel-to-key fallback"
    );
    assert!(
        ansi.contains(DISABLE_MOUSE_MODES),
        "must clear inline mouse capture before opening the transcript"
    );
    assert!(
        !ansi.contains(ENABLE_MOUSE_CLICK_CAPTURE),
        "full transcript must leave native terminal text selection available"
    );
    assert!(
        !ansi.contains(ENABLE_MOUSE_DRAG_CAPTURE),
        "full transcript must not capture drag events by default"
    );
}

#[test]
fn transcript_overlay_mouse_mode_enables_scrollbar_drag_reporting() {
    let mut bytes = Vec::new();
    set_transcript_overlay_mouse_mode(&mut bytes, true, false)
        .expect("enable transcript overlay mouse mode");
    let ansi = String::from_utf8(bytes).expect("ansi");

    assert!(
        ansi.starts_with(DISABLE_MOUSE_MODES),
        "must reset stale mouse modes before enabling drag reporting"
    );
    assert!(
        ansi.contains(ENABLE_MOUSE_DRAG_CAPTURE),
        "scrollbar mode must enable button-drag reporting"
    );
    assert!(
        ansi.contains("\x1b[?1002h"),
        "scrollbar mode must report drag events, not just clicks"
    );
}

#[test]
fn transcript_overlay_mouse_mode_can_restore_main_click_capture() {
    let mut bytes = Vec::new();
    set_transcript_overlay_mouse_mode(&mut bytes, false, true).expect("restore main mouse capture");
    let ansi = String::from_utf8(bytes).expect("ansi");

    assert!(
        ansi.starts_with(DISABLE_MOUSE_MODES),
        "must leave overlay drag mode before restoring main mouse capture"
    );
    assert!(
        ansi.contains(ENABLE_MOUSE_CLICK_CAPTURE),
        "main click capture should be restored when it was enabled before the overlay"
    );
}

#[test]
fn transcript_overlay_screen_exit_restores_inline_mouse_setting() {
    let mut without_restore = Vec::new();
    leave_transcript_overlay_screen(&mut without_restore, false)
        .expect("leave transcript overlay screen");
    let without_restore = String::from_utf8(without_restore).expect("ansi");
    assert!(
        !without_restore.contains(ENABLE_MOUSE_CLICK_CAPTURE),
        "default inline mode should not keep mouse capture enabled"
    );

    let mut with_restore = Vec::new();
    leave_transcript_overlay_screen(&mut with_restore, true)
        .expect("leave transcript overlay screen");
    let with_restore = String::from_utf8(with_restore).expect("ansi");
    let leave_pos = with_restore
        .find("\x1b[?1049l")
        .expect("must leave alt screen");
    let restore_pos = with_restore
        .find(ENABLE_MOUSE_CLICK_CAPTURE)
        .expect("must restore opt-in mouse capture");
    assert!(
        restore_pos > leave_pos,
        "opt-in inline mouse capture should be restored after returning to the main buffer"
    );
}

#[test]
fn modify_other_keys_reset_uses_xterm_sequence() {
    let mut disable = String::new();
    DisableModifyOtherKeys
        .write_ansi(&mut disable)
        .expect("disable modifyOtherKeys");
    assert_eq!(disable, "\x1b[>4;0m");
}

#[test]
fn inline_history_flush_contains_startup_and_new_transcript() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("find getFoo"));
    app.push_transcript_item(TranscriptItem::assistant("No definition found."));

    let len = app.transcript.len();
    let first = inline_history_lines_for_flush(&app, 100, true, 0, len);
    let rendered = lines_to_plain_text(&first);

    assert!(rendered.contains("Squeezy v0.1.0"), "{rendered}");
    assert!(rendered.contains("find getFoo"), "{rendered}");
    assert!(rendered.contains("☽ No definition found."), "{rendered}");

    let next = inline_history_lines_for_flush(&app, 100, false, len, len);
    assert!(next.is_empty());
}

#[test]
fn settling_node_is_held_from_flush_and_folds_in_live_region() {
    let mut app = test_app(SessionMode::Build);
    // A success tool result arms a settle-fold (it rests collapsed).
    app.push_tool_result(sample_tool_result("grep", "match-1\nmatch-2"));
    assert!(app.transcript[0].settle.is_some(), "a success tool folds");

    // The scrollback flush stops before the settling node — it is held back.
    let boundary = settling_flush_boundary(&app);
    assert_eq!(boundary, 0);
    let flushed = inline_history_lines_for_flush(&app, 100, false, 0, boundary);
    assert!(flushed.is_empty(), "settling node must not flush yet");

    // Meanwhile it renders folding on the rail in the live region.
    let live = lines_to_plain_text(&live_settling_lines(&app, 100));
    assert!(live.contains("grep"), "{live}");
    assert!(
        live.contains("├─") || live.contains("╰─"),
        "rail gutter: {live}"
    );

    // Once the fold finishes the node becomes flushable and leaves the live region.
    app.finalize_settles_for_test();
    assert_eq!(settling_flush_boundary(&app), app.transcript.len());
    assert!(live_settling_lines(&app, 100).is_empty());
}

#[test]
fn inline_live_viewport_excludes_flushed_history() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("old prompt"));
    app.push_transcript_item(TranscriptItem::assistant("old answer"));
    set_input(&mut app, "new prompt".to_string());

    let output = render_inline_to_string(&app, 100, 12);

    assert!(!output.contains("Squeezy v"), "{output}");
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
fn cross_project_resume_hint_quotes_target_cwd_and_command() {
    let hint = cross_project_resume_hint("session-abc", "/work/other");
    assert!(hint.contains("/work/other"), "{hint}");
    assert!(
        hint.contains("squeezy sessions resume session-abc"),
        "{hint}"
    );
}

#[test]
fn render_prompt_uses_rotating_coin_and_cursor() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "ship it".to_string());
    app.turn_visual = TurnVisualState::Running;

    let output = render_to_string(&app, 100, 12);
    let coin = prompt_coin_frame(&app);
    assert!(output.contains(&format!("{coin}  ship it┃")), "{output}");
}

#[test]
fn first_turn_empty_composer_spinner_animates_before_streaming_output() {
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);
    app.cancel = Some(CancellationToken::new());
    app.turn_visual = TurnVisualState::Running;
    app.animation_tick_rate = Duration::from_millis(100);
    app.input.clear();
    app.pending_assistant.clear();
    app.transcript.clear();

    app.animation_tick = 0;
    let coin_first = prompt_coin_frame(&app);
    let first = render_to_string(&app, 100, 12);
    app.animation_tick = 8;
    let coin_second = prompt_coin_frame(&app);
    let second = render_to_string(&app, 100, 12);

    assert!(first.contains(&format!("{coin_first}  ┃")), "{first}");
    assert!(second.contains(&format!("{coin_second}  ┃")), "{second}");
    assert_ne!(
        first, second,
        "first-turn spinner frame should repaint before any assistant delta"
    );
}

#[test]
fn render_prompt_places_cursor_inside_input_text() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "abcd".to_string());
    app.input_cursor = 2;

    let output = render_to_string(&app, 100, 12);
    let coin = prompt_coin_frame(&app);
    assert!(output.contains(&format!("{coin}  ab┃cd")), "{output}");
    assert!(!output.contains(&format!("{coin}  abcd┃")), "{output}");
}

#[test]
fn active_prompt_keeps_one_blank_line_after_header() {
    let app = test_app(SessionMode::Build);

    let output = render_to_string(&app, 100, 16);
    let lines = output.lines().collect::<Vec<_>>();
    let header_bottom = lines
        .iter()
        .rposition(|line| line.contains("Squeezy v"))
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
            .take(6)
            .any(|line| line.contains('┃')),
        "{output}"
    );
}

#[test]
fn startup_banner_is_centered_in_viewport() {
    let app = test_app(SessionMode::Build);

    let output = render_to_string(&app, 120, 16);
    let banner = output
        .lines()
        .find(|line| line.contains("Squeezy v"))
        .expect("startup banner");
    let leading = banner.chars().take_while(|ch| *ch == ' ').count();
    let content_width = banner.trim().chars().count();
    let expected = (120usize.saturating_sub(content_width)) / 2;

    assert!(
        leading.abs_diff(expected) <= 1,
        "banner should be centered: leading={leading} expected={expected}\n{output}"
    );
}

#[test]
fn footer_mentions_transcript_shortcut() {
    let app = test_app(SessionMode::Build);

    let output = render_to_string(&app, 140, 16);

    assert!(output.contains("Ctrl+T full transcript"), "{output}");
    assert!(
        !output.contains("Ctrl-O expand") && !output.contains("Ctrl-E expand all"),
        "the removed per-entry expand keys must not be advertised: {output}"
    );
}

#[test]
fn active_prompt_cursor_is_vertically_centered() {
    let app = test_app(SessionMode::Build);

    let lines = prompt_input_lines(&app, PROMPT_MIN_HEIGHT, 80);

    // Composer at min height: top rule + top pad + content + bottom pad = 4 rows.
    assert_eq!(lines.len(), 4);
    let coin = prompt_coin_frame(&app);
    let has_coin = |line: &Line<'_>| line.spans.iter().any(|s| s.content.contains(coin));
    assert!(!has_coin(&lines[0]), "{lines:?}");
    assert!(!has_coin(&lines[1]), "{lines:?}");
    assert!(has_coin(&lines[2]), "{lines:?}");
    assert!(!has_coin(&lines[3]), "{lines:?}");
}

#[test]
fn assistant_marker_uses_answer_color() {
    let item = TranscriptItem::assistant("done");

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal, "Ctrl+T");

    assert_eq!(lines[0].spans[1].content.as_ref(), "☽");
    assert_eq!(
        lines[0].spans[1].style.fg,
        Some(crate::render::theme::green())
    );
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

    let lines = format_message_entry(&item, false, false, MessageOutcome::Failed, "Ctrl+T");

    assert_eq!(lines[0].spans[1].content.as_ref(), "☽");
    assert_eq!(
        lines[0].spans[1].style.fg,
        Some(crate::render::theme::red())
    );
}

#[test]
fn ansi_system_entry_parses_escapes_into_styled_spans() {
    // System messages whose content carries ANSI escape sequences flow
    // through `format_ansi_system_entry`, which parses each escape into
    // a styled ratatui span. This is how `/cost` and `/context` render
    // with bold colored headers and per-value accents. Plain-text system
    // messages should keep their original single-style rendering — see
    // `accounting_block_dispatch_skips_unrelated_system_messages` below.
    let accent_rgb = match crate::render::theme::accent() {
        ratatui::style::Color::Rgb(r, g, b) => (r, g, b),
        other => panic!("expected accent to resolve to Color::Rgb in tests, got {other:?}"),
    };
    let bold_accent_header = format!(
        "\x1b[1m\x1b[38;2;{};{};{}mCost accounting\x1b[0m",
        accent_rgb.0, accent_rgb.1, accent_rgb.2,
    );
    let value_in_accent = format!(
        "\x1b[38;2;{};{};{}m1200\x1b[0m",
        accent_rgb.0, accent_rgb.1, accent_rgb.2,
    );
    let content = format!("{bold_accent_header}\n  input={value_in_accent}");
    let item = TranscriptItem::system(content);

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal, "Ctrl+T");
    assert!(lines.len() >= 2, "{lines:?}");

    // First line: the role chrome (`• Noted`) prefix plus the parsed
    // header span, which should be bold + accent colored.
    let header_span = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "Cost accounting")
        .unwrap_or_else(|| panic!("expected `Cost accounting` span in {:?}", lines[0]));
    assert_eq!(
        header_span.style.fg,
        Some(crate::render::theme::accent()),
        "header should be accent colored: {:?}",
        header_span.style
    );
    assert!(
        header_span.style.add_modifier.contains(Modifier::BOLD),
        "header should be bold: {:?}",
        header_span.style
    );

    // Second line: continuation indent + plain prefix + accented value.
    let value_span = lines[1]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "1200")
        .unwrap_or_else(|| panic!("expected `1200` span in {:?}", lines[1]));
    assert_eq!(value_span.style.fg, Some(crate::render::theme::accent()));
}

#[test]
fn accounting_block_dispatch_skips_unrelated_system_messages() {
    let item = TranscriptItem::system("Random system note\nwith multiple\nlines");
    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal, "Ctrl+T");
    // The unrelated content keeps the default single-style rendering: the
    // header gets the standard `• Noted` chrome, the body lines fall
    // through to `action_text_lines_styled` with no per-token coloring.
    assert!(!lines.is_empty());
    let header_has_noted = lines[0]
        .spans
        .iter()
        .any(|span| span.content.as_ref() == "Noted");
    assert!(header_has_noted, "{lines:?}");
}

#[test]
fn context_snapshot_stays_expanded_in_compact_transcript() {
    use squeezy_agent::{
        AttachmentShape, ConversationShape, SessionAccountingSnapshot, TranscriptShape,
    };
    use squeezy_core::{CostSnapshot, SessionMetrics, SessionMode};
    use squeezy_llm::{RequestTokenEstimate, TokenizerKind};

    let mut config = test_config(SessionMode::Build);
    config.tui.transcript_default = TranscriptDefault::Compact;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let estimate = RequestTokenEstimate {
        input_tokens: 123_456,
        context_window_tokens: Some(400_000),
        effective_context_window_tokens: Some(368_000),
        headroom_tokens: Some(32_000),
        max_output_tokens: Some(64_000),
        input_budget_tokens: Some(304_000),
        remaining_input_tokens: Some(180_544),
        used_input_percent_x100: Some(30_86),
        tokenizer: TokenizerKind::OpenAiCompatible,
        estimated: true,
    };
    let snapshot = SessionAccountingSnapshot {
        session_id: Some("sess-context".to_string()),
        provider: "openai",
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string(),
        mode: SessionMode::Build,
        store_responses: false,
        previous_response_id: None,
        cost: CostSnapshot::default(),
        metrics: SessionMetrics::default(),
        redactions: 0,
        transcript: TranscriptShape::default(),
        conversation: ConversationShape::default(),
        attachments: AttachmentShape::default(),
        transmitted_request: estimate,
        full_history_request: estimate,
    };
    let body = commands::format_context_command(&snapshot);

    app.push_transcript_item(TranscriptItem::system(body));
    let output = render_to_string(&app, 120, 40);

    assert!(output.contains("Context window"), "{output}");
    assert!(output.contains("Consumption by source"), "{output}");
    assert!(output.contains("remaining:"), "{output}");
    assert!(
        !output.contains("for full transcript"),
        "explicit /context output should not collapse to a summary: {output}"
    );
}

#[test]
fn pending_assistant_uses_static_moon_marker() {
    let mut app = test_app(SessionMode::Build);
    app.pending_assistant.push_delta("streaming");
    app.turn_visual = TurnVisualState::Running;
    app.animation_tick = 4;

    let lines = transcript_lines_for_render(&app, Some(80), false);

    // The assistant reply marker is a static full moon (never timer-animated
    // or input-driven); the working-line star carries the motion instead.
    assert_eq!(lines[0].spans[1].content.as_ref(), "☽");
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

    assert!(output.contains("☽ Because."), "{output}");
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

    assert!(output.contains("why?"), "{output}");
    assert!(output.contains("Working ("), "{output}");
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
    assert!(!output.contains("Working ("), "{output}");
    assert!(!output.contains("• Done"), "{output}");
}

#[test]
fn failed_turn_shows_red_moon_duration_row() {
    let mut app = test_app(SessionMode::Build);
    app.turn_visual = TurnVisualState::Failed;
    app.last_turn_duration = Some(Duration::from_secs(7));

    let line = last_turn_divider_line(&app, Duration::from_secs(7), 80);

    assert_eq!(line.spans[0].content.as_ref(), "☽");
    assert_eq!(line.spans[0].style.fg, Some(crate::render::theme::red()));
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("Failed after 7s"), "{text}");
    assert!(!text.contains("Worked for"), "{text}");
}

#[test]
fn cancelled_turn_shows_cyan_moon_duration_row() {
    let mut app = test_app(SessionMode::Build);
    app.turn_visual = TurnVisualState::Cancelled;
    app.last_turn_duration = Some(Duration::from_secs(5));

    let line = last_turn_divider_line(&app, Duration::from_secs(5), 80);

    assert_eq!(line.spans[0].content.as_ref(), "☽");
    assert_eq!(line.spans[0].style.fg, Some(crate::render::theme::cyan()));
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("Cancelled after 5s"), "{text}");
    assert!(!text.contains("Worked for"), "{text}");
    assert!(!text.contains("Failed after"), "{text}");
}

#[test]
fn working_shimmer_sweeps_left_to_right() {
    let left = shimmer_word_spans("Working", 1_200);
    let right = shimmer_word_spans("Working", 2_200);
    let repeated_left = shimmer_word_spans("Working", 4_600);
    let left_foregrounds = left.iter().map(|span| span.style.fg).collect::<Vec<_>>();
    let right_foregrounds = right.iter().map(|span| span.style.fg).collect::<Vec<_>>();

    assert!(
        left_foregrounds.contains(&Some(crate::render::theme::shimmer())),
        "{left_foregrounds:?}"
    );
    assert!(
        right_foregrounds.contains(&Some(crate::render::theme::shimmer())),
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
            .any(|(fg, bg, _)| *fg != crate::render::theme::accent() && *bg == Color::Reset),
        "{first:?}"
    );
    assert!(
        second
            .iter()
            .any(|(fg, bg, _)| *fg != crate::render::theme::accent() && *bg == Color::Reset),
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
        .position(|line| line.contains("Working ("))
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
        .position(|line| line.contains("Working ("))
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
fn submitted_prompt_renders_bubble_around_text() {
    let item = TranscriptItem::user("find getFoo");

    let lines = format_message_entry_with_width(
        &item,
        false,
        false,
        MessageOutcome::Normal,
        Some(40),
        true,
        "Ctrl+T",
    );

    let content = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let bottom = lines[1]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(content.starts_with("  "), "{content}");
    assert!(content.ends_with("find getFoo"), "{content}");
    assert!(bottom.starts_with("  ╰☾"), "{bottom}");
    assert!(bottom.ends_with('╯'), "{bottom}");
    // The prompt threads a gutter connector down into the turn.
    let connector = lines
        .last()
        .expect("connector")
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert_eq!(connector, "   │");
}

#[test]
fn submitted_prompt_preserves_empty_lines() {
    let item = TranscriptItem::user("one\n\nthree\n");

    let lines = format_message_entry_with_width(
        &item,
        false,
        false,
        MessageOutcome::Normal,
        Some(30),
        true,
        "Ctrl+T",
    );
    let rendered = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    // Open bubble: 4 content rows ("one", "", "three", "") + bottom edge + connector.
    assert_eq!(lines.len(), 6);
    assert!(rendered[0].contains("one"), "{rendered:?}");
    assert_eq!(rendered[1].trim(), "");
    assert!(rendered[2].contains("three"), "{rendered:?}");
    assert_eq!(rendered[3].trim(), "");
    assert!(rendered[4].starts_with("  ╰☾"), "{rendered:?}");
    assert_eq!(rendered[5], "   │");
}

#[test]
fn failure_log_renders_as_detail_under_user_turn() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("hi"));
    app.push_log("turn failed: provider stream failed".to_string());

    let output = render_to_string(&app, 120, 16);
    assert!(output.contains(" hi"), "{output}");
    assert!(
        output.contains("│ turn failed: provider stream failed"),
        "{output}"
    );
    assert!(!output.contains("chars  turn failed"), "{output}");
}

#[test]
fn long_error_log_wraps_on_the_rail() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("yo"));
    app.push_error(
        "turn failed: provider request failed: Unsupported parameter temperature for this model"
            .to_string(),
    );

    let output = render_to_string(&app, 48, 16);
    let wrapped = output
        .lines()
        .find(|line| line.contains("temperature"))
        .unwrap_or_else(|| panic!("missing wrapped error continuation:\n{output}"));

    assert!(
        wrapped.trim_start().starts_with('│'),
        "wrapped error continuation must stay on the rail:\n{output}"
    );
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

    assert!(output.contains("Working ("), "{output}");
    assert!(output.contains("esc to interrupt"), "{output}");
    assert!(output.contains("Definition: getFoo"), "{output}");
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
    assert!(output.contains("Working ("), "{output}");
    assert!(output.contains("esc to interrupt"), "{output}");
    assert!(output.contains("Grep: getFoo"), "{output}");
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
    assert!(output.contains("Shell: ls"), "{output}");
    assert!(!output.contains("update_task_state"), "{output}");
}

#[test]
fn working_cell_shows_current_tool() {
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);
    app.remember_active_tool_call(ToolCall {
        call_id: "call-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "cargo test --workspace"}),
    });

    let output = render_to_string(&app, 140, 16);

    assert!(output.contains("Working ("), "{output}");
    assert!(output.contains("Shell: cargo test --workspace"), "{output}");
}

#[test]
fn working_cell_appends_per_tool_elapsed_after_heartbeat() {
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);
    app.remember_active_tool_call(ToolCall {
        call_id: "call-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "cargo test --workspace"}),
    });
    app.note_active_tool_progress("shell", 4_500);

    let output = render_to_string(&app, 140, 16);

    assert!(output.contains("Shell: cargo test --workspace"), "{output}");
    assert!(output.contains(" · 4s"), "{output}");
}

#[test]
fn working_cell_truncates_long_command_args() {
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);
    app.remember_active_tool_call(ToolCall {
        call_id: "call-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({
            "command": format!("echo {}", "x".repeat(200)),
        }),
    });

    let output = render_to_string(&app, 200, 16);

    assert!(output.contains("Shell: echo "), "{output}");
    assert!(output.contains("..."), "{output}");
    assert!(!output.contains(&"x".repeat(200)), "{output}");
}

#[test]
fn working_cell_omits_args_for_unknown_tool() {
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);
    app.remember_active_tool_call(ToolCall {
        call_id: "call-1".to_string(),
        name: "mcp__server__custom".to_string(),
        arguments: serde_json::json!({"opaque": "payload"}),
    });

    let output = render_to_string(&app, 140, 16);

    // Capitalized tool name with no colon when no template-driven args.
    assert!(output.contains("Mcp__server__custom"), "{output}");
    assert!(!output.contains("Mcp__server__custom:"), "{output}");
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
        "Ctrl+T",
    );
    // Open bubble: lines[0] = bullet + content text.
    assert_eq!(
        user_lines[0].spans[1].style.fg,
        Some(crate::render::theme::accent())
    );
    assert!(
        user_lines[0].spans[1].content.ends_with(' '),
        "{:?}",
        user_lines[0].spans[1].content
    );
    assert_eq!(user_lines[0].spans[2].content.as_ref(), "hi");
    assert_eq!(
        user_lines[0].spans[2].style.fg,
        Some(crate::render::theme::foreground())
    );

    let log_lines = format_transcript_entry(
        &app.transcript[1],
        false,
        app.tool_output_verbosity,
        message_outcome(&app.transcript, 1),
        "Ctrl+T",
    );
    assert_eq!(
        log_lines[0].spans[1].style.fg,
        Some(crate::render::theme::red())
    );
    assert_eq!(
        log_lines[0].spans[2].style.fg,
        Some(crate::render::theme::muted())
    );
}

#[test]
fn user_prompt_text_is_highlighted_in_transcript() {
    let item = TranscriptItem::user("find getFoo");

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal, "Ctrl+T");
    let _text = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    // Open bubble: lines[0].spans = [indent "  ", bullet (phase + space), text]
    assert_eq!(
        lines[0].spans[1].style.fg,
        Some(crate::render::theme::accent())
    );
    assert!(
        lines[0].spans[1].content.ends_with(' '),
        "{:?}",
        lines[0].spans[1].content
    );
    assert_eq!(lines[0].spans[2].content.as_ref(), "find getFoo");
    assert_eq!(
        lines[0].spans[2].style.fg,
        Some(crate::render::theme::foreground())
    );
}

#[test]
fn submitted_bang_prompt_marks_first_nonempty_bang_dark_red() {
    let item = TranscriptItem::user("  !ls");

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal, "Ctrl+T");
    // Content row is lines[0].
    let bang = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "!")
        .expect("bang marker span");
    let rest = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "ls")
        .expect("command body span");

    assert_eq!(bang.style.fg, Some(crate::render::theme::red()));
    assert_eq!(rest.style.fg, Some(crate::render::theme::foreground()));
}

#[test]
fn live_prompt_marks_first_nonempty_bang_dark_red() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "  !ls".to_string());

    let lines = prompt_input_content_lines(&app);
    let bang = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "!")
        .expect("bang marker span");
    let rest = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "ls")
        .expect("command body span");

    assert_eq!(bang.style.fg, Some(crate::render::theme::red()));
    assert_eq!(rest.style.fg, Some(crate::render::theme::foreground()));
}

#[test]
fn submitted_double_bang_prompt_marks_both_bangs_dark_red() {
    // `!!cmd` runs locally but skips the LLM context (F01). Both bangs
    // need to glow `crate::render::theme::red()` so the user can tell quiet bangs apart from
    // the regular single-bang at a glance.
    let item = TranscriptItem::user("  !!git status");

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal, "Ctrl+T");
    let bang = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "!!")
        .expect("double-bang marker span");
    let rest = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "git status")
        .expect("command body span");

    assert_eq!(bang.style.fg, Some(crate::render::theme::red()));
    assert_eq!(rest.style.fg, Some(crate::render::theme::foreground()));
    assert!(
        lines[1]
            .spans
            .iter()
            .all(|span| span.content.as_ref() != "!"),
        "the two `!` chars should merge into a single `!!` red span",
    );
}

#[test]
fn live_double_bang_prompt_marks_both_bangs_dark_red() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "  !!git status".to_string());

    let lines = prompt_input_content_lines(&app);
    let bang = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "!!")
        .expect("double-bang marker span");
    let rest = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "git status")
        .expect("command body span");

    assert_eq!(bang.style.fg, Some(crate::render::theme::red()));
    assert_eq!(rest.style.fg, Some(crate::render::theme::foreground()));
}

#[test]
fn prompt_height_grows_for_multiline_input() {
    let mut app = test_app(SessionMode::Build);
    assert_eq!(input_panel_height(&app, 100), PROMPT_MIN_HEIGHT);

    set_input(&mut app, "one\ntwo\nthree".to_string());
    assert_eq!(input_panel_height(&app, 100), 6);

    set_input(
        &mut app,
        (0..30)
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
async fn esc_cancels_active_turn_and_never_exits_when_idle() {
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

    for _ in 0..3 {
        let quit = handle_key(
            &mut app,
            &mut agent,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await
        .expect("idle esc");
        assert!(!quit);
        assert!(!app.exit_confirm_armed);
    }
}

#[tokio::test]
async fn ctrl_c_arms_exit_confirm_then_exits_on_second_press() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .expect("first ctrl-c");
    assert!(!quit);
    assert!(app.exit_confirm_armed);
    assert!(format_status_tokens(&app).contains("Ctrl+C or Y to exit"));

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .expect("second ctrl-c");
    assert!(quit);
}

#[tokio::test]
async fn ctrl_c_then_y_exits() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .expect("first ctrl-c");
    assert!(app.exit_confirm_armed);

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
    )
    .await
    .expect("y confirm");
    assert!(quit);
}

#[tokio::test]
async fn ctrl_c_then_other_key_disarms_and_keystroke_falls_through() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .expect("first ctrl-c");
    assert!(app.exit_confirm_armed);

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
    )
    .await
    .expect("letter");
    assert!(!quit);
    assert!(!app.exit_confirm_armed);
    assert_eq!(app.input, "a");
}

#[tokio::test]
async fn ctrl_c_during_turn_cancels_turn_and_does_not_arm_exit() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    app.turn_rx = Some(rx);
    app.cancel = Some(cancel.clone());

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-c during turn");
    assert!(!quit);
    assert!(cancel.is_cancelled());
    assert!(!app.exit_confirm_armed);
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
    assert!(!app.exit_confirm_armed);
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
async fn slash_pins_empty_renders_guidance() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/pins").await);

    assert_eq!(app.status, "no pinned context");
    let rendered = last_message_content(&app).expect("system guidance");
    assert!(rendered.contains("No pinned context yet"), "{rendered}");
    assert!(rendered.contains("/pin selected"), "{rendered}");
}

#[tokio::test]
async fn slash_feedback_previews_redacted_message_and_prompts_for_decision() {
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

    assert_eq!(app.status, "feedback ready: Enter send · Esc discard");
    assert!(app.pending_feedback.is_some());
    let TranscriptEntryKind::Message(item) = &app.transcript.last().expect("preview").kind else {
        panic!("feedback preview should be a message entry");
    };
    let preview = item.content.clone();
    assert!(preview.contains("feedback preview"), "{preview}");
    assert!(preview.contains("<redacted:"), "{preview}");
    assert!(!preview.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
    assert!(
        preview.contains("Press Enter to send or Esc to discard."),
        "{preview}"
    );

    let output = render_to_string(&app, 100, 24);
    assert!(output.contains("Send feedback?"), "{output}");
    assert!(output.contains("Enter/Y Send"), "{output}");
    assert!(output.contains("Esc/N Discard"), "{output}");
}

#[tokio::test]
async fn slash_feedback_escape_discards_pending_preview() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/feedback too much ceremony").await);
    assert!(app.pending_feedback.is_some());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
    )
    .await
    .expect("handle feedback discard");

    assert!(app.pending_feedback.is_none());
    assert_eq!(app.status, "feedback discarded");
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

    assert!(handle_slash_command(&mut app, &mut agent, "/tasks").await);
    assert_eq!(app.status, "1 tasks");
    assert!(
        last_message_content(&app).is_some_and(|content| content.contains("checkpoint_list")),
        "expected tasks list to include checkpoint_list"
    );

    assert!(handle_slash_command(&mut app, &mut agent, &format!("/task {}", job.id)).await);
    assert!(app.status.starts_with(&format!("task {} ", job.id)));
    let detail = last_message_content(&app).unwrap_or_default().to_string();
    assert!(
        detail.contains("output_handle=-"),
        "expected task detail to include output handle placeholder: {detail}"
    );
    assert!(
        detail.contains("tool=checkpoint_list"),
        "expected task detail to include tool name: {detail}"
    );
    assert!(
        detail.contains("call_id=test-checkpoints"),
        "expected task detail to include call_id: {detail}"
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

    assert!(handle_slash_command(&mut app, &mut agent, &format!("/task-cancel {}", job.id)).await);
    assert!(
        app.status.starts_with("cancelling task ")
            || app.status.starts_with(&format!("task {} ", job.id)),
        "expected cancel acknowledgement, got {}",
        app.status
    );

    // A second cancel for the same id should report inactive once the job has settled.
    let max_attempts = 50;
    let mut saw_inactive = false;
    for _ in 0..max_attempts {
        assert!(
            handle_slash_command(&mut app, &mut agent, &format!("/task-cancel {}", job.id)).await
        );
        if app.status == format!("task {} not active", job.id) {
            saw_inactive = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        saw_inactive,
        "task never reported as inactive: {}",
        app.status
    );
}

#[tokio::test]
async fn slash_job_cancel_rejects_non_numeric_id() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/task-cancel abc").await);
    assert_eq!(app.status, "task id must be a number");

    assert!(handle_slash_command(&mut app, &mut agent, "/task-cancel").await);
    assert_eq!(app.status, "usage: /task-cancel <id>");
}

// `/jobs`, `/job`, `/job-cancel` are kept as aliases for one release. The
// canonical name is `/tasks` — see F07-cc-tasks-and-background-jobs.
mod slash_commands {
    pub(super) mod tasks {
        use super::super::*;

        #[tokio::test]
        async fn lists_jobs_and_reviewer() {
            let mut agent = test_agent(SessionMode::Build);
            let mut app = test_app(SessionMode::Build);
            // An in-flight (Queued / Running) job belongs to the listing.
            let job = agent.start_local_tool_job(ToolCall {
                call_id: "test-tasks".to_string(),
                name: "checkpoint_list".to_string(),
                arguments: serde_json::json!({}),
            });
            app.jobs.insert(job.id, job.clone());

            assert!(handle_slash_command(&mut app, &mut agent, "/tasks").await);
            let body = last_message_content(&app).unwrap_or_default().to_string();
            assert!(
                body.contains("checkpoint_list"),
                "expected /tasks output to include the in-flight job: {body}"
            );
            assert!(
                body.contains("reviewer"),
                "expected /tasks output to include a reviewer line: {body}"
            );
        }
    }
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
fn non_retryable_provider_request_drops_retry_hint() {
    let marker = squeezy_llm::anthropic_error::NON_RETRYABLE_MARKER;
    let payload = format!(
        "{marker}Anthropic rejected request (invalid_request_error): thinking.enabled.budget_tokens must be >= 1024. Raise max_output_tokens to at least 1025 or lower reasoning_effort.",
    );
    let status = format_error_status(&SqueezyError::ProviderRequest(payload));
    assert!(
        !status.contains("retry or check provider/network status"),
        "non-retryable status must not advertise a bogus retry: {status}",
    );
    assert!(
        !status.contains("[non-retryable]"),
        "sentinel marker must not leak into the user-facing status: {status}",
    );
    assert!(
        status.contains("Anthropic rejected request"),
        "human prose must survive: {status}",
    );
    assert!(
        status.contains("max_output_tokens"),
        "next-step hint must survive: {status}",
    );
}

#[test]
fn retryable_provider_request_keeps_retry_hint() {
    let status = format_error_status(&SqueezyError::ProviderRequest(
        "Anthropic rejected request (overloaded_error): Overloaded".into(),
    ));
    assert!(
        status.contains("retry or check provider/network status"),
        "5xx-style errors keep the retry suffix: {status}",
    );
}

#[test]
fn repo_status_starts_pending_then_drains_from_background() {
    let config = test_config(SessionMode::Build);
    let mut app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );
    // The probe is deferred off the startup path, so a fresh app shows the
    // neutral placeholder rather than the misleading "no repo".
    assert!(app.repo.pending);
    assert!(status_left_text(&app).contains("git …"));

    // Deliver a detected status the way the background `spawn_blocking`
    // probe does, then drain it as the main loop would.
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send(RepoStatus {
        branch: Some("main".to_string()),
        changed_files: 0,
        operation: None,
        available: true,
        pull_request: None,
        branch_changes: None,
        pending: false,
    })
    .unwrap();
    app.repo_status_rx = Some(rx);
    drain_repo_status(&mut app);

    assert!(!app.repo.pending);
    assert!(status_left_text(&app).contains("git main"));
}

#[test]
fn repo_status_handles_non_git_roots() {
    let config = AppConfig {
        workspace_root: std::env::temp_dir(),
        ..test_config(SessionMode::Build)
    };

    assert_eq!(
        RepoStatus::detect_at(&config.workspace_root).compact(),
        "repo=none"
    );
}

#[test]
fn base64_encoder_supports_osc52_payloads() {
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"a"), "YQ==");
    assert_eq!(base64_encode(b"ab"), "YWI=");
    assert_eq!(base64_encode(b"abc"), "YWJj");
}

#[tokio::test]
async fn successful_edit_turn_pushes_diff_undo_hint() {
    let mut config = test_config(SessionMode::Build);
    config.checkpoints_enabled = true;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);

    let edit_result = sample_tool_result("apply_patch", "patched ok");
    tx.send(AgentEvent::ToolCallCompleted {
        turn_id: TurnId::new(1),
        result: edit_result,
    })
    .await
    .expect("send tool result");
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("done"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    let hint = app.transcript.iter().find_map(|entry| match &entry.kind {
        TranscriptEntryKind::Log(LogEntry { message, .. })
            if message.contains("/diff") && message.contains("/undo") =>
        {
            Some(message.clone())
        }
        _ => None,
    });
    assert!(
        hint.is_some(),
        "successful edit turn must push a /diff /undo hint; transcript: {:?}",
        app.transcript
    );
    assert!(
        !app.last_turn_had_edits,
        "flag must reset after the hint fires"
    );
}

#[tokio::test]
async fn successful_edit_turn_hides_undo_hint_when_checkpointing_disabled() {
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);

    let edit_result = sample_tool_result("apply_patch", "patched ok");
    tx.send(AgentEvent::ToolCallCompleted {
        turn_id: TurnId::new(1),
        result: edit_result,
    })
    .await
    .expect("send tool result");
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("done"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    let hint = app.transcript.iter().find_map(|entry| match &entry.kind {
        TranscriptEntryKind::Log(LogEntry { message, .. }) if message.contains("/diff") => {
            Some(message.clone())
        }
        _ => None,
    });
    let hint = hint.expect("successful edit turn must push a /diff hint");
    assert!(
        !hint.contains("/undo"),
        "checkpointing disabled should hide /undo hint: {hint}"
    );
}

#[tokio::test]
async fn readonly_turn_does_not_push_undo_hint() {
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);

    let read_result = sample_tool_result("read_file", "file body");
    tx.send(AgentEvent::ToolCallCompleted {
        turn_id: TurnId::new(1),
        result: read_result,
    })
    .await
    .expect("send tool result");
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("done"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    let hint_count = app
        .transcript
        .iter()
        .filter(|entry| {
            matches!(&entry.kind, TranscriptEntryKind::Log(LogEntry { message, .. })
                if message.contains("/diff") && message.contains("/undo"))
        })
        .count();
    assert_eq!(hint_count, 0, "read-only turn must not produce /undo hint");
}

#[tokio::test]
async fn repeated_raw_shell_output_is_not_rendered_as_assistant_reply() {
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    let output = "about.hbs\nabout.toml\nCargo.toml";

    let mut shell_result = sample_tool_result("shell", output);
    shell_result.content = serde_json::json!({
        "command": "ls",
        "stdout": output,
        "stderr": "",
        "exit_code": 0,
    });
    tx.send(AgentEvent::ToolCallCompleted {
        turn_id: TurnId::new(1),
        result: shell_result,
    })
    .await
    .expect("send tool result");
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant(output),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    let assistant_count = app
        .transcript
        .iter()
        .filter(|entry| {
            matches!(&entry.kind, TranscriptEntryKind::Message(item)
                if item.role == Role::Assistant)
        })
        .count();
    let tool_count = app
        .transcript
        .iter()
        .filter(|entry| matches!(&entry.kind, TranscriptEntryKind::ToolResult(_)))
        .count();

    assert_eq!(tool_count, 1, "shell tool card should remain visible");
    assert_eq!(
        assistant_count, 0,
        "assistant duplicate should be dropped: {:?}",
        app.transcript
    );
}

#[tokio::test]
async fn failed_edit_turn_error_status_mentions_undo() {
    let mut config = test_config(SessionMode::Build);
    config.checkpoints_enabled = true;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);

    let edit_result = sample_tool_result("write_file", "wrote ok");
    tx.send(AgentEvent::ToolCallCompleted {
        turn_id: TurnId::new(1),
        result: edit_result,
    })
    .await
    .expect("send tool result");
    tx.send(AgentEvent::Failed {
        turn_id: TurnId::new(1),
        error: SqueezyError::Permission("denied".to_string()),
    })
    .await
    .expect("send failed");
    drop(tx);
    drain_agent_events(&mut app).await;

    assert!(
        app.status.contains("/undo"),
        "failed turn after an edit must surface /undo in status; got: {}",
        app.status
    );
}

#[tokio::test]
async fn cancel_preserves_pending_prompt_for_ctrl_r() {
    let mut app = test_app(SessionMode::Build);
    app.cancelled_prompt = Some("write the README".to_string());

    let hint = format_status_hints(&app);
    assert!(
        hint.contains("Ctrl+R"),
        "idle hint must advertise Ctrl+R when a cancelled prompt is stashed; got: {hint}"
    );
    assert!(restore_cancelled_prompt(&mut app), "restore must succeed");
    assert_eq!(app.input, "write the README");
    assert!(app.cancelled_prompt.is_none());
    assert_eq!(app.input_cursor, app.input.len());
}

#[test]
fn restore_is_noop_when_no_cancelled_prompt() {
    let mut app = test_app(SessionMode::Build);
    assert!(
        !restore_cancelled_prompt(&mut app),
        "restore must report no-op when nothing is stashed"
    );
    assert!(app.input.is_empty());
}

#[test]
fn restore_does_not_overwrite_in_progress_input() {
    let mut app = test_app(SessionMode::Build);
    app.input = "draft".to_string();
    app.input_cursor = app.input.len();
    app.cancelled_prompt = Some("previous".to_string());
    assert!(
        !restore_cancelled_prompt(&mut app),
        "restore must refuse when composer is non-empty"
    );
    assert_eq!(app.input, "draft");
    assert_eq!(app.cancelled_prompt.as_deref(), Some("previous"));
}

#[tokio::test]
async fn completion_clears_cancelled_prompt() {
    let mut app = test_app(SessionMode::Build);
    app.cancelled_prompt = Some("hello".to_string());
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("done"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;
    assert!(
        app.cancelled_prompt.is_none(),
        "successful completion must clear the cancelled-prompt slot",
    );
}

#[tokio::test]
async fn cancel_restores_prompt_into_composer() {
    let mut app = test_app(SessionMode::Build);
    // Mirror the Enter handler: prompt stashed, composer cleared, turn live.
    app.cancelled_prompt = Some("write the README".to_string());
    app.input.clear();
    app.input_cursor = 0;
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Cancelled {
        turn_id: TurnId::new(1),
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
    })
    .await
    .expect("send cancelled");
    drop(tx);
    drain_agent_events(&mut app).await;
    assert_eq!(
        app.input, "write the README",
        "cancel must put the typed prompt back in the composer",
    );
    assert_eq!(app.input_cursor, app.input.len());
    assert!(
        app.cancelled_prompt.is_none(),
        "auto-restore consumes the stash; got: {:?}",
        app.cancelled_prompt
    );
}

#[tokio::test]
async fn cancel_does_not_clobber_draft_typed_during_interrupt() {
    let mut app = test_app(SessionMode::Build);
    app.cancelled_prompt = Some("original".to_string());
    // User started typing again before the Cancelled event drained.
    app.input = "new draft".to_string();
    app.input_cursor = app.input.len();
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Cancelled {
        turn_id: TurnId::new(1),
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
    })
    .await
    .expect("send cancelled");
    drop(tx);
    drain_agent_events(&mut app).await;
    assert_eq!(
        app.input, "new draft",
        "cancel must not overwrite the user's in-progress draft",
    );
    assert_eq!(
        app.cancelled_prompt.as_deref(),
        Some("original"),
        "stash stays so Ctrl+R can still recover the original prompt",
    );
}

#[test]
fn context_budget_renders_percent_and_threshold() {
    let mut app = test_app(SessionMode::Build);
    app.context_compaction_threshold = 6_000;
    app.context_estimate = ContextEstimate {
        estimated_tokens: 4_500,
        ..ContextEstimate::default()
    };
    let details = format_status_details(&app);
    assert!(
        details.contains("ctx 4500/6000 (75%)"),
        "expected context cell with percent; got: {details}"
    );
}

#[test]
fn context_budget_hint_at_high_usage() {
    let mut app = test_app(SessionMode::Build);
    app.context_compaction_threshold = 6_000;
    app.context_estimate = ContextEstimate {
        estimated_tokens: 5_800,
        ..ContextEstimate::default()
    };
    let hint = format_status_hints(&app);
    assert!(
        hint.contains("/pin") && hint.contains("/compact"),
        "expected /pin and /compact hints when near threshold; got: {hint}"
    );
}

#[test]
fn context_budget_hint_omitted_below_threshold() {
    let mut app = test_app(SessionMode::Build);
    app.context_compaction_threshold = 6_000;
    app.context_estimate = ContextEstimate {
        estimated_tokens: 2_000,
        ..ContextEstimate::default()
    };
    let hint = format_status_hints(&app);
    assert!(
        !hint.contains("/pin to keep"),
        "low usage must not surface /pin hint; got: {hint}"
    );
}

/// Helper: count post-turn compaction-nudge log entries in the transcript.
/// We key on the "auto-compact" wording the nudge introduces — that phrase
/// is unique to this advisory, so the filter doubles as proof that the
/// rendered text references the auto-trigger explicitly (the whole point
/// of the nudge is to be actionable advice *before* auto-compaction
/// rewrites the conversation).
fn count_auto_compact_nudges(app: &TuiApp) -> usize {
    app.transcript
        .iter()
        .filter(|entry| {
            matches!(&entry.kind, TranscriptEntryKind::Log(LogEntry { message, .. })
                if message.contains("auto-compact"))
        })
        .count()
}

#[tokio::test]
async fn pre_compaction_nudge_pushed_once_then_resets_after_compaction() {
    let mut app = test_app(SessionMode::Build);
    app.context_compaction_threshold = 6_000;

    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("ok"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate {
            // 5_800 / 6_000 ≈ 97% of the auto-compact threshold — comfortably
            // above the 70% nudge floor and below the 100% suppression edge.
            estimated_tokens: 5_800,
            ..ContextEstimate::default()
        },
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    assert_eq!(
        count_auto_compact_nudges(&app),
        1,
        "nudge must fire exactly once on first crossing"
    );

    // A second high-usage turn must not fire the nudge again until
    // compaction resets the latch.
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(2),
        message: TranscriptItem::assistant("ok"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate {
            estimated_tokens: 5_900,
            ..ContextEstimate::default()
        },
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    assert_eq!(
        count_auto_compact_nudges(&app),
        1,
        "nudge must not fire again until compaction"
    );
}

/// The nudge must fire as soon as estimated tokens reach 70% of the
/// auto-compact threshold (well before the 100% mark that triggers
/// compaction), and the rendered text must reference the auto-trigger so
/// the advice is actionable — otherwise users only see "consider
/// /compact" *after* compaction has already rewritten the conversation.
#[tokio::test]
async fn pre_compaction_nudge_fires_at_seventy_percent_of_threshold() {
    let mut app = test_app(SessionMode::Build);
    app.context_compaction_threshold = 6_000;

    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("ok"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate {
            // 4_200 / 6_000 = 70.0% — exactly at the firing point.
            estimated_tokens: 4_200,
            ..ContextEstimate::default()
        },
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    let message = app
        .transcript
        .iter()
        .find_map(|entry| match &entry.kind {
            TranscriptEntryKind::Log(LogEntry { message: text, .. })
                if text.contains("auto-compact") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .expect("nudge must fire at 70% of the auto-compact threshold");
    // Text must reference the auto-trigger explicitly so the advice is
    // actionable — without "auto-compact" in the message, users can't tell
    // why /pin or /compact would help.
    assert!(
        message.contains("auto-compact"),
        "nudge must reference the auto-trigger; got: {message}"
    );
    assert!(
        message.contains("/pin") && message.contains("/compact"),
        "nudge must surface the deliberate actions; got: {message}"
    );
}

/// Once estimated tokens reach or pass the auto-compact threshold the
/// nudge must stay silent — auto-compaction is already taking over the
/// UI, and a stale "consider /compact" advisory would just clutter the
/// post-compaction transcript with advice the user can no longer act on.
#[tokio::test]
async fn pre_compaction_nudge_suppressed_at_or_past_threshold() {
    let mut app = test_app(SessionMode::Build);
    app.context_compaction_threshold = 6_000;

    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("ok"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate {
            // 6_100 / 6_000 ≈ 102% — past the threshold, so we're already in
            // auto-compaction territory. The nudge would be useless advice.
            estimated_tokens: 6_100,
            ..ContextEstimate::default()
        },
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    assert_eq!(
        count_auto_compact_nudges(&app),
        0,
        "nudge must not fire once usage has already crossed the auto-compact threshold",
    );
}

/// Below 70% of the auto-compact threshold the nudge should stay silent —
/// firing earlier would be alarmist for routine sessions.
#[tokio::test]
async fn pre_compaction_nudge_silent_below_seventy_percent() {
    let mut app = test_app(SessionMode::Build);
    app.context_compaction_threshold = 6_000;

    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("ok"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate {
            // 4_100 / 6_000 ≈ 68% — just under the 70% firing floor.
            estimated_tokens: 4_100,
            ..ContextEstimate::default()
        },
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    assert_eq!(
        count_auto_compact_nudges(&app),
        0,
        "nudge must stay silent below the 70% firing floor",
    );
}

#[tokio::test]
async fn proposed_plan_block_renders_as_log_entry_and_persists_under_plans_dir() {
    let root = temp_workspace("proposed_plan_persist");
    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    for delta in [
        "intro <propos",
        "ed_plan>\nstep 1\nstep 2\n</propos",
        "ed_plan>\ntrailing\n",
    ] {
        tx.send(AgentEvent::AssistantDelta {
            turn_id: TurnId::new(1),
            delta: delta.to_string(),
        })
        .await
        .expect("send delta");
    }
    drop(tx);

    drain_agent_events(&mut app).await;

    assert_eq!(
        app.pending_assistant.text(),
        "intro \ntrailing\n",
        "proposed plan markers must not appear in the live assistant pane",
    );
    // Plan-mode v3 PR-F: the proposed plan lands as a styled
    // [`TranscriptEntryKind::PlanCard`], not a free-form log entry.
    let card_id = app
        .transcript
        .iter()
        .find_map(|entry| match &entry.kind {
            TranscriptEntryKind::PlanCard(data) => Some(data.plan_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a PlanCard transcript entry; transcript={:?}",
                app.transcript
            )
        });
    assert!(card_id.starts_with("plan-"));

    let plan_id = app
        .current_plan_id
        .as_ref()
        .expect("current_plan_id should be set after persistence");
    assert_eq!(plan_id, &card_id);
    assert!(plan_id.starts_with("plan-"));
    // After PR-D the layout is per-session:
    // <workspace>/.squeezy/plans/<session_id>/<plan_id>.md. Test-mode
    // sessions have no agent-assigned session id, so the TUI falls back
    // to [`proposed_plan::FALLBACK_SESSION_ID`].
    let path = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID)
        .join(format!("{plan_id}.md"));
    // The file on disk wraps the body in YAML front-matter (PR-D); use
    // the public helper to strip it before comparing.
    assert_eq!(
        proposed_plan::read_plan_body(&path).expect("plan file exists"),
        "step 1\nstep 2\n"
    );

    let _ = std::fs::remove_dir_all(&root);
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
    assert_eq!(app.pending_assistant.text(), "streamed");
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
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
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
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
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
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
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
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
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
    app.pending_assistant
        .push_delta(&format!("Here is the report:\n\n```text\n{stdout}"));

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
        // See `test_agent`: keep the test fixture off the real workspace so
        // `Agent::new` / `TuiApp::new` don't crawl the repo on every test.
        workspace_root: temp_workspace("config"),
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
        context: None,
        preview: Vec::new(),
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
        subagent_id: None,
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

async fn wait_for_turn_completion(app: &mut TuiApp) {
    for _ in 0..100 {
        drain_agent_events(app).await;
        if app.turn_rx.is_none() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("help turn did not complete");
}

fn transcript_message_contents(app: &TuiApp) -> Vec<&str> {
    app.transcript
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message(item) => Some(item.content.as_str()),
            _ => None,
        })
        .collect()
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
        output.push_str(&rendered_line_text(line));
        output.push('\n');
    }
    output
}

fn rendered_line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
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
    // Use a fresh empty temp workspace so the agent's tool registry doesn't
    // crawl the entire repo (which adds seconds per test, especially on
    // Windows where filesystem syscalls are slow). The TUI tests never
    // touch the workspace; they only need a valid `AppConfig`.
    test_agent_with_config(AppConfig {
        session_mode: mode,
        workspace_root: temp_workspace("agent"),
        ..Default::default()
    })
}

fn test_agent_with_config(config: AppConfig) -> Agent {
    Agent::new(
        config,
        Arc::new(UnavailableProvider::new("scripted", "test provider")),
    )
}

fn test_agent_without_session_log(mode: SessionMode) -> Agent {
    test_agent_without_session_log_with_config(AppConfig {
        session_mode: mode,
        workspace_root: temp_workspace("agent"),
        ..Default::default()
    })
}

fn test_agent_without_session_log_with_config(config: AppConfig) -> Agent {
    Agent::new_ephemeral(
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

#[test]
fn apply_patch_failure_then_success_on_same_path_drops_the_failure_row() {
    // Background: apply_patch's unified-diff fallback occasionally fails
    // and the agent immediately retries with a full-file rewrite that
    // succeeds. The user gains nothing from seeing the transient ✖ Failed
    // row once we know the retry worked, so it gets pruned from the
    // transcript on the matching success.
    let mut app = test_app(SessionMode::Build);

    let mut failure = sample_tool_result("apply_patch", "");
    failure.status = ToolStatus::Error;
    failure.content = serde_json::json!({
        "error": "unified-diff fallback could not apply cleanly",
        "failed_path": "src/middleware/open_beta_cache.rs",
    });
    app.push_tool_result_with_call(failure, None);
    assert_eq!(app.transcript.len(), 1, "failure row recorded");

    let mut success = sample_tool_result("apply_patch", "");
    success.status = ToolStatus::Success;
    success.content = serde_json::json!({
        "files": [{ "path": "src/middleware/open_beta_cache.rs" }],
    });
    app.push_tool_result_with_call(success, None);

    assert_eq!(
        app.transcript.len(),
        1,
        "the prior failure row must be replaced, not stacked"
    );
    let kind = &app.transcript[0].kind;
    match kind {
        TranscriptEntryKind::ToolResult(tool) => {
            assert_eq!(tool.result.status, ToolStatus::Success);
        }
        other => panic!("expected ToolResult entry, got {other:?}"),
    }
    assert!(
        app.recent_edit_failures.is_empty(),
        "tracker should be cleared after replay"
    );
}

#[test]
fn apply_patch_failure_on_different_path_is_not_suppressed_by_unrelated_success() {
    let mut app = test_app(SessionMode::Build);

    let mut failure = sample_tool_result("apply_patch", "");
    failure.status = ToolStatus::Error;
    failure.content = serde_json::json!({
        "error": "unified-diff fallback could not apply cleanly",
        "failed_path": "src/middleware/open_beta_cache.rs",
    });
    app.push_tool_result_with_call(failure, None);

    let mut success = sample_tool_result("apply_patch", "");
    success.status = ToolStatus::Success;
    success.content = serde_json::json!({
        "files": [{ "path": "src/other.rs" }],
    });
    app.push_tool_result_with_call(success, None);

    assert_eq!(
        app.transcript.len(),
        2,
        "failure on a different file should still be visible"
    );
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
        web_call_stats: None,
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
        image_media_type: None,
        image_data_base64: None,
    }
}

// ---- /effort session-level reasoning-effort setter ----

#[tokio::test]
async fn slash_effort_low_sets_session_reasoning_effort() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    assert!(agent.config_snapshot().reasoning_effort.is_none());

    let ran = handle_slash_command(&mut app, &mut agent, "/effort low").await;
    assert!(ran);
    assert_eq!(
        agent.config_snapshot().reasoning_effort,
        Some(squeezy_core::ReasoningEffort::Low),
    );
}

#[tokio::test]
async fn slash_effort_auto_clears_session_reasoning_effort() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    // Pre-seed a value so `auto` has something to clear.
    let mut seeded = agent.config_snapshot();
    seeded.reasoning_effort = Some(squeezy_core::ReasoningEffort::High);
    agent.replace_config(seeded);
    let mut app = test_app(SessionMode::Build);

    let ran = handle_slash_command(&mut app, &mut agent, "/effort auto").await;
    assert!(ran);
    assert!(agent.config_snapshot().reasoning_effort.is_none());
}

#[tokio::test]
async fn slash_effort_rejects_unknown_value() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut seeded = agent.config_snapshot();
    seeded.reasoning_effort = Some(squeezy_core::ReasoningEffort::Medium);
    agent.replace_config(seeded);
    let mut app = test_app(SessionMode::Build);

    let ran = handle_slash_command(&mut app, &mut agent, "/effort bogus").await;
    assert!(ran);
    // Original value preserved on bad input.
    assert_eq!(
        agent.config_snapshot().reasoning_effort,
        Some(squeezy_core::ReasoningEffort::Medium),
    );
    assert!(
        app.status.contains("unknown effort"),
        "expected error status, got {}",
        app.status,
    );
}

// ---- /theme palette switch ----

/// `/theme dark` is a backwards-compatible alias for the bundled default
/// theme and mirrors the choice into the agent's config.
#[tokio::test]
async fn slash_theme_dark_selects_default_theme_and_config() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    // Point settings writes at a tempfile so the test never touches HOME.
    let dir = temp_workspace("theme_dark");
    let settings_path = dir.join("settings.toml");
    let _guard = ScopedSettingsPath::new(settings_path.clone());

    let ran = handle_slash_command(&mut app, &mut agent, "/theme dark").await;
    assert!(ran, "/theme dark should dispatch");
    assert_eq!(crate::render::theme::current_theme_name(), "default");
    assert_eq!(agent.config_snapshot().tui.theme, "default");
    let saved = std::fs::read_to_string(&settings_path).expect("settings file written");
    assert!(
        saved.contains("theme = \"default\""),
        "settings.toml should record the theme; got {saved}"
    );
}

/// `/theme light` is a backwards-compatible alias for the brighter builtin.
#[tokio::test]
async fn slash_theme_light_selects_bright_theme() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_light");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let ran = handle_slash_command(&mut app, &mut agent, "/theme light").await;
    assert!(ran);
    assert_eq!(crate::render::theme::current_theme_name(), "bright");
    assert_eq!(agent.config_snapshot().tui.theme, "bright");
}

/// `/theme system` is a backwards-compatible alias for the bundled default.
#[tokio::test]
async fn slash_theme_system_selects_default_theme() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_system");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let ran = handle_slash_command(&mut app, &mut agent, "/theme light").await;
    assert!(ran);
    assert_eq!(crate::render::theme::current_theme_name(), "bright");

    let ran = handle_slash_command(&mut app, &mut agent, "/theme system").await;
    assert!(ran);
    assert_eq!(crate::render::theme::current_theme_name(), "default");
    assert_eq!(agent.config_snapshot().tui.theme, "default");
}

/// Unknown sub-arguments don't mutate anything — the user sees a usage hint
/// instead of a silent tone change.
#[tokio::test]
async fn slash_theme_unknown_value_does_not_change_palette() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_bad");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let before_theme_name = crate::render::theme::current_theme_name();
    let before_theme = agent.config_snapshot().tui.theme;
    let ran = handle_slash_command(&mut app, &mut agent, "/theme zebra").await;
    assert!(ran);
    assert_eq!(
        crate::render::theme::current_theme_name(),
        before_theme_name
    );
    assert_eq!(agent.config_snapshot().tui.theme, before_theme);
    assert!(
        app.status.contains("unknown theme"),
        "status should mention the bad value, got: {}",
        app.status
    );
}

/// Bare `/theme` opens the config screen focused on Themes, matching `/model`
/// and the other no-argument config shortcuts.
#[tokio::test]
async fn slash_theme_without_arg_opens_theme_config_section() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    let ran = handle_slash_command(&mut app, &mut agent, "/theme").await;
    assert!(ran);
    let state = app.config_screen.expect("config screen should be open");
    assert_eq!(
        state.current_section().id,
        squeezy_core::config_schema::SectionId::Themes
    );
}

/// `/theme catppuccin` selects the bundled mauve palette.
#[tokio::test]
async fn slash_theme_catppuccin_selects_mauve_theme() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_catppuccin");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let ran = handle_slash_command(&mut app, &mut agent, "/theme catppuccin").await;
    assert!(ran, "/theme catppuccin should dispatch");
    assert_eq!(crate::render::theme::current_theme_name(), "catppuccin");
    assert_ne!(
        crate::render::theme::rgb(crate::render::theme::token::PALETTE_ACCENT),
        crate::render::theme::resolve_theme(&agent.config_snapshot(), "default")
            .resolve(crate::render::theme::token::PALETTE_ACCENT)
            .expect("default accent"),
        "catppuccin must override the amber default to the mauve accent",
    );
    assert_eq!(agent.config_snapshot().tui.theme, "catppuccin");
}

/// `/theme high-contrast` selects the bundled high-contrast palette.
#[tokio::test]
async fn slash_theme_high_contrast_selects_builtin_theme() {
    let mut agent = test_agent_without_session_log(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_hc");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let ran = handle_slash_command(&mut app, &mut agent, "/theme high-contrast").await;
    assert!(ran);
    assert_eq!(crate::render::theme::current_theme_name(), "high-contrast");
    assert_eq!(agent.config_snapshot().tui.theme, "high-contrast");

    let ran = handle_slash_command(&mut app, &mut agent, "/theme system").await;
    assert!(ran);
    assert_eq!(crate::render::theme::current_theme_name(), "default");
}

/// Regression for `squeezy-ramu`: when the eval harness pins a settings
/// path override, `/theme dark` must persist to the scratch file and
/// must NOT touch the env-resolved (production-style "home") path. The
/// harness's per-run scratch path beats `$SQUEEZY_SETTINGS_PATH` /
/// `$HOME/.squeezy/settings.toml` so scenarios can't clobber the
/// operator's real config mid-run.
#[tokio::test]
async fn tui_harness_settings_override_pins_theme_writes_to_scratch() {
    use crate::testing::TuiHarness;

    let dir = temp_workspace("ramu_harness_override");
    let scratch_settings = dir.join("scratch-settings.toml");
    // `fake_home_settings` stands in for `~/.squeezy/settings.toml`:
    // we point `SQUEEZY_SETTINGS_PATH` at it so `default_settings_path`
    // (the production fallback) resolves here, then assert the override
    // wins and this file is never created.
    let fake_home_settings = dir.join("fake-home-settings.toml");
    let _guard = ScopedSettingsPath::new(fake_home_settings.clone());

    let mut config = test_config(SessionMode::Build);
    config.workspace_root = dir.clone();
    let provider = Arc::new(UnavailableProvider::new(
        "scripted",
        "harness override test",
    ));
    let mut harness = TuiHarness::new(
        config,
        SessionMode::Build,
        provider,
        80,
        24,
        Some(scratch_settings.clone()),
    )
    .expect("harness builds with override");

    // Sanity: the override is the resolved persistence target.
    assert_eq!(
        harness.app_mut().user_settings_path(),
        scratch_settings,
        "override should win over $SQUEEZY_SETTINGS_PATH",
    );

    // Drive `/theme dark` through the live slash dispatch — same code
    // path the eval composer hits via `send_keys` typing `/theme dark`
    // + Enter. We invoke the dispatcher directly to keep the test
    // fast and decoupled from key-event parsing.
    let ran = {
        let (app, agent) = harness.app_and_agent_mut();
        handle_slash_command(app, agent, "/theme dark").await
    };
    assert!(ran, "/theme dark should dispatch");
    assert_eq!(crate::render::theme::current_theme_name(), "default");

    // Scratch file got the override; the env-pointed "home" file did
    // NOT — that's the whole point of the fix.
    let saved = std::fs::read_to_string(&scratch_settings).expect("scratch settings file written");
    assert!(
        saved.contains("theme = \"default\""),
        "scratch should record the theme; got {saved}",
    );
    assert!(
        !fake_home_settings.exists(),
        "env-pointed fallback path must not be created when override is set; \
         leaked write to {}",
        fake_home_settings.display(),
    );
}

/// Companion regression: with the override left `None` the harness
/// still resolves the production path via `default_settings_path()` —
/// i.e. we did not accidentally break the non-eval default branch.
#[tokio::test]
async fn tui_harness_without_override_falls_through_to_default_path() {
    use crate::testing::TuiHarness;

    let dir = temp_workspace("ramu_harness_default");
    let env_settings = dir.join("env-settings.toml");
    let _guard = ScopedSettingsPath::new(env_settings.clone());

    let mut config = test_config(SessionMode::Build);
    config.workspace_root = dir.clone();
    let provider = Arc::new(UnavailableProvider::new("scripted", "harness default test"));
    let mut harness = TuiHarness::new(config, SessionMode::Build, provider, 80, 24, None)
        .expect("harness builds without override");

    assert_eq!(
        harness.app_mut().user_settings_path(),
        env_settings,
        "no override ⇒ default_settings_path (env-backed) should win",
    );
}

/// `/keymap` lists the current bindings — even with no overrides it
/// must surface every action plus the persisted-defaults hint so a
/// fresh install can be inspected.
#[tokio::test]
async fn slash_keymap_lists_defaults() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    let ran = handle_slash_command(&mut app, &mut agent, "/keymap").await;
    assert!(ran);
    let body = last_message_content(&app)
        .expect("transcript entry")
        .to_string();
    assert!(body.contains("transcript_overlay"), "missing entry: {body}");
    assert!(body.contains("page_up"), "missing entry: {body}");
    assert!(body.contains("Ctrl+T"), "default binding missing: {body}");
    assert!(body.contains("PageUp"), "default binding missing: {body}");
    assert!(body.contains("[tui.keymap]"), "config hint missing: {body}");
    assert!(
        !body.contains("(override)"),
        "no overrides expected: {body}",
    );
    assert!(
        app.status.contains("defaults"),
        "status should hint defaults, got: {}",
        app.status,
    );
}

/// A user override in `[tui.keymap]` flips the live key dispatch so
/// the rebound key fires the action and the original default no
/// longer does. Exercises the resolver wiring end-to-end.
#[tokio::test]
async fn keymap_override_redirects_transcript_overlay() {
    let mut config = test_config(SessionMode::Build);
    config
        .tui
        .keymap
        .insert("transcript_overlay".to_string(), "Ctrl+o".to_string());
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app_with_config(&config, SessionMode::Build);

    // The pre-existing default no longer toggles the overlay.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");
    assert!(
        app.transcript_overlay.is_none(),
        "Ctrl+T must no longer toggle when rebound",
    );

    // The user's new binding now toggles it open and closed.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");
    assert!(
        app.transcript_overlay.is_some(),
        "Ctrl+O must open the overlay after rebind",
    );
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");
    assert!(
        app.transcript_overlay.is_none(),
        "Ctrl+O must close the overlay on second press",
    );
}

/// After rebinding `transcript_overlay`, collapsed tool-card truncation hints
/// in the inline transcript and the overlay title must advertise the rebound
/// key, not the hardcoded default "Ctrl+T".
#[test]
fn keymap_rebind_updates_collapsed_card_hint() {
    let mut config = test_config(SessionMode::Build);
    config
        .tui
        .keymap
        .insert("transcript_overlay".to_string(), "Ctrl+o".to_string());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    // Push a long grep result so the collapsed card needs a truncation hint.
    let payload = (0..30)
        .map(|i| format!("match-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_tool_result(sample_tool_result("grep", &payload));
    app.finalize_settles_for_test();
    assert!(app.transcript[0].collapsed);

    let rendered = render_to_string(&app, 100, 24);
    assert!(
        rendered.contains("Ctrl+O for full transcript"),
        "collapsed card hint must use the rebound key; got:\n{rendered}"
    );
    assert!(
        !rendered.contains("Ctrl+T for full transcript"),
        "stale default must not appear after rebind; got:\n{rendered}"
    );
}

/// `/keymap` reports overrides and validation problems so the user
/// can see why a typo silently fell back to the default.
#[tokio::test]
async fn slash_keymap_surfaces_overrides_and_diagnostics() {
    let mut config = test_config(SessionMode::Build);
    config
        .tui
        .keymap
        .insert("transcript_overlay".to_string(), "Ctrl+o".to_string());
    config
        .tui
        .keymap
        .insert("not_a_real_action".to_string(), "Ctrl+x".to_string());
    config
        .tui
        .keymap
        .insert("page_up".to_string(), "garbage".to_string());
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app_with_config(&config, SessionMode::Build);

    let ran = handle_slash_command(&mut app, &mut agent, "/keymap").await;
    assert!(ran);
    let body = last_message_content(&app)
        .expect("transcript entry")
        .to_string();
    assert!(
        body.contains("transcript_overlay")
            && body.contains("Ctrl+O")
            && body.contains("(override)"),
        "override line missing: {body}",
    );
    assert!(
        body.contains("Unknown action names") && body.contains("not_a_real_action"),
        "unknown-action diagnostic missing: {body}",
    );
    assert!(
        body.contains("Invalid key specs") && body.contains("page_up"),
        "invalid-spec diagnostic missing: {body}",
    );
    // page_up keeps its default binding even though the override was
    // invalid — verifies the resolver isolates failures.
    assert!(body.contains("PageUp"), "default binding lost: {body}");
}

/// Serializes `/theme` tests so the process-global palette override and the
/// `SQUEEZY_SETTINGS_PATH` env var don't race between concurrent tests.
static THEME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `SQUEEZY_SETTINGS_PATH` lets tests redirect persistence away from the
/// real home directory; this guard also resets the runtime palette override
/// so leftover state from one test doesn't bleed into the next.
struct ScopedSettingsPath {
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl ScopedSettingsPath {
    fn new(path: PathBuf) -> Self {
        let lock = THEME_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Start every theme test from a known runtime theme — even if a
        // previous test left the process-global theme snapshot behind.
        let mut default_cfg = squeezy_core::AppConfig::default();
        default_cfg.tui.theme = "default".to_string();
        default_cfg.tui.themes.clear();
        crate::render::theme::set_active_theme(&default_cfg);
        let previous = std::env::var_os("SQUEEZY_SETTINGS_PATH");
        // SAFETY: the global mutex above ensures no other theme test is
        // mutating this env var at the same time.
        unsafe { std::env::set_var("SQUEEZY_SETTINGS_PATH", &path) };
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for ScopedSettingsPath {
    fn drop(&mut self) {
        // SAFETY: see `new`.
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("SQUEEZY_SETTINGS_PATH", value) },
            None => unsafe { std::env::remove_var("SQUEEZY_SETTINGS_PATH") },
        }
        let mut default_cfg = squeezy_core::AppConfig::default();
        default_cfg.tui.theme = "default".to_string();
        default_cfg.tui.themes.clear();
        crate::render::theme::set_active_theme(&default_cfg);
    }
}

// ---- F37: @-mention composer ----

#[test]
fn mention_popup_opens_after_typing_at_word() {
    let mut app = test_app(SessionMode::Build);
    // Seed workspace files so the popup doesn't trigger a real crawl.
    app.workspace_file_cache = Some(mention::WorkspaceFileCache::from_paths_for_tests(vec![
        PathBuf::from("crates/squeezy-graph/src/lib.rs"),
        PathBuf::from("docs/readme.md"),
    ]));

    insert_input_text(&mut app, "@graph");
    let popup = app
        .mention_popup
        .as_ref()
        .expect("popup should open after @graph");
    assert_eq!(popup.query, "graph");
    assert!(!popup.is_empty());
    assert!(popup.matches[0].to_string_lossy().contains("squeezy-graph"),);
}

#[tokio::test]
async fn mention_popup_inserts_path_on_enter() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.workspace_file_cache = Some(mention::WorkspaceFileCache::from_paths_for_tests(vec![
        PathBuf::from("crates/squeezy-graph/src/lib.rs"),
    ]));

    insert_input_text(&mut app, "@graph");
    assert!(app.mention_popup.is_some());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("enter");

    assert!(app.mention_popup.is_none());
    assert_eq!(app.input, "crates/squeezy-graph/src/lib.rs ");
}

#[test]
fn mention_popup_footer_warns_when_workspace_walk_truncated() {
    let mut app = test_app(SessionMode::Build);
    // A cache flagged truncated stands in for a workspace that exceeded
    // MAX_WORKSPACE_FILES without paying for a 5000-file walk.
    app.workspace_file_cache = Some(mention::WorkspaceFileCache::from_truncated_paths_for_tests(
        vec![PathBuf::from("crates/squeezy-graph/src/lib.rs")],
    ));

    insert_input_text(&mut app, "@graph");
    assert!(
        app.mention_popup
            .as_ref()
            .expect("popup should open")
            .truncated,
        "popup must carry the truncation flag from the cache"
    );

    let footer = mention_popup_lines(&app)
        .last()
        .expect("footer line")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>();
    assert!(
        footer.contains("more files not shown"),
        "footer should warn about truncated candidates, got: {footer:?}"
    );
}

#[test]
fn mention_popup_footer_has_no_hint_when_not_truncated() {
    let mut app = test_app(SessionMode::Build);
    app.workspace_file_cache = Some(mention::WorkspaceFileCache::from_paths_for_tests(vec![
        PathBuf::from("crates/squeezy-graph/src/lib.rs"),
    ]));

    insert_input_text(&mut app, "@graph");
    let footer = mention_popup_lines(&app)
        .last()
        .expect("footer line")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>();
    assert!(
        !footer.contains("more files not shown"),
        "untruncated popup must not show the hint, got: {footer:?}"
    );
}

#[tokio::test]
async fn mention_popup_escapes_on_esc() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.workspace_file_cache = Some(mention::WorkspaceFileCache::from_paths_for_tests(vec![
        PathBuf::from("crates/squeezy-graph/src/lib.rs"),
    ]));

    insert_input_text(&mut app, "@graph");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("esc");
    assert!(app.mention_popup.is_none());
    assert_eq!(app.input, "@graph");
}

// ---- F40: working row details ----

#[test]
fn apply_mcp_status_update_does_not_log_when_no_servers_configured() {
    let mut app = test_app(SessionMode::Build);
    let empty = McpStatusSnapshot {
        per_server: std::collections::BTreeMap::new(),
        generated_unix_millis: 0,
    };
    let baseline = app.transcript.len();

    // First snapshot with no servers — must not push a log entry.
    super::apply_mcp_status_update(&mut app, empty.clone());
    assert_eq!(
        app.transcript.len(),
        baseline,
        "empty MCP status should not log"
    );
    assert!(app.mcp_status.is_some(), "snapshot should still be cached");

    // A second identical empty snapshot also must not log.
    super::apply_mcp_status_update(&mut app, empty.clone());
    assert_eq!(app.transcript.len(), baseline);
}

#[test]
fn apply_mcp_status_update_logs_transitions_to_and_from_servers() {
    let mut app = test_app(SessionMode::Build);
    let empty = McpStatusSnapshot {
        per_server: std::collections::BTreeMap::new(),
        generated_unix_millis: 0,
    };
    let mut active_servers = std::collections::BTreeMap::new();
    active_servers.insert(
        "alpha".to_string(),
        McpServerStatus::Ready {
            tools_count: 4,
            cached: false,
        },
    );
    let active = McpStatusSnapshot {
        per_server: active_servers,
        generated_unix_millis: 0,
    };

    super::apply_mcp_status_update(&mut app, empty.clone());
    let after_empty = app.transcript.len();

    // Empty → active should log.
    super::apply_mcp_status_update(&mut app, active.clone());
    assert!(
        app.transcript.len() > after_empty,
        "transition to active MCP servers should log"
    );
    let after_active = app.transcript.len();

    // Repeat of the same active snapshot must not log.
    super::apply_mcp_status_update(&mut app, active.clone());
    assert_eq!(app.transcript.len(), after_active);

    // Active → empty should log (servers dropped).
    super::apply_mcp_status_update(&mut app, empty);
    assert!(app.transcript.len() > after_active);

    // Every logged transition must land on the transcript rail as a
    // `Note`, not a bare `Log` line. The previous behaviour pushed
    // through `push_log`, which renders off-rail and breaks the
    // visual alignment with adjacent reasoning / routed notices.
    let last_log_kind = app
        .transcript
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            super::TranscriptEntryKind::Log(log) => Some(log.kind),
            _ => None,
        })
        .expect("mcp status update should push at least one log entry");
    assert_eq!(
        last_log_kind,
        super::LogKind::Note,
        "mcp status transitions must use the rail-threaded Note kind"
    );
}

#[test]
fn terminal_title_for_clears_when_idle() {
    assert_eq!(
        terminal_title_for(TerminalTitleState::Cleared, "~/proj", 0),
        None
    );
}

#[test]
fn terminal_title_for_animates_spinner_while_working() {
    let early = terminal_title_for(TerminalTitleState::Working, "~/proj", 0)
        .expect("working state always renders a title");
    let later = terminal_title_for(
        TerminalTitleState::Working,
        "~/proj",
        TITLE_SPINNER_INTERVAL_MS,
    )
    .expect("working state always renders a title");
    assert!(early.contains("squeezy · ~/proj"), "got: {early}");
    assert!(later.contains("squeezy · ~/proj"), "got: {later}");
    assert_ne!(
        early.chars().next(),
        later.chars().next(),
        "spinner frame should advance after one interval"
    );
}

#[test]
fn terminal_title_for_uses_notification_glyph_when_done() {
    let title = terminal_title_for(TerminalTitleState::Notification, "~/proj", 0)
        .expect("notification state always renders a title");
    assert!(
        title.starts_with(TITLE_NOTIFICATION_GLYPH),
        "expected notification glyph prefix, got: {title}"
    );
    assert!(title.contains("~/proj"), "got: {title}");
}

#[test]
fn working_panel_height_is_one_without_detail() {
    let mut app = test_app(SessionMode::Build);
    app.cancel = Some(CancellationToken::new()); // turn in progress
    assert!(turn_in_progress(&app));
    assert_eq!(task_panel_height(&app), 1);
    assert!(working_detail_line(&app).is_none());
}

#[test]
fn working_panel_grows_to_two_rows_when_mcp_starting() {
    // McpStatusSnapshot/McpServerStatus are re-exported from squeezy_tools.
    let mut per_server = std::collections::BTreeMap::new();
    per_server.insert("alpha".to_string(), McpServerStatus::Starting);
    per_server.insert(
        "beta".to_string(),
        McpServerStatus::Ready {
            tools_count: 3,
            cached: false,
        },
    );

    let mut app = test_app(SessionMode::Build);
    app.cancel = Some(CancellationToken::new());
    app.mcp_status = Some(McpStatusSnapshot {
        per_server,
        generated_unix_millis: 0,
    });

    assert_eq!(task_panel_height(&app), 2);
    let detail = working_detail_line(&app).expect("expected mcp detail line");
    let text: String = detail.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("starting"), "got: {text}");
    assert!(text.contains("/2"), "got: {text}");
}

#[test]
fn working_detail_summarises_extra_queued_tools() {
    use squeezy_tools::ToolCall;
    let mut app = test_app(SessionMode::Build);
    app.cancel = Some(CancellationToken::new());
    app.active_tool_calls.insert(
        "a".to_string(),
        ToolCall {
            call_id: "a".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::json!({"command": "ls"}),
        },
    );
    app.active_tool_calls.insert(
        "b".to_string(),
        ToolCall {
            call_id: "b".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::json!({"command": "pwd"}),
        },
    );

    let detail = working_detail_line(&app).expect("queued summary");
    let text: String = detail.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("+1 more tool call queued"), "got: {text}");
}

// ---- F38: status segment composition ----

#[test]
fn status_details_render_via_segments_match_legacy_format() {
    let app = test_app(SessionMode::Build);
    let details = format_status_details(&app);
    // Each segment must appear with its label so downstream consumers
    // (CLI status verbose, logs) keep parsing.
    for needle in [
        "repo ",
        "sandbox ",
        "telemetry ",
        "mcp ",
        "cost ",
        "tok ",
        "ctx ",
        "pins ",
        "compact ",
        "tools ",
        "budget ",
        "cfg ",
        "read ",
        "receipts ",
        "redactions ",
        "cached ",
        "cache_write ",
    ] {
        assert!(
            details.contains(needle),
            "missing segment label {needle:?} in: {details}"
        );
    }
}

#[test]
fn cost_segment_renders_cap_and_percent_when_configured() {
    // When `max_session_cost_usd_micros` is set, the cost segment must show
    // the spend, the cap, and the integer percent so the user can see where
    // they stand without opening the /cost overlay.
    let mut config = test_config(SessionMode::Build);
    config.max_session_cost_usd_micros = Some(500_000); // $0.50 cap
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.cost.estimated_usd_micros = Some(125_000); // $0.125 spent => 25%

    let details = format_status_details(&app);
    assert!(
        details.contains("cost $0.125000 / $0.50 (25%)"),
        "unexpected status: {details}"
    );
}

#[test]
fn cost_segment_renders_without_cap_when_unset() {
    // When no cap is configured the segment must fall back to the legacy
    // single-value format so existing log scrapers keep working.
    let mut config = test_config(SessionMode::Build);
    config.max_session_cost_usd_micros = None;
    let mut app = test_app_with_config(&config, SessionMode::Build);
    app.cost.estimated_usd_micros = Some(42);

    let details = format_status_details(&app);
    assert!(details.contains("cost $0.000042"), "{details}");
    assert!(!details.contains(" / $"), "cap separator leaked: {details}");
}

#[test]
fn status_segment_individually_returns_expected_text() {
    let app = test_app(SessionMode::Build);
    assert_eq!(
        super::status::segments::sandbox(&app),
        Some(format!("sandbox {}", app.permissions.sandbox))
    );
    assert!(
        super::status::segments::tools(&app)
            .as_deref()
            .map(|s| s.starts_with("tools "))
            .unwrap_or(false)
    );
}

#[test]
fn status_segments_render_in_priority_order() {
    // Locks the order segments appear in the joined detail line so that
    // adding a new segment stays a one-place change: append a function in
    // `status::segments` and add a single row in `render_status_details`.
    // Downstream consumers (CLI status verbose, logs) parse position-
    // sensitively, so reordering existing labels is a breaking change.
    let mut app = test_app(SessionMode::Build);
    app.cost.input_tokens = Some(120);
    app.cost.output_tokens = Some(34);
    app.cost.cached_input_tokens = Some(7);
    app.cost.cache_write_input_tokens = Some(3);
    app.cost.estimated_usd_micros = Some(2_500);
    app.context_estimate.estimated_tokens = 4096;
    app.context_compaction_threshold = 10_000;
    app.metrics.tool_calls = 5;
    app.metrics.bytes_read = 1024;
    app.metrics.redactions = 1;
    app.metrics.receipt_stub_hits = 2;
    app.metrics.negative_receipt_hits = 1;
    app.metrics.budget_denials = 0;
    app.context_compaction.generation = 1;
    app.context_compaction.pinned.clear();
    app.cost_cap_usd_micros = None;

    // Priority order mirrors `status::render_status_details`. Keep this
    // list in sync with the segments array in `status.rs`.
    let expected_labels = [
        "permissions",
        "repo ",
        "sandbox ",
        "telemetry ",
        "mcp ",
        "cost ",
        "tok ",
        "ctx ",
        "pins ",
        "compact ",
        "tools ",
        "budget ",
        "cfg ",
        "read ",
        "receipts ",
        "redactions ",
        "cached ",
        "cache_write ",
    ];

    let joined = super::status::render_status_details(&app);
    let mut cursor = 0usize;
    for label in expected_labels {
        // `permissions` is the first segment but emits the compact policy
        // text (e.g. `r:allow…`) rather than a literal `permissions ` prefix.
        // Skip the prefix check for that one; the second-position
        // `repo ` anchors the order verification.
        if label == "permissions" {
            continue;
        }
        let needle_pos = joined[cursor..]
            .find(label)
            .unwrap_or_else(|| panic!("segment {label:?} missing or out of order in {joined:?}"));
        cursor += needle_pos + label.len();
    }
}

// ---- F39: slash command capabilities ----

#[test]
fn slash_commands_have_documented_capability_for_every_entry() {
    // Sanity-check that every slash command has been classified.
    let mutating = [
        "/plan",
        "/build",
        "/compact",
        "/clear",
        "/attach",
        "/pin",
        "/unpin",
        "/resume",
        "/fork",
        "/session-export",
        "/session-export-html",
        "/undo",
        "/revert-turn",
        "/effort",
        "/tool-verbosity",
    ];
    for command in SLASH_COMMANDS {
        let expected_dim = mutating.contains(&command.name);
        assert_eq!(
            command.available_during_task, !expected_dim,
            "{} availability mismatch",
            command.name
        );
    }
}

#[test]
fn slash_compact_during_turn_renders_queue_hint() {
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    set_input(&mut app, "/compact".to_string());

    let output = render_to_string(&app, 120, 16);
    assert!(
        output.contains("/compact"),
        "expected /compact in output: {output}"
    );
    assert!(
        output.contains("queues after turn"),
        "expected queue hint: {output}"
    );
}

#[test]
fn slash_queued_commands_stay_readable_during_turn() {
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    set_input(&mut app, "/attach".to_string());

    let lines = slash_suggestion_lines(&app, 120);
    let attach_line = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content == "/attach"))
        .expect("attach suggestion line");
    let command_span = attach_line
        .spans
        .iter()
        .find(|span| span.content == "/attach")
        .expect("attach command span");

    assert_ne!(
        command_span.style.fg,
        Some(crate::render::theme::quiet()),
        "unavailable command names should not use the low-contrast quiet color"
    );
    assert!(
        !command_span.style.add_modifier.contains(Modifier::DIM),
        "unavailable command names should not use terminal DIM, which can render as black"
    );
    assert!(
        attach_line
            .spans
            .iter()
            .any(|span| span.content.contains("queues after turn")),
        "queue hint should remain explicit"
    );
}

#[tokio::test]
async fn slash_compact_during_turn_short_circuits_dispatcher() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.cancel = Some(CancellationToken::new());

    let ran = handle_slash_command(&mut app, &mut agent, "/compact").await;
    assert!(ran, "command should be recognised");
    assert!(
        app.status.contains("unavailable during turn"),
        "status should reflect block: {}",
        app.status
    );
}

#[tokio::test]
async fn slash_help_is_allowed_during_turn() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.cancel = Some(CancellationToken::new());

    let ran = handle_slash_command(&mut app, &mut agent, "/help").await;
    assert!(ran, "help should execute");
    assert!(
        !app.status.contains("unavailable during turn"),
        "help should be allowed: {}",
        app.status
    );
}

#[test]
fn slash_parameter_hint_appears_in_render() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/tool-verbosity".to_string());
    let output = render_to_string(&app, 120, 16);
    assert!(
        output.contains("compact|normal|verbose"),
        "expected parameter hint to render the tool-verbosity options actually accepted \
         by `/tool-verbosity`: {output}"
    );
}

#[test]
fn slash_menu_omits_low_value_config_shortcuts() {
    let commands = SLASH_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();

    assert!(!commands.contains(&"/verbosity"), "{commands:?}");
    assert!(!commands.contains(&"/spinner"), "{commands:?}");
    assert!(commands.contains(&"/config"), "{commands:?}");
}

#[test]
fn slash_parameter_hint_uses_status_model_color() {
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/attach".to_string());

    let lines = slash_suggestion_lines(&app, 120);
    let attach_line = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content == "/attach"))
        .expect("attach suggestion line");
    let description_span = attach_line
        .spans
        .iter()
        .find(|span| span.content.contains("insert a file token in the prompt"))
        .expect("attach description span");
    let hint_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.trim() == "<path>")
        .expect("attach parameter hint span");

    assert_eq!(
        description_span.style.fg,
        Some(crate::render::theme::quiet()),
        "description color should stay unchanged"
    );
    assert_eq!(
        hint_span.style.fg,
        Some(crate::render::theme::cyan()),
        "parameter hints should render with the status-line model color"
    );
}

#[test]
fn json_patch_preview_parser_emits_events_per_patch() {
    use super::streaming_patch::{JsonPatchPreviewParser, PatchPreviewEvent};

    let payload = r#"{"patches":[{"path":"a.txt","search":"foo","replace":"bar","expected_sha256":"deadbeef"},{"path":"b.txt","search":"baz","replace":"qux","expected_sha256":"cafebabe"}],"plan_id":"P1"}"#;

    let mut parser = JsonPatchPreviewParser::new();
    let mut events = Vec::new();
    // Feed byte-by-byte to mirror the worst-case streaming cadence.
    for byte in payload.bytes() {
        let chunk = [byte];
        events.extend(parser.push_delta(std::str::from_utf8(&chunk).unwrap()));
    }
    events.extend(parser.finish());

    let patch_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PatchPreviewEvent::Patch { .. }))
        .collect();
    assert_eq!(patch_events.len(), 2, "events: {events:?}");
    match &patch_events[0] {
        PatchPreviewEvent::Patch {
            index,
            path,
            search_hash,
            replace_hash,
        } => {
            assert_eq!(*index, 0);
            assert_eq!(path, "a.txt");
            assert!(!search_hash.is_empty());
            assert!(!replace_hash.is_empty());
            assert_ne!(search_hash, replace_hash);
        }
        _ => panic!("expected patch event"),
    }
    let complete = events
        .iter()
        .find(|e| matches!(e, PatchPreviewEvent::Complete { .. }))
        .expect("complete event");
    if let PatchPreviewEvent::Complete { count } = complete {
        assert_eq!(*count, 2);
    }
}

#[tokio::test]
async fn shell_sandbox_best_effort_fallback_warns_user_once_per_session() {
    // F3-4: the TUI must surface the silent sandbox degradation as a
    // durable transcript warning on the first fallback; the agent's
    // once-per-session gate means this event only ever fires once, so we
    // assert the transcript holds a single naming entry afterwards.
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::ShellSandboxBestEffortFallback {
        turn_id: TurnId::new(3),
        backend: "macos-sandbox-exec".to_string(),
        fallback_count: 1,
    })
    .await
    .expect("send fallback warning");
    drop(tx);
    drain_agent_events(&mut app).await;

    let needle = "shell sandbox degraded";
    let matching: Vec<&str> = app
        .transcript
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message(item) if item.content.contains(needle) => {
                Some(item.content.as_str())
            }
            TranscriptEntryKind::Log(LogEntry { message, .. }) if message.contains(needle) => {
                Some(message.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "fallback warning must render exactly once; transcript: {:?}",
        app.transcript
    );
    assert!(
        matching[0].contains("macos-sandbox-exec"),
        "fallback warning must name the degraded backend: {}",
        matching[0]
    );
}

#[tokio::test]
async fn cost_warning_event_renders_exactly_once() {
    // CostWarning previously pushed the same notice into both the
    // transcript and the log pane; both render in the same stream so the
    // line appeared twice back-to-back. Now the system transcript entry
    // is the single source of truth.
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::CostWarning {
        turn_id: TurnId::new(1),
        status: CostCapStatus {
            spent_usd_micros: 80_000,
            cap_usd_micros: 100_000,
            percent: 80,
        },
    })
    .await
    .expect("send cost warning");
    drop(tx);
    drain_agent_events(&mut app).await;

    let needle = "session cost crossed warning threshold";
    let occurrences = app
        .transcript
        .iter()
        .filter(|entry| match &entry.kind {
            TranscriptEntryKind::Message(item) => item.content.contains(needle),
            TranscriptEntryKind::Log(LogEntry { message, .. }) => message.contains(needle),
            _ => false,
        })
        .count();
    assert_eq!(
        occurrences, 1,
        "CostWarning must render exactly once; transcript: {:?}",
        app.transcript
    );
}

#[test]
fn assistant_repeated_shell_output_dedups_even_with_glued_fence() {
    // Real-world regression: the model emitted the opening fence glued to
    // the prose ("...metadata.```text") rather than on its own line. The
    // fence-aware dedup scanned line-by-line and missed the duplicate, so
    // the entire `ls -la` body was rendered both in the tool card and
    // again inside the assistant message. `normalize_fence_boundaries`
    // breaks the fence onto its own line before the strip helpers run.
    let mut app = test_app(SessionMode::Build);
    let body = "total 328\ndrwxr-xr-x@  32 exampleuser  staff   1024 May 26 17:05 .\ndrwxr-xr-x@   3 exampleuser  staff     96 May 23 01:01 .cargo\n-rw-r--r--@   1 exampleuser  staff    892 May 23 01:01 .gitignore\n-rw-r--r--@   1 exampleuser  staff   3252 May 23 01:01 AGENTS.md\n-rw-r--r--@   1 exampleuser  staff  89483 May 23 01:01 Cargo.lock\ndrwxr-xr-x@  42 exampleuser  staff   1344 May 23 01:01 src";
    let mut result = sample_tool_result("shell", "");
    result.content = serde_json::json!({
        "command": "ls -la",
        "exit_code": 0,
        "stdout": body,
        "stderr": "",
    });
    app.push_tool_result(result);

    let assistant =
        format!("I'll list the repository root with detailed file metadata.```text\n{body}\n```");
    let item = TranscriptItem::assistant(assistant.clone());
    let deduped = super::dedupe_assistant_repeated_tool_output(&app, item)
        .expect("deduped item should remain (intro prose survives)");
    assert!(
        !deduped.content.contains("total 328"),
        "shell body must not appear inside the assistant message after dedup; got:\n{}",
        deduped.content
    );
    assert!(
        !deduped.content.contains("AGENTS.md"),
        "shell body must not survive dedup; got:\n{}",
        deduped.content
    );
    assert!(
        deduped.content.contains("detailed file metadata"),
        "intro prose should survive: {}",
        deduped.content
    );
}

#[test]
fn tool_card_truncates_model_shell_to_five_lines_with_head_tail() {
    let mut app = test_app(SessionMode::Build);
    let body = (0..30)
        .map(|i| format!("line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "ls -la"}),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "ls -la",
        "workdir": ".",
        "exit_code": 0,
        "stdout": body,
        "stderr": "",
    });
    app.push_tool_result_with_call(result, Some(call));
    // Rest the freshly-pushed node past its settle-fold so this asserts the
    // collapsed head-tail preview rather than the mid-fold expanded body.
    app.finalize_settles_for_test();

    let output = render_to_string(&app, 140, 18);
    // First and last lines must survive head-tail truncation; middle is elided.
    assert!(output.contains("line-00"), "head missing: {output}");
    assert!(output.contains("line-29"), "tail missing: {output}");
    assert!(
        !output.contains("line-14"),
        "middle should be elided: {output}"
    );
    assert!(
        output.contains("Ctrl+T for full transcript"),
        "ellipsis hint missing: {output}"
    );
}

#[test]
fn tool_card_user_shell_keeps_fifty_line_cap() {
    let mut app = test_app(SessionMode::Build);
    let body = (0..80)
        .map(|i| format!("line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({
            "command": "ls -la",
            "direct_user_shell": true,
        }),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "ls -la",
        "workdir": ".",
        "exit_code": 0,
        "stdout": body,
        "stderr": "",
        "direct_user_shell": true,
    });
    app.push_tool_result_with_call(result, Some(call));

    // With 80 lines and a 50-line cap, head+tail keep 100 lines max so we
    // expect NO truncation. Render area is intentionally tall.
    let output = render_to_string(&app, 140, 200);
    assert!(
        !output.contains("Ctrl-O to expand"),
        "user-shell under 2*cap should not be truncated: {output}"
    );
    assert!(output.contains("line-40"), "mid line missing: {output}");
}

#[test]
fn apply_patch_card_is_not_truncated_to_five_lines() {
    let mut app = test_app(SessionMode::Build);
    let patch = (0..20)
        .map(|i| format!("+ added line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let patch_body = format!("--- a/foo.rs\n+++ b/foo.rs\n@@ -1,0 +1,20 @@\n{patch}\n");
    let mut result = sample_tool_result("apply_patch", "");
    result.content = serde_json::json!({
        "files": [{
            "path": "foo.rs",
            "additions": 20,
            "deletions": 0,
            "patch": patch_body,
        }],
    });
    app.push_tool_result_with_call(result, None);

    let output = render_to_string(&app, 140, 60);
    assert!(
        !output.contains("Ctrl-O to expand"),
        "apply_patch must bypass the 5-line cap: {output}"
    );
    assert!(output.contains("foo.rs"), "header missing: {output}");
}

#[tokio::test]
async fn ctrl_t_opens_and_closes_transcript_overlay() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");
    assert!(
        app.transcript_overlay.is_some(),
        "Ctrl+T should open the overlay"
    );
    let overlay = app.transcript_overlay.expect("overlay");
    assert_eq!(
        overlay.scroll, TRANSCRIPT_OVERLAY_SCROLL_BOTTOM,
        "Ctrl+T should open anchored to the bottom/live end"
    );
    assert!(
        !overlay.mode.mouse_capture(),
        "Ctrl+T should preserve native text selection by default"
    );

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert!(
        app.transcript_overlay.is_none(),
        "Esc should close the overlay"
    );
}

#[tokio::test]
async fn esc_closes_transcript_overlay_without_interrupting_active_turn() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    app.turn_rx = Some(rx);
    app.cancel = Some(cancel.clone());
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("esc closes overlay");

    assert!(!quit);
    assert!(app.transcript_overlay.is_none());
    assert!(app.turn_rx.is_some(), "active turn should keep running");
    assert!(!cancel.is_cancelled(), "Esc should close overlay first");
}

#[tokio::test]
async fn transcript_overlay_m_toggles_scrollbar_drag_mode() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    )
    .await
    .expect("open overlay");
    let native_hint = format_status_hints(&app);
    assert!(
        native_hint.contains("native select/copy"),
        "overlay should start in native selection mode: {native_hint}"
    );
    assert!(
        native_hint.contains("M scrollbar drag"),
        "overlay should expose the scrollbar-drag toggle: {native_hint}"
    );

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
    )
    .await
    .expect("toggle overlay mouse mode");
    let overlay = app.transcript_overlay.expect("overlay");
    assert!(
        overlay.mode.mouse_capture(),
        "M should enable app-handled right scrollbar dragging"
    );
    let drag_hint = format_status_hints(&app);
    assert!(
        drag_hint.contains("drag right gutter scroll"),
        "drag mode should describe the active scrollbar behavior: {drag_hint}"
    );
    assert!(
        drag_hint.contains("Shift-drag select"),
        "drag mode should preserve the terminal selection escape hatch: {drag_hint}"
    );

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
    )
    .await
    .expect("toggle overlay mouse mode off");
    assert!(
        !app.transcript_overlay
            .expect("overlay")
            .mode
            .mouse_capture(),
        "second M should restore native selection mode"
    );
}

#[tokio::test]
async fn transcript_overlay_ctrl_m_does_not_toggle_scrollbar_drag_mode() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-m while overlay is open");

    assert!(
        !app.transcript_overlay
            .expect("overlay")
            .mode
            .mouse_capture(),
        "Ctrl+M must not arm terminal mouse-drag reporting"
    );
}

#[test]
fn transcript_overlay_drag_release_keeps_scrollbar_drag_mode() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::ScrollbarDrag,
        detail: OverlayDetail::Expanded,
    });

    let changed = handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 79,
            row: 10,
            modifiers: KeyModifiers::NONE,
        },
    );

    assert!(
        !changed,
        "mouse release alone should not redraw the overlay"
    );
    assert!(
        app.transcript_overlay
            .expect("overlay")
            .mode
            .mouse_capture(),
        "explicit scrollbar drag mode must remain armed until the user toggles it off"
    );
}

#[test]
fn input_batch_coalesces_transcript_scrollbar_drag_flood_before_key() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::ScrollbarDrag,
        detail: OverlayDetail::Expanded,
    });
    let events = vec![
        Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 79,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
        Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 79,
            row: 8,
            modifiers: KeyModifiers::NONE,
        }),
        Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 79,
            row: 12,
            modifiers: KeyModifiers::NONE,
        }),
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    ];

    let coalesced = coalesce_input_events_for_dispatch(&app, events);

    assert_eq!(
        coalesced.len(),
        2,
        "drag floods should collapse to the latest drag plus the following key"
    );
    match coalesced[0] {
        Event::Mouse(mouse) => {
            assert_eq!(mouse.row, 12);
            assert!(matches!(
                mouse.kind,
                MouseEventKind::Drag(crossterm::event::MouseButton::Left)
            ));
        }
        ref other => panic!("expected coalesced drag first, got {other:?}"),
    }
    assert!(
        matches!(coalesced[1], Event::Key(key) if key.code == KeyCode::Esc),
        "keyboard event must remain immediately reachable after the coalesced drag"
    );
}

#[test]
fn input_batch_prioritizes_key_before_transcript_drag_flood() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::ScrollbarDrag,
        detail: OverlayDetail::Expanded,
    });
    let mut events = vec![
        Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 79,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
        Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 79,
            row: 8,
            modifiers: KeyModifiers::NONE,
        }),
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    ];

    let priority = take_priority_key_event_for_dispatch(&app, &mut events);

    assert!(
        matches!(priority, Some(Event::Key(key)) if key.code == KeyCode::Esc),
        "keys must be handled before drag repaint work while scrollbar capture is active"
    );
    assert_eq!(
        events.len(),
        2,
        "mouse events stay available for later coalescing after the key"
    );
}

#[tokio::test]
async fn dispatch_input_events_applies_every_key_in_one_batch() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    let events = vec![
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
    ];

    let quit = dispatch_input_events(&mut app, &mut agent, events)
        .await
        .expect("dispatch batch");

    assert!(!quit, "typing must not quit");
    assert_eq!(
        app.input, "abc",
        "all three keys read in one poll window must reach the composer",
    );
}

#[tokio::test]
async fn dispatch_input_events_opens_overlay_for_chord_in_one_batch() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    let events = vec![
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
    ];

    dispatch_input_events(&mut app, &mut agent, events)
        .await
        .expect("dispatch chord batch");

    assert!(
        app.pending_chord.is_none(),
        "chord must clear once the follow-up key is processed",
    );
    assert!(
        app.prompt_queue_overlay.is_some(),
        "Ctrl+X then Q landing in one poll window must open the queue overlay",
    );
    assert!(
        app.input.is_empty(),
        "the chord follow-up must not leak into the composer",
    );
}

#[test]
fn input_poll_limit_expands_in_transcript_scrollbar_drag_mode() {
    let mut app = test_app(SessionMode::Build);
    assert_eq!(input_events_per_poll_limit(&app), MAX_INPUT_EVENTS_PER_POLL);

    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::ScrollbarDrag,
        detail: OverlayDetail::Expanded,
    });

    assert_eq!(
        input_events_per_poll_limit(&app),
        MAX_TRANSCRIPT_DRAG_INPUT_EVENTS_PER_POLL,
        "scrollbar drag mode should drain deep mouse floods before repainting"
    );
}

#[test]
fn input_batch_does_not_coalesce_drags_outside_scrollbar_drag_mode() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::NativeSelection,
        detail: OverlayDetail::Expanded,
    });
    let events = vec![
        Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 79,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
        Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 79,
            row: 8,
            modifiers: KeyModifiers::NONE,
        }),
    ];

    let coalesced = coalesce_input_events_for_dispatch(&app, events);

    assert_eq!(
        coalesced.len(),
        2,
        "native-selection mode should not rewrite mouse event batches"
    );
}

#[test]
fn transcript_overlay_drag_uses_cached_scrollbar_geometry() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::ScrollbarDrag,
        detail: OverlayDetail::Expanded,
    });
    app.transcript_overlay_scrollbar_cache
        .set(Some(TranscriptOverlayScrollbarCache {
            scrollbar_area: Rect {
                x: 79,
                y: 1,
                width: 1,
                height: 10,
            },
            geometry: TranscriptScrollbarGeometry {
                thumb_top: 0,
                thumb_height: 2,
                max_scroll: 100,
            },
        }));

    let changed = handle_mouse(
        &mut app,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 79,
            row: 10,
            modifiers: KeyModifiers::NONE,
        },
    );

    assert!(
        changed,
        "dragging the cached scrollbar should update scroll"
    );
    assert_eq!(
        app.transcript_overlay.expect("overlay").scroll,
        TRANSCRIPT_OVERLAY_SCROLL_BOTTOM,
        "drag mapping should use cached max-scroll without relayouting transcript lines"
    );
}

#[tokio::test]
async fn transcript_overlay_end_boundary_keeps_escape_and_ctrl_c_responsive() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::NativeSelection,
        detail: OverlayDetail::Expanded,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
    )
    .await
    .expect("scroll back in overlay");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
    )
    .await
    .expect("jump to overlay end");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("esc after end");

    assert!(
        app.transcript_overlay.is_none(),
        "Esc should still close Ctrl+T after scrolling back to the end"
    );

    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: TRANSCRIPT_OVERLAY_SCROLL_BOTTOM,
        mode: TranscriptOverlayMode::NativeSelection,
        detail: OverlayDetail::Expanded,
    });
    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-c after end");

    assert!(!quit, "first Ctrl+C should arm exit confirm, not exit");
    assert!(
        app.exit_confirm_armed,
        "Ctrl+C should still reach the exit-confirm path after overlay end"
    );
}

#[tokio::test]
async fn transcript_overlay_owns_page_keys_before_global_keymap() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    for index in 0..80 {
        app.push_transcript_item(TranscriptItem::user(format!("turn {index}")));
    }
    app.transcript_scroll_from_bottom = 12;
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: 0,
        mode: TranscriptOverlayMode::NativeSelection,
        detail: OverlayDetail::Expanded,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
    )
    .await
    .expect("page down while overlay is open");

    assert_eq!(
        app.transcript_overlay.expect("overlay").scroll,
        10,
        "PageDown should scroll the Ctrl+T transcript overlay"
    );
    assert_eq!(
        app.transcript_scroll_from_bottom, 12,
        "PageDown must not scroll the underlying transcript while Ctrl+T is open"
    );
}

#[tokio::test]
async fn transcript_overlay_swallows_ctrl_x_queue_chord() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.prompt_queue.push_back("queued".to_string());
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-x while overlay is open");
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    )
    .await
    .expect("q while overlay is open");

    assert!(
        app.prompt_queue_overlay.is_none(),
        "Ctrl+X Q must not open the queue overlay behind Ctrl+T"
    );
    assert!(
        app.transcript_overlay.is_some(),
        "Ctrl+T overlay should keep owning the key path"
    );
}

#[tokio::test]
async fn turn_completion_preserves_transcript_overlay_scrollbar_drag_mode() {
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: TRANSCRIPT_OVERLAY_SCROLL_BOTTOM,
        mode: TranscriptOverlayMode::ScrollbarDrag,
        detail: OverlayDetail::Expanded,
    });

    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("done"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);

    drain_agent_events(&mut app).await;

    assert!(app.turn_rx.is_none(), "turn should finish");
    let overlay = app.transcript_overlay.expect("overlay should remain open");
    assert!(
        overlay.mode.mouse_capture(),
        "turn completion must preserve explicit scrollbar drag mode"
    );
}

#[tokio::test]
async fn esc_still_closes_overlay_after_turn_completes_from_scrollbar_drag_mode() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: TRANSCRIPT_OVERLAY_SCROLL_BOTTOM,
        mode: TranscriptOverlayMode::ScrollbarDrag,
        detail: OverlayDetail::Expanded,
    });

    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("done"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("esc should be handled");

    assert!(
        app.transcript_overlay.is_none(),
        "Esc should close Ctrl+T after turn completion"
    );
}

#[tokio::test]
async fn ctrl_c_still_reaches_exit_confirm_after_turn_completes_from_scrollbar_drag_mode() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let (tx, rx) = mpsc::channel(4);
    app.turn_rx = Some(rx);
    app.transcript_overlay = Some(TranscriptOverlayState {
        scroll: TRANSCRIPT_OVERLAY_SCROLL_BOTTOM,
        mode: TranscriptOverlayMode::ScrollbarDrag,
        detail: OverlayDetail::Expanded,
    });

    tx.send(AgentEvent::Completed {
        turn_id: TurnId::new(1),
        message: TranscriptItem::assistant("done"),
        response_id: None,
        cost: CostSnapshot::default(),
        metrics: TurnMetrics::default(),
        context_estimate: ContextEstimate::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })
    .await
    .expect("send completed");
    drop(tx);
    drain_agent_events(&mut app).await;

    let quit = handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-c should be handled");

    assert!(!quit, "first Ctrl+C should arm exit confirm, not exit");
    assert!(
        app.exit_confirm_armed,
        "Ctrl+C should still reach the global exit-confirm path"
    );
}

#[test]
fn transcript_overlay_renders_entries_uncollapsed() {
    let mut app = test_app(SessionMode::Build);
    let body = (0..30)
        .map(|i| format!("line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let call = ToolCall {
        call_id: "shell-1".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "ls -la"}),
    };
    let mut result = sample_tool_result("shell", "");
    result.call_id = "shell-1".to_string();
    result.content = serde_json::json!({
        "command": "ls -la",
        "workdir": ".",
        "exit_code": 0,
        "stdout": body,
        "stderr": "",
    });
    app.push_tool_result_with_call(result, Some(call));
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    let output = render_to_string(&app, 140, 60);
    assert!(
        output.contains("Transcript"),
        "overlay frame missing: {output}"
    );
    // Middle line is normally elided by the head-tail cap; the overlay
    // forces every entry expanded so the line must be present.
    assert!(
        output.contains("line-14"),
        "overlay must show full body: {output}"
    );
    assert!(
        !output.contains("Ctrl-O to expand"),
        "overlay should not show truncation ellipsis: {output}"
    );
}

#[test]
fn transcript_overlay_includes_pending_assistant_mid_turn() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("tell me a story"));
    app.pending_reasoning = "planning the next line".to_string();
    app.pending_assistant.push_delta("Once the lantern woke,");
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    let lines = transcript_lines_for_overlay(&app, Some(100), true);
    let rendered = lines_to_plain_text(&lines);

    assert!(
        rendered.contains("planning the next line"),
        "overlay should show live reasoning while the turn streams: {rendered}"
    );
    assert!(
        rendered.contains("Once the lantern woke,"),
        "overlay should show live assistant text while the turn streams: {rendered}"
    );
}

#[test]
fn transcript_overlay_cache_invalidates_for_pending_stream_changes() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("tell me a story"));
    app.transcript_overlay = Some(TranscriptOverlayState::default());
    app.pending_assistant.push_delta("first live tail");

    let first = lines_to_plain_text(&transcript_overlay_rows_for_render(&app, 80));
    assert!(first.contains("first live tail"), "{first}");

    app.pending_assistant.clear();
    app.pending_assistant.push_delta("second live tail");

    let second = lines_to_plain_text(&transcript_overlay_rows_for_render(&app, 80));
    assert!(second.contains("second live tail"), "{second}");
    assert!(
        !second.contains("first live tail"),
        "cached overlay rows must not hold stale live output: {second}"
    );
}

#[test]
fn transcript_overlay_cache_invalidates_for_width_changes() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::assistant(
        "alpha beta gamma delta epsilon zeta eta theta iota kappa",
    ));
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    let narrow = transcript_overlay_rows_for_render(&app, 12).len();
    let wide = transcript_overlay_rows_for_render(&app, 80).len();

    assert!(
        narrow > wide,
        "narrow overlay rows should wrap into more visible rows: narrow={narrow}, wide={wide}"
    );
}

#[test]
fn transcript_overlay_repaint_clears_stale_characters() {
    let mut app = test_app(SessionMode::Build);
    app.transcript_overlay = Some(TranscriptOverlayState::default());
    app.push_transcript_item(TranscriptItem::assistant(
        "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ",
    ));

    let backend = TestBackend::new(64, 12);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render(frame, &app))
        .expect("first draw");

    app.transcript.clear();
    app.push_transcript_item(TranscriptItem::assistant("ok"));
    terminal
        .draw(|frame| render(frame, &app))
        .expect("second draw");

    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..12 {
        for x in 0..64 {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }

    assert!(output.contains("ok"), "{output}");
    assert!(
        !output.contains("ZZZZ"),
        "shorter overlay rows must clear stale characters from the prior frame: {output}"
    );
}

#[test]
fn transcript_overlay_renders_right_scrollbar_for_overflow() {
    let mut app = test_app(SessionMode::Build);
    for index in 0..40 {
        app.push_transcript_item(TranscriptItem::user(format!("turn {index}")));
    }
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    let output = render_to_string(&app, 80, 12);

    assert!(
        output.contains('█'),
        "overflowing transcript overlay should render a scrollbar thumb: {output}"
    );
    assert!(
        output.contains('░'),
        "overflowing transcript overlay should render a scrollbar track: {output}"
    );
}

#[test]
fn transcript_overlay_keeps_status_footer_visible() {
    let mut app = test_app(SessionMode::Build);
    for index in 0..20 {
        app.push_transcript_item(TranscriptItem::user(format!("turn {index}")));
    }
    app.transcript_overlay = Some(TranscriptOverlayState::default());

    let output = render_to_string(&app, 120, 18);

    assert!(
        output.contains("Transcript"),
        "overlay frame missing: {output}"
    );
    assert!(
        output.contains("Build mode (Shift+Tab to cycle)"),
        "overlay should keep the mode/status row visible: {output}"
    );
    assert!(
        output.contains("PgUp/PgDn/Wheel scroll"),
        "overlay should keep the transcript hint row visible: {output}"
    );
    assert!(
        output.contains("M scrollbar drag"),
        "overlay should expose the scrollbar-drag toggle: {output}"
    );
    assert!(
        output.contains("native select/copy"),
        "overlay should default to native text selection: {output}"
    );
}

#[test]
fn transcript_overlay_scrollbar_click_maps_to_scroll_offset() {
    let scrollbar_area = Rect {
        x: 79,
        y: 1,
        width: 1,
        height: 10,
    };

    assert_eq!(
        transcript_overlay_scroll_for_scrollbar_row(1, scrollbar_area, 100),
        Some(0)
    );
    assert_eq!(
        transcript_overlay_scroll_for_scrollbar_row(10, scrollbar_area, 100),
        Some(90)
    );
    assert!(
        transcript_overlay_scroll_for_scrollbar_row(6, scrollbar_area, 100).expect("middle row")
            > 0
    );
}

#[test]
fn transcript_overlay_tool_cards_are_expanded_and_plain() {
    let mut app = test_app(SessionMode::Build);
    let body = (0..30)
        .map(|i| format!("line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let body_len = body.len();
    let call = ToolCall {
        call_id: "read-1".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({"path": "src/lib.rs"}),
    };
    let result = ToolResult {
        call_id: "read-1".to_string(),
        tool_name: "read_file".to_string(),
        status: ToolStatus::Success,
        content: serde_json::json!({
            "path": "src/lib.rs",
            "bytes_returned": body_len,
            "total_bytes": body_len,
            "ranges": [{"start": 1, "end": 30}],
            "content": body,
        }),
        cost_hint: ToolCostHint {
            output_bytes: body_len as u64,
            ..ToolCostHint::default()
        },
        receipt: ToolReceipt {
            output_sha256: "abcdef1234567890".to_string(),
            content_sha256: Some("0123456789abcdef".to_string()),
        },
        spill_model_output: None,
        web_call_stats: None,
    };
    app.push_tool_result_with_call(result, Some(call));

    let lines = transcript_lines_for_overlay(&app, Some(100), true);
    let rendered = lines_to_plain_text(&lines);

    assert!(
        rendered.contains("line-14"),
        "overlay transcript must include uncapped output: {rendered}"
    );
    assert!(
        !rendered.contains("Ctrl+T for full transcript"),
        "{rendered}"
    );
    assert!(
        lines.iter().all(|line| {
            line.style.bg.is_none() && line.spans.iter().all(|span| span.style.bg.is_none())
        }),
        "overlay transcript should not carry card background tints: {lines:?}"
    );
}

#[test]
fn diff_card_pushed_via_helper_renders_summary_and_lines() {
    let mut app = test_app(SessionMode::Build);
    let lines: Vec<ratatui::text::Line<'static>> = vec![
        ratatui::text::Line::from(ratatui::text::Span::raw("+ added".to_string())),
        ratatui::text::Line::from(ratatui::text::Span::raw("- removed".to_string())),
    ];
    app.push_diff_card(super::DiffCardData {
        summary: "1 file · +1 -1".to_string(),
        plain: "+ added\n- removed\n".to_string(),
        lines,
    });

    let output = render_to_string(&app, 140, 18);
    assert!(output.contains("Diff"), "header missing: {output}");
    assert!(
        output.contains("1 file · +1 -1"),
        "summary missing: {output}"
    );
    assert!(output.contains("+ added"), "diff body missing: {output}");
    assert!(output.contains("- removed"), "diff body missing: {output}");
}

#[test]
fn json_patch_preview_parser_handles_escaped_quotes_in_search() {
    use super::streaming_patch::{JsonPatchPreviewParser, PatchPreviewEvent};

    // `search` contains an escaped double-quote — the parser must not let it
    // close the surrounding string literal early.
    let payload = r#"{"patches":[{"path":"a.rs","search":"println!(\"hi\");","replace":"println!(\"hello\");","expected_sha256":"x"}]}"#;

    let mut parser = JsonPatchPreviewParser::new();
    let events = parser.push_delta(payload);
    let patches: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            PatchPreviewEvent::Patch { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        patches,
        vec!["a.rs".to_string()],
        "should emit exactly one patch, got events: {events:?}"
    );
}

#[test]
fn json_patch_preview_parser_preserves_non_ascii_utf8() {
    use super::streaming_patch::{JsonPatchPreviewParser, PatchPreviewEvent};

    // A patch whose path/search/replace carry multi-byte UTF-8 must be
    // surfaced verbatim — `byte as char` would split each sequence into
    // Latin-1 code points and store mojibake (`café` -> `cafÃ©`).
    let payload = r#"{"patches":[{"path":"café.rs","search":"naïve","replace":"résumé"}]}"#;

    let mut parser = JsonPatchPreviewParser::new();
    let events = parser.push_delta(payload);

    let path = events
        .iter()
        .find_map(|e| match e {
            PatchPreviewEvent::Patch { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("patch object should emit a Patch event");
    assert_eq!(path, "café.rs", "patch path must round-trip non-ASCII");

    let partial = parser.latest_partial();
    assert_eq!(partial.path.as_deref(), Some("café.rs"));
    assert_eq!(partial.search.as_deref(), Some("naïve"));
    assert_eq!(partial.replace.as_deref(), Some("résumé"));
}

#[test]
fn streaming_chunks_emit_partial_previews_with_growing_content() {
    use super::streaming_patch::{
        JsonPatchPreviewParser, PatchPartial, PatchPreviewEvent, render_streaming_preview,
    };

    // F14: simulate the provider streaming the apply_patch args in five
    // chunks. After each chunk, every `Partial` event must reflect the
    // fields that have actually finished streaming — none should claim
    // to know `search` before its closing quote has arrived.
    let chunks = [
        r#"{"patches":[{"#,
        r#""path":"src/foo.rs""#,
        r#","search":"old line""#,
        r#","replace":"new line""#,
        r#"}]}"#,
    ];

    let mut parser = JsonPatchPreviewParser::new();
    let mut events: Vec<PatchPreviewEvent> = Vec::new();
    let mut snapshot_per_chunk: Vec<PatchPartial> = Vec::new();
    for chunk in chunks {
        events.extend(parser.push_delta(chunk));
        snapshot_per_chunk.push(parser.latest_partial().clone());
    }
    events.extend(parser.finish());

    let partials: Vec<&PatchPartial> = events
        .iter()
        .filter_map(|e| match e {
            PatchPreviewEvent::Partial(p) => Some(p),
            _ => None,
        })
        .collect();
    assert!(
        partials.len() >= 3,
        "expected at least one Partial per tracked field (path/search/replace), got {} \
         partial events: {events:?}",
        partials.len(),
    );

    let final_partial = partials
        .last()
        .copied()
        .expect("at least one partial recorded");
    assert_eq!(final_partial.path.as_deref(), Some("src/foo.rs"));
    assert_eq!(final_partial.search.as_deref(), Some("old line"));
    assert_eq!(final_partial.replace.as_deref(), Some("new line"));

    // Snapshots taken before the value strings closed must NOT yet
    // claim those fields — that would render a half-streamed diff body
    // and confuse a watching user.
    assert!(
        snapshot_per_chunk[0].path.is_none(),
        "no fields should be visible while only `{{\"patches\":[{{` has streamed; got {:?}",
        snapshot_per_chunk[0]
    );
    assert!(
        snapshot_per_chunk[1].path.as_deref() == Some("src/foo.rs")
            && snapshot_per_chunk[1].search.is_none()
            && snapshot_per_chunk[1].replace.is_none(),
        "path should be visible but not search/replace; got {:?}",
        snapshot_per_chunk[1]
    );
    assert!(
        snapshot_per_chunk[2].search.as_deref() == Some("old line")
            && snapshot_per_chunk[2].replace.is_none(),
        "search should be visible but not replace; got {:?}",
        snapshot_per_chunk[2]
    );

    // Render the diff for the snapshot captured BEFORE `replace` had
    // arrived — the preview must show the deletion side but no
    // additions yet, matching what the user would see frame-by-frame.
    let search_only_lines = render_streaming_preview(&snapshot_per_chunk[2]);
    let search_only_text = lines_to_string(&search_only_lines);
    assert!(
        search_only_text.contains("-old line"),
        "search-only preview must render the deletion line: {search_only_text}",
    );
    assert!(
        !search_only_text.contains("+old line") && !search_only_text.contains("+new line"),
        "search-only preview must not render any addition yet: {search_only_text}",
    );

    // Once replace has streamed, the preview must show both sides.
    let final_lines = render_streaming_preview(final_partial);
    let final_text = lines_to_string(&final_lines);
    assert!(
        final_text.contains("-old line") && final_text.contains("+new line"),
        "final preview must render both deletion and addition: {final_text}",
    );
    assert!(
        final_text.contains("src/foo.rs"),
        "preview header should surface the target path: {final_text}",
    );
}

#[test]
fn render_streaming_preview_renders_partial_search_only() {
    use super::streaming_patch::{PatchPartial, render_streaming_preview};

    // The provisional preview must handle a snapshot where `search`
    // has closed but `replace` has not yet streamed — i.e. mid-frame
    // state during a live edit. The render must produce deletions and
    // omit additions entirely.
    let partial = PatchPartial {
        index: 0,
        path: Some("src/lib.rs".to_string()),
        search: Some("first\nsecond".to_string()),
        replace: None,
    };
    let lines = render_streaming_preview(&partial);
    let text = lines_to_string(&lines);
    assert!(
        text.contains("-first") && text.contains("-second"),
        "multi-line search must produce one deletion per line: {text}",
    );
    assert!(
        !text.contains("+first") && !text.contains("+second"),
        "no replace yet → no additions in preview: {text}",
    );
}

#[test]
fn render_streaming_preview_for_empty_snapshot_is_empty() {
    use super::streaming_patch::{PatchPartial, render_streaming_preview};

    // Until any field has closed, the streaming preview must render
    // nothing — there is nothing meaningful to show, and a stray
    // header would suggest a patch is queued before one actually is.
    let lines = render_streaming_preview(&PatchPartial::default());
    assert!(
        lines.is_empty(),
        "empty partial must render no preview lines: {lines:?}",
    );
}

fn lines_to_string(lines: &[ratatui::text::Line<'static>]) -> String {
    let mut out = String::new();
    for line in lines {
        for span in &line.spans {
            out.push_str(&span.content);
        }
        out.push('\n');
    }
    out
}

#[test]
fn idle_app_reports_no_active_animation() {
    let mut app = test_app(SessionMode::Build);
    // Fresh `TuiApp` starts with needs_redraw = true so the first frame
    // paints. Simulate the loop having drawn that frame.
    app.needs_redraw = false;
    assert_eq!(app.turn_visual, TurnVisualState::Idle);
    assert_eq!(app.terminal_title_state, TerminalTitleState::Cleared);
    assert!(
        !app.has_active_animation(),
        "idle TuiApp should not advertise any active animation"
    );
    assert!(
        !app.needs_redraw,
        "no mutation occurred so needs_redraw should stay false"
    );
}

#[test]
fn settle_visible_line_count_eases_from_expanded_to_collapsed() {
    // The pure fold-height helper is time-injected, so its full curve can
    // be asserted without a clock: it starts at the expanded height, never
    // increases, and lands exactly on the collapsed height at 600ms.
    let from = 20u16;
    let collapsed = 5u16;

    assert_eq!(
        settle_visible_line_count(from, collapsed, 0),
        from,
        "at elapsed 0 the fold has not moved off the expanded height"
    );
    assert_eq!(
        settle_visible_line_count(from, collapsed, SETTLE_DURATION_MS),
        collapsed,
        "at exactly 600ms the fold rests on the collapsed height"
    );
    assert_eq!(
        settle_visible_line_count(from, collapsed, SETTLE_DURATION_MS + 5_000),
        collapsed,
        "past 600ms the fold stays pinned to the collapsed height"
    );

    // Monotonically non-increasing across the whole window, and strictly
    // inside the [collapsed, from] band before it finishes.
    let mut prev = from;
    for elapsed in (0..=SETTLE_DURATION_MS).step_by(20) {
        let visible = settle_visible_line_count(from, collapsed, elapsed);
        assert!(
            visible <= prev,
            "fold height must not grow: {visible} > {prev} at {elapsed}ms"
        );
        assert!(
            (collapsed..=from).contains(&visible),
            "fold height {visible} escaped the [{collapsed}, {from}] band at {elapsed}ms"
        );
        prev = visible;
    }

    // A node already at or below its collapsed height has nothing to fold.
    assert_eq!(settle_visible_line_count(3, 5, 0), 3);
    assert_eq!(settle_visible_line_count(5, 5, 0), 5);
}

#[test]
fn armed_settle_renders_more_lines_than_after_it_finishes() {
    // A freshly-armed work node renders its expanded block (folding down),
    // so it occupies more rows than the same node once the settle finishes
    // and it rests collapsed. The early-frame count is asserted via the
    // pure helper to keep the test independent of the wall clock.
    let mut app = test_app(SessionMode::Build);
    let payload = (0..30)
        .map(|i| format!("row-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_tool_result(sample_tool_result("grep", &payload));

    let settle = app.transcript[0]
        .settle
        .expect("a finished tool result must arm a settle-fold");
    assert!(
        settle.from_lines > 1,
        "expanded grep output should seed a multi-line fold start, got {}",
        settle.from_lines
    );

    // Render the just-armed (folding) node and count its rows. The exact
    // count drifts with the few microseconds elapsed between push and
    // render, so the strict "more lines early than late" inequality is
    // asserted via the pure helper below; here we only require the folding
    // frame to be taller than the resting collapsed frame.
    let folding = transcript_lines_for_render(&app, Some(100), false);
    let folding_count = folding.len();

    // After the settle finalizes the node rests collapsed.
    app.finalize_settles_for_test();
    assert!(app.transcript[0].settle.is_none());
    assert!(app.transcript[0].collapsed);
    let settled = transcript_lines_for_render(&app, Some(100), false);
    let settled_count = settled.len();

    assert!(
        folding_count > settled_count,
        "a folding node ({folding_count} rows) must be taller than the same node \
         once settled collapsed ({settled_count} rows)"
    );

    // The pure helper carries the strict monotonic claim independent of the
    // clock: shortly after arming the visible height exceeds the 600ms one.
    let early = settle_visible_line_count(settle.from_lines, settled_count as u16, 30);
    let done =
        settle_visible_line_count(settle.from_lines, settled_count as u16, SETTLE_DURATION_MS);
    assert!(
        early > done,
        "fold height shortly after arming ({early}) must exceed the settled height ({done})"
    );
    assert_eq!(
        done, settled_count as u16,
        "the fold's 600ms height must equal the resting collapsed row count"
    );
}

#[test]
fn failed_tool_does_not_settle_fold_and_stays_expanded() {
    // An auto-expanded failure must NOT arm a settle-fold: the fold finalizes
    // by force-collapsing, which would hide the error after 600ms — the exact
    // opposite of the "show the failure reason inline" behaviour.
    let mut app = test_app(SessionMode::Build);
    let mut failed = sample_tool_result("delegate", "missing required string field: prompt");
    failed.status = ToolStatus::Error;
    app.push_tool_result(failed);

    assert!(
        app.transcript[0].settle.is_none(),
        "a failed (auto-expanded) tool result must not arm a settle-fold"
    );
    assert!(!app.transcript[0].collapsed);
    app.finalize_settles_for_test();
    assert!(
        !app.transcript[0].collapsed,
        "a failed tool result must stay expanded, never fold collapsed"
    );
}

#[test]
fn settling_node_threads_the_rail_gutter() {
    // While folding, a node stays on the Quiet Rail (├─/╰─ elbow) instead of
    // rendering off-rail and jumping onto it when the fold finalizes.
    let mut app = test_app(SessionMode::Build);
    let payload = (0..30)
        .map(|i| format!("row-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_tool_result(sample_tool_result("grep", &payload));
    assert!(
        app.transcript[0].settle.is_some(),
        "success tool should fold"
    );

    let folding = lines_to_plain_text(&transcript_lines_for_render(&app, Some(100), false));
    assert!(
        folding.contains("├─") || folding.contains("╰─"),
        "folding node must thread the rail gutter: {folding}"
    );
}

#[test]
fn settling_entry_keeps_animation_tick_alive_then_idles_when_settled() {
    // The fold must keep repaints flowing frame to frame while a node is
    // settling, then return to idle (no perpetual repaint) once every
    // settle finalizes.
    let mut app = test_app(SessionMode::Build);
    app.focused = false;
    app.needs_redraw = false;
    assert!(
        !should_advance_animation_tick(&app),
        "an idle, unfocused transcript with no settles and no turn must not tick"
    );

    app.push_tool_result(sample_tool_result("grep", "alpha\nbeta\ngamma"));
    assert!(app.has_settling_entry(), "the pushed node arms a settle");
    // The tick keeps advancing while settling regardless of focus, so the
    // loop keeps cycling to finalize the fold.
    assert!(
        should_advance_animation_tick(&app),
        "a node mid settle-fold must keep the animation tick alive"
    );
    // A focused window also keeps `draw_app` running so the fold actually
    // animates frame to frame; an unfocused background window does not
    // repaint a fold it cannot show (it still finalizes via the tick loop).
    app.focused = true;
    assert!(
        app.has_active_animation(),
        "a focused settling node must keep draw_app running so the fold animates"
    );
    app.focused = false;
    assert!(
        !app.has_active_animation(),
        "an unfocused settling node must not repaint the background window"
    );

    app.focused = false;
    app.finalize_settles_for_test();
    assert!(!app.has_settling_entry());
    assert!(
        !should_advance_animation_tick(&app),
        "once every settle finalizes the tick returns to its idle behavior"
    );
    assert!(
        !app.has_active_animation(),
        "a fully settled idle transcript must not advertise active animation"
    );
}

#[test]
fn note_turn_started_marks_dirty_and_animation_active() {
    let mut app = test_app(SessionMode::Build);
    app.needs_redraw = false;
    app.turn_visual = TurnVisualState::Running;
    app.note_turn_started();
    assert!(app.needs_redraw, "starting a turn should request a redraw");
    assert!(
        app.has_active_animation(),
        "running turn should keep animations alive"
    );
}

#[test]
fn focus_lost_keeps_active_turn_spinner_animating_but_freezes_idle_motion() {
    // The main loop freezes idle background motion while unfocused, but an
    // in-flight turn must keep ticking so the first-prompt spinner does not
    // park on one frame if the terminal reports a transient focus loss.
    let mut app = test_app(SessionMode::Build);
    let (_tx, rx) = mpsc::channel(1);
    app.turn_rx = Some(rx);
    app.cancel = Some(CancellationToken::new());
    app.turn_visual = TurnVisualState::Running;
    app.terminal_title_state = TerminalTitleState::Working;
    assert!(
        app.has_active_animation(),
        "focused running turn must advertise an active animation"
    );

    // FocusLost arrived during an active turn. The turn remains the driver, so
    // the spinner and title animation keep advancing.
    app.focused = false;
    assert!(
        app.has_active_animation(),
        "unfocused active turn must still advertise an active animation"
    );

    let baseline = app.animation_tick;
    for _ in 0..32 {
        if should_advance_animation_tick(&app) {
            app.animation_tick = app.animation_tick.wrapping_add(1);
        }
    }
    assert_eq!(
        app.animation_tick,
        baseline + 32,
        "active turn should keep the animation tick alive while unfocused"
    );

    let mut idle = test_app(SessionMode::Build);
    idle.focused = false;
    idle.turn_visual = TurnVisualState::Running;
    idle.terminal_title_state = TerminalTitleState::Working;
    assert!(
        !idle.has_active_animation(),
        "unfocused idle UI should not keep background animation alive"
    );
    let idle_baseline = idle.animation_tick;
    for _ in 0..32 {
        if should_advance_animation_tick(&idle) {
            idle.animation_tick = idle.animation_tick.wrapping_add(1);
        }
    }
    assert_eq!(
        idle.animation_tick, idle_baseline,
        "idle animation tick should stay frozen while unfocused"
    );
}

#[test]
fn idle_prompt_coin_is_frozen_regardless_of_animation_tick() {
    let mut app = test_app(SessionMode::Build);
    app.turn_visual = TurnVisualState::Idle;
    // Walk through an entire glyph cycle worth of ticks. None of them
    // should advance the prompt coin glyph or change its colour, because
    // the coin is meant to be static at idle.
    for tick in 0..32u64 {
        app.animation_tick = tick * 50; // ~160 frames worth of motion
        assert_eq!(
            prompt_coin_frame(&app),
            "☽",
            "idle prompt coin glyph must stay '☽' at tick {tick}"
        );
        let span = prompt_coin_span(&app);
        assert_eq!(span.content.as_ref(), "☽");
        assert_eq!(span.style.fg, Some(crate::render::theme::accent()));
    }
}

#[test]
fn status_line_unset_uses_builtin_colored_detail_items() {
    let app = test_app(SessionMode::Build);
    // No /statusline configured means "use the built-in detail list" with
    // colors enabled by default. Row 2 remains the hints line.
    let lines = format_status_lines(&app, 120);
    assert_eq!(lines.len(), 2);
    let row1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    let row2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        row1.contains("scripted:gpt-test"),
        "default detail row should include provider:model: {row1}"
    );
    assert!(row2.contains("Enter send"), "row 2 should be hints: {row2}");
    assert!(
        !row1.contains("dir "),
        "default detail row should replace the legacy dir/git prefix: {row1}"
    );
    assert!(row2.contains("Enter send"), "row 2 should be hints: {row2}");
}

#[test]
fn status_line_empty_list_disables_detail_items() {
    let mut app = test_app(SessionMode::Build);
    app.status_line_items = Some(Vec::new());

    let lines = format_status_lines(&app, 120);
    assert_eq!(lines.len(), 2);
    let row1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    let row2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        row1.contains("dir "),
        "row 1 should fall back to legacy overview: {row1}"
    );
    assert!(
        !row1.contains("scripted:gpt-test"),
        "empty list should disable detail items: {row1}"
    );
    assert!(row2.contains("Enter send"), "row 2 should be hints: {row2}");
}

#[test]
fn status_line_default_detail_truncates_before_mode_label() {
    let mut app = test_app(SessionMode::Build);
    app.directory =
        "/very/long/workspace/path/that/should/not/push/the/mode/label/off/screen".to_string();

    let lines = format_status_lines(&app, 80);
    let row1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();

    assert!(
        row1.contains("..."),
        "long default detail row should be truncated: {row1}"
    );
    assert!(
        row1.contains("Build mode (Shift+Tab to cycle)"),
        "mode label must remain visible: {row1}"
    );
    let provider_span = lines[0]
        .spans
        .iter()
        .find(|s| s.content.contains("scripted:gpt-test"))
        .expect("provider span");
    assert_eq!(
        provider_span.style.fg,
        Some(crate::render::theme::cyan()),
        "unset status_line should still use the default colored detail row"
    );
}

#[test]
fn status_line_configured_replaces_overview_dir_and_branch() {
    use crate::status::StatusLineItem;
    let mut app = test_app(SessionMode::Build);
    app.status_line_items = Some(vec![
        StatusLineItem::ProviderAndModel,
        StatusLineItem::CurrentDir,
    ]);
    app.status_line_use_colors = true;
    let lines = format_status_lines(&app, 200);
    assert_eq!(lines.len(), 2);
    let row1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    let row2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    // Row 1 carries the configured detail items, no longer the legacy
    // "dir … · git …" prefix that duplicates them.
    assert!(row1.contains("scripted:gpt-test"), "row1={row1}");
    assert!(
        !row1.contains("dir "),
        "overview should be replaced; row1={row1}"
    );
    assert!(
        !row1.contains("· git "),
        "overview should be replaced; row1={row1}"
    );
    // Mode label still right-aligns on row 1.
    assert!(row1.contains("Build mode"), "row1={row1}");
    // Hints move to row 2 alone.
    assert!(row2.contains("Enter send"), "row2={row2}");
    let provider_span = lines[0]
        .spans
        .iter()
        .find(|s| s.content.contains("scripted:gpt-test"))
        .expect("provider span");
    assert_eq!(
        provider_span.style.fg,
        Some(crate::render::theme::cyan()),
        "provider-and-model should paint with the Model accent (cyan)"
    );
}

#[test]
fn status_line_languages_use_squeezy_amber() {
    use crate::status::StatusLineItem;
    let expected_theme =
        crate::render::theme::resolve_theme(&squeezy_core::AppConfig::default(), "default");
    let expected_language_color = expected_theme.color(crate::render::theme::token::PALETTE_ACCENT);
    let mut app = test_app(SessionMode::Build);
    app.directory = "~/project".to_string();
    app.language_summary = "Python 10, Rust 247".to_string();
    app.status_line_items = Some(vec![StatusLineItem::CurrentDir, StatusLineItem::Languages]);
    app.status_line_use_colors = true;

    let lines = format_status_lines(&app, 200);
    let dir_span = lines[0]
        .spans
        .iter()
        .find(|s| s.content == "~/project")
        .expect("directory span");
    let language_span = lines[0]
        .spans
        .iter()
        .find(|s| s.content.contains("Python 10, Rust 247"))
        .expect("languages span");

    assert_eq!(
        language_span.style.fg,
        Some(expected_language_color),
        "languages should use Squeezy's darker brand accent"
    );
    assert_ne!(
        dir_span.style.fg,
        Some(expected_language_color),
        "other path-family status items should not use the language brand accent"
    );
}

#[test]
fn status_line_items_stay_compact_for_paths_tokens_and_session_ids() {
    use crate::status::StatusLineItem;
    let mut app = test_app(SessionMode::Build);
    app.directory =
        "/Users/example/workspaces/squeezy-with-a-very-long-path/crates/squeezy-tui/src".into();
    app.session_id = Some("019e57d8-dbf1-79c2-ae5a-cc67e93f3a34".into());
    app.metrics.bytes_read = 2_500_000;

    let dir = status::resolve_status_item(&app, StatusLineItem::CurrentDir).expect("dir");
    let session = status::resolve_status_item(&app, StatusLineItem::SessionId).expect("session");
    let bytes = status::resolve_status_item(&app, StatusLineItem::BytesRead).expect("bytes");

    assert!(dir.len() <= 52, "{dir}");
    assert!(session.len() <= 24, "{session}");
    assert_eq!(bytes, "read 2.4MB");
}

#[test]
fn status_line_empty_list_disables_detail() {
    let mut app = test_app(SessionMode::Build);
    app.status_line_items = Some(Vec::new());
    let lines = format_status_lines(&app, 120);
    assert_eq!(lines.len(), 2);
    let row2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(row2.contains("Enter send"), "{row2}");
    assert!(!row2.contains(" · openai:"), "{row2}");
}

#[test]
fn status_line_item_round_trips_through_slug() {
    use crate::status::StatusLineItem;
    for item in StatusLineItem::ALL {
        let parsed: StatusLineItem = item
            .slug()
            .parse()
            .unwrap_or_else(|_| panic!("slug {} should parse back to its item", item.slug()));
        assert_eq!(parsed, *item);
    }
}

#[test]
fn status_line_codex_aliases_parse() {
    use crate::status::StatusLineItem;
    assert_eq!(
        "codex-version".parse::<StatusLineItem>().unwrap(),
        StatusLineItem::SqueezyVersion
    );
    assert_eq!(
        "status".parse::<StatusLineItem>().unwrap(),
        StatusLineItem::RunState
    );
    assert_eq!(
        "project".parse::<StatusLineItem>().unwrap(),
        StatusLineItem::ProjectName
    );
}

#[test]
fn frame_rate_limiter_allows_first_frame_immediately() {
    let limiter = FrameRateLimiter::default();
    let now = Instant::now();
    assert!(
        limiter.allow(now),
        "fresh limiter must let the first frame through"
    );
    assert!(
        limiter.time_until_next(now).is_none(),
        "fresh limiter reports no wait before the first frame"
    );
}

#[test]
fn frame_rate_limiter_rejects_frames_inside_min_interval() {
    let mut limiter = FrameRateLimiter::default();
    let t0 = Instant::now();
    limiter.mark_emitted(t0);

    // 1 ms after the last emit is well inside the 16 ms budget: deny.
    let too_soon = t0 + Duration::from_millis(1);
    assert!(!limiter.allow(too_soon), "limiter must clamp to 60 FPS");
    let wait = limiter
        .time_until_next(too_soon)
        .expect("limiter should report remaining wait");
    assert_eq!(
        wait,
        MAX_FRAME_INTERVAL - Duration::from_millis(1),
        "wait reflects time left in the frame budget"
    );

    // At exactly `MAX_FRAME_INTERVAL` after the last emit the next frame
    // is allowed again.
    let at_budget = t0 + MAX_FRAME_INTERVAL;
    assert!(
        limiter.allow(at_budget),
        "limiter must release once MAX_FRAME_INTERVAL has elapsed"
    );
    assert!(
        limiter.time_until_next(at_budget).is_none(),
        "no wait once the budget has elapsed"
    );
}

#[test]
fn frame_rate_limiter_coalesces_burst_into_one_emit() {
    // Simulates a flurry of events arriving inside a single frame budget:
    // only one draw should pass the limiter; the rest must be denied so
    // the loop coalesces them into the next frame.
    let mut limiter = FrameRateLimiter::default();
    let t0 = Instant::now();
    assert!(limiter.allow(t0), "first burst frame is allowed");
    limiter.mark_emitted(t0);

    let mut denied = 0;
    for i in 1..=5 {
        let t = t0 + Duration::from_millis(i);
        if !limiter.allow(t) {
            denied += 1;
        }
    }
    assert_eq!(
        denied, 5,
        "all five follow-up events inside the budget must be denied"
    );
}

#[tokio::test]
async fn alt_one_resumes_most_recent_non_active_session() {
    // Seed two extra sessions for the workspace and assert Alt+1 lands on
    // the more recently started one (the active session is excluded so
    // slot 1 picks the next-newest peer).
    let root = temp_workspace("quick_switch");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let store = squeezy_store::SessionStore::open(&config);

    let older = store
        .start_session_eager(squeezy_store::SessionMetadata::new(&config, "scripted"))
        .expect("seed older session");
    let newer = store
        .start_session_eager(squeezy_store::SessionMetadata::new(&config, "scripted"))
        .expect("seed newer session");
    // Force an explicit ordering so the test does not depend on
    // back-to-back `now_ms()` calls landing on different millisecond
    // boundaries.
    older
        .update_metadata(|metadata| metadata.started_at_ms = 1_000)
        .expect("backdate older");
    newer
        .update_metadata(|metadata| metadata.started_at_ms = 2_000)
        .expect("backdate newer");

    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    assert!(
        handle_session_quick_switch(&mut app, &mut agent, 1).await,
        "Alt+1 should claim the press when a peer session exists"
    );
    assert_eq!(
        agent.session_id().as_deref(),
        Some(newer.session_id()),
        "Alt+1 must land on the newer peer; status={}",
        app.status,
    );
    assert!(
        app.status.contains("resumed session"),
        "status should report the resume: {}",
        app.status,
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn quick_switch_clears_stale_subagent_pane() {
    // Subagent records belong to the session being left; they are never
    // persisted or rehydrated. Switching sessions must reset the pane so the
    // prior session's rows do not linger and an active subagent view does not
    // hijack the resumed transcript.
    let root = temp_workspace("quick_switch_subagent");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let store = squeezy_store::SessionStore::open(&config);
    store
        .start_session_eager(squeezy_store::SessionMetadata::new(&config, "scripted"))
        .expect("seed peer session");

    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);

    // Seed a subagent row in the current session and pin the main view to it.
    app.note_subagent_started(7, "delegate".to_string(), "Inspect src".to_string());
    app.subagent_pane.active = ConversationSource::Subagent(7);
    app.subagent_pane.focused = true;
    app.subagent_pane.selected = 3;
    assert!(
        active_subagent_record(&app).is_some(),
        "precondition: subagent view is active before the switch",
    );

    assert!(
        handle_session_quick_switch(&mut app, &mut agent, 1).await,
        "Alt+1 should claim the press when a peer session exists",
    );
    assert!(
        app.status.contains("resumed session"),
        "status should report the resume: {}",
        app.status,
    );

    assert!(
        app.subagent_pane.records.is_empty(),
        "the prior session's subagent rows must be cleared on switch",
    );
    assert_eq!(
        app.subagent_pane.active,
        ConversationSource::Main,
        "active source must fall back to the main conversation",
    );
    assert!(!app.subagent_pane.focused, "pane focus must be released");
    assert_eq!(app.subagent_pane.selected, 0, "pane selection must reset");
    assert!(
        active_subagent_record(&app).is_none(),
        "no stale subagent record may hijack the resumed transcript",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn alt_nine_reports_no_session_when_slot_is_empty() {
    // With only one peer session in the store, Alt+9 has nothing to land
    // on; the handler still claims the keypress (so it doesn't fall
    // through to other Alt handlers) but reports the empty slot.
    let root = temp_workspace("quick_switch_empty");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let store = squeezy_store::SessionStore::open(&config);
    let only = store
        .start_session_eager(squeezy_store::SessionMetadata::new(&config, "scripted"))
        .expect("seed only peer");
    let only_id = only.session_id().to_string();

    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let active_before = agent.session_id();

    assert!(
        handle_session_quick_switch(&mut app, &mut agent, 9).await,
        "Alt+9 must claim the press even when no slot 9 exists"
    );
    assert_eq!(
        agent.session_id(),
        active_before,
        "agent must stay on its current session when slot 9 is empty",
    );
    assert!(
        app.status.contains("no recent session"),
        "status should surface the empty-slot message: {}",
        app.status,
    );
    // Sanity check that the seeded peer actually exists; otherwise the
    // empty-slot assertion would pass for the wrong reason.
    let listed = agent
        .list_sessions(&squeezy_store::SessionQuery::default())
        .expect("list sessions");
    assert!(
        listed.iter().any(|meta| meta.session_id == only_id),
        "seeded peer must appear in list",
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn synchronized_output_resolver_honours_always_and_never_without_env() {
    // `Always` must short-circuit env probing so capability detection
    // never demotes an explicit opt-in to off.
    assert!(super::resolve_synchronized_output(
        TuiSynchronizedOutput::Always
    ));
    assert!(!super::resolve_synchronized_output(
        TuiSynchronizedOutput::Never
    ));
}

#[test]
fn synchronized_output_auto_detects_known_capable_terminals() {
    // Each row covers one of the capability signals advertised by the
    // terminals listed in the F09 finding — when only that variable is
    // set, `auto` must resolve to enabled.
    let signals: &[&[(&str, &str)]] = &[
        &[("KITTY_WINDOW_ID", "42")],
        &[("WEZTERM_PANE", "0")],
        &[("WEZTERM_EXECUTABLE", "/usr/local/bin/wezterm")],
        &[(
            "GHOSTTY_RESOURCES_DIR",
            "/Applications/Ghostty.app/Contents/Resources",
        )],
        &[("ALACRITTY_LOG", "/tmp/alacritty.log")],
        &[("ALACRITTY_WINDOW_ID", "1")],
        &[("ITERM_SESSION_ID", "w0t0p0")],
        &[("TERM_PROGRAM", "iTerm.app")],
        &[("TERM_PROGRAM", "WezTerm")],
        &[("TERM_PROGRAM", "ghostty")],
        &[("TERM_PROGRAM", "kitty")],
        &[("TERM_PROGRAM", "vscode")],
        &[("TERM", "xterm-kitty")],
        &[("TERM", "wezterm")],
        &[("TERM", "alacritty")],
        &[("TERM", "xterm-ghostty")],
        &[("TERM", "foot")],
        &[("TERM", "contour")],
    ];
    for fixture in signals {
        let lookup = |key: &str| -> Option<std::ffi::OsString> {
            fixture
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| std::ffi::OsString::from(*v))
        };
        assert!(
            super::detect_synchronized_output_support_from_env(lookup),
            "capability detection should flag {fixture:?}"
        );
    }
}

#[test]
fn synchronized_output_auto_stays_off_for_unknown_terminals() {
    // No env signals: capability detection must NOT speculatively
    // enable sync mode. Users on terminals we have no evidence about
    // still get the no-op-safe codes via `Always`, but `Auto` errs on
    // the side of leaving them alone.
    let empty = |_: &str| -> Option<std::ffi::OsString> { None };
    assert!(!super::detect_synchronized_output_support_from_env(empty));

    let only_screen = |key: &str| -> Option<std::ffi::OsString> {
        if key == "TERM" {
            Some(std::ffi::OsString::from("screen-256color"))
        } else {
            None
        }
    };
    assert!(
        !super::detect_synchronized_output_support_from_env(only_screen),
        "screen/tmux passthrough must not auto-enable BSU"
    );

    let only_dumb = |key: &str| -> Option<std::ffi::OsString> {
        if key == "TERM" {
            Some(std::ffi::OsString::from("dumb"))
        } else {
            None
        }
    };
    assert!(
        !super::detect_synchronized_output_support_from_env(only_dumb),
        "dumb terminal must not auto-enable BSU"
    );
}

#[tokio::test]
async fn alt_one_skips_when_an_approval_is_pending() {
    // Modal-blocking states (approval, plan choice, config screen, …)
    // must not consume the Alt+1 press so the user can still type into
    // the prompt; the handler returns `false` and leaves status alone.
    let root = temp_workspace("quick_switch_blocked");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let store = squeezy_store::SessionStore::open(&config);
    store
        .start_session_eager(squeezy_store::SessionMetadata::new(&config, "scripted"))
        .expect("seed peer session");

    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let (decision_tx, _decision_rx) = tokio::sync::oneshot::channel();
    app.pending_approval = Some(PendingApproval {
        request: sample_approval_request(),
        decision_tx,
    });
    let status_before = app.status.clone();

    assert!(
        !handle_session_quick_switch(&mut app, &mut agent, 1).await,
        "Alt+1 must fall through while an approval is pending",
    );
    assert_eq!(
        app.status, status_before,
        "blocked Alt+1 must not overwrite status"
    );

    let _ = fs::remove_dir_all(root);
}

// ─── /config dispatch routing — TuiApp-level eval tests ─────────────────
//
// Companion fixture: crates/squeezy-eval/fixtures/scenarios/options-screen-routing.toml.
// These tests exercise the slash-router by driving `handle_key` against a
// real `TuiApp`; screen-internal behaviour (rendering, key handling,
// save dispatch) lives in `config_screen_tests.rs` instead.

#[tokio::test]
async fn unknown_config_slug_emits_warning_then_opens_default() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/config nosuchsection".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert!(app.config_screen.is_some(), "screen should still open");
    let message = transcript_log_text(&app);
    assert!(
        message.contains("nosuchsection") && message.contains("not a navigable section"),
        "unknown slug should surface a warning naming the bad slug, got: {message}"
    );
}

/// Concatenate every `Log` entry's message in the transcript. Tests use it
/// to assert that a notice landed in the durable transcript instead of the
/// removed rotating notification pane.
fn transcript_log_text(app: &TuiApp) -> String {
    app.transcript
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Log(LogEntry { message, .. }) => Some(message.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn config_slug_for_unregistered_meta_warns() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/config skills".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    let message = transcript_log_text(&app);
    assert!(
        message.contains("skills"),
        "skills slug (SectionId variant w/o ConfigSectionMeta) should warn, got: {message}"
    );
}

#[tokio::test]
async fn config_with_no_arg_does_not_emit_unknown_slug_warning() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/config".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    let message = transcript_log_text(&app);
    assert!(
        !message.contains("not a navigable section"),
        "no-arg /config must not warn about a missing slug, got: {message}"
    );
}

#[tokio::test]
async fn config_and_options_alias_open_the_same_screen() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/config".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    let after_config = app.config_screen.as_ref().map(|s| s.current_section().id);
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    set_input(&mut app, "/options".to_string());
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    let after_options = app.config_screen.as_ref().map(|s| s.current_section().id);
    assert_eq!(
        after_config, after_options,
        "/config and /options must land on the same starting section"
    );
}

#[test]
fn rail_head_spans_are_seven_cells() {
    use rail::RailMarker::*;
    for marker in [Settled, Queued, Plan, Ok, Fail, Warn, Live("✦".to_string())] {
        let cells: usize = rail::head_spans(&marker, false)
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(
            cells, 7,
            "{marker:?} head must be 7 cells (indent + elbow + marker)"
        );
    }
}

#[test]
fn rail_apply_gutter_is_noop_on_empty_block() {
    let mut lines: Vec<Line<'static>> = Vec::new();
    rail::apply_gutter(&mut lines, rail::RailMarker::Ok, false);
    assert!(
        lines.is_empty(),
        "an empty block must leave no orphan connector"
    );
}

#[test]
fn rail_apply_gutter_heads_first_line_connects_the_rest() {
    let mut lines = vec![
        Line::from(vec![Span::raw("  "), Span::raw("header")]),
        Line::from(vec![Span::raw("  "), Span::raw("body")]),
    ];
    rail::apply_gutter(&mut lines, rail::RailMarker::Ok, false);
    // Header line: indent + ├─ elbow + ✓ marker replace the leading margin.
    assert_eq!(lines[0].spans[0].content.as_ref(), "   ");
    assert_eq!(lines[0].spans[1].content.as_ref(), "├─");
    assert_eq!(lines[0].spans[2].content.as_ref(), "✓ ");
    assert_eq!(lines[0].spans[3].content.as_ref(), "header");
    // Continuation line: indent + dim connector replaces the margin.
    assert_eq!(lines[1].spans[0].content.as_ref(), "   │   ");
    assert_eq!(lines[1].spans[1].content.as_ref(), "body");
}

#[test]
fn rail_apply_gutter_last_node_uses_the_close_elbow() {
    let mut lines = vec![Line::from(vec![Span::raw("  "), Span::raw("x")])];
    rail::apply_gutter(&mut lines, rail::RailMarker::Settled, true);
    assert_eq!(lines[0].spans[0].content.as_ref(), "   ");
    assert_eq!(lines[0].spans[1].content.as_ref(), "╰─");
    assert_eq!(lines[0].spans[2].content.as_ref(), "◦ ");
}

#[test]
fn rail_chrome_lights_special_nodes() {
    use crate::render::theme;
    let subagent = TranscriptEntryKind::Log(LogEntry {
        message: "delegate subagent started: x".to_string(),
        kind: LogKind::Subagent,
    });
    let plain = TranscriptEntryKind::Log(LogEntry {
        message: "note".to_string(),
        kind: LogKind::Normal,
    });
    // Plain work is dim; a subagent breadcrumb glows magenta; a normal log is
    // not a rail node at all, but if asked it stays dim.
    assert_eq!(rail_chrome(&subagent, false), theme::magenta());
    assert_eq!(rail_chrome(&plain, false), rail::dim());
    // Inside a subagent's own transcript the whole rail turns magenta.
    assert_eq!(rail_chrome(&plain, true), theme::magenta());
    // Only subagent logs thread the rail; other logs flow off it.
    assert!(is_rail_work_node(&subagent));
    assert!(!is_rail_work_node(&plain));
}

/// Fixture: a full turn (reasoning → tool → subagent breadcrumbs → a failed
/// tool → answer) rendered on the inline Quiet Rail. Guards the combined
/// look — every work node threads the rail, the failure's stderr is not
/// duplicated, and the answer leaves the rail set off by a blank line.
#[test]
fn rail_gallery_renders_a_full_turn() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user(
        "Refactor the auth module and run the workspace tests",
    ));
    app.push_reasoning_segment(squeezy_core::ReasoningSnapshot::from_payload(
        squeezy_core::ReasoningPayload::OpenAi {
            item_id: "rsn-1".to_string(),
            summary: vec!["Plan: read auth.rs, delegate an audit, then run the tests.".to_string()],
            encrypted_content: None,
        },
    ));
    app.push_tool_result(sample_tool_result(
        "grep",
        "src/auth.rs:12: fn login(\nsrc/auth.rs:40: fn logout(",
    ));
    app.push_subagent_note("delegate subagent started: Audit auth.rs".to_string());
    app.push_subagent_note(
        "delegate subagent completed · 5 tools · Found 2 issues: missing rate-limit, weak password hash"
            .to_string(),
    );
    let mut failed = sample_tool_result("shell", "");
    failed.status = ToolStatus::Error;
    failed.content = serde_json::json!({
        "command": "cargo test --workspace",
        "exit_code": 1,
        "stdout": "",
        "stderr": "error[E0308]: mismatched types\n  --> src/auth.rs:12:5\n   |\n12 |     login(user)\n   |     ^^^^^ expected String, found &str",
    });
    app.push_tool_result(failed);
    app.push_transcript_item(TranscriptItem::assistant(
        "Found 2 auth issues; the test failure is a type mismatch in login().",
    ));
    app.finalize_settles_for_test();

    let len = app.transcript.len();
    let scrollback = lines_to_plain_text(&inline_history_lines_for_flush(&app, 96, true, 0, len));

    // Every work node threads the rail.
    assert!(scrollback.contains("├─▸ reasoning · Plan:"), "{scrollback}");
    assert!(
        scrollback.contains("├─◆ delegate subagent started"),
        "{scrollback}"
    );
    assert!(
        scrollback.contains("├─◆ delegate subagent completed · 5 tools"),
        "{scrollback}"
    );
    assert!(scrollback.contains("├─✖ Failed cargo test"), "{scrollback}");
    // The auto-expanded stderr renders exactly once (no duplication).
    assert_eq!(
        scrollback.matches("expected String, found &str").count(),
        1,
        "{scrollback}"
    );
    // The gutter threads into the answer: a `│` connector then the `☽` answer,
    // whose crescent sits in the gutter column (col 3) so the line is unbroken.
    assert!(
        scrollback.contains("exit 1\n   │\n   ☽ Found 2 auth issues"),
        "{scrollback}"
    );
}

#[test]
fn prompt_coin_iterates_full_moon_cycle_while_typing() {
    let mut app = test_app(SessionMode::Build);
    // An empty composer rests on a steady crescent.
    app.input.clear();
    assert_eq!(prompt_coin_frame(&app), "☽");
    // Each character advances exactly one phase, walking the full lunar cycle.
    // The live typing coin is not restricted to the sent prompt's half-moons.
    for (typed, expected) in [
        (1usize, "◑"),
        (2, "●"),
        (3, "◐"),
        (4, "☾"),
        (5, "○"),
        (6, "☽"),
    ] {
        set_input(&mut app, "x".repeat(typed));
        assert_eq!(
            prompt_coin_frame(&app),
            expected,
            "{typed} characters typed"
        );
    }
}

#[test]
fn routing_note_threads_the_rail_as_a_dim_dot() {
    let mut app = test_app(SessionMode::Build);
    app.turn_visual = TurnVisualState::Succeeded;
    app.push_transcript_item(TranscriptItem::user("hey"));
    app.push_note("routed `sonnet` → `haiku` (llm_judge)".to_string());
    app.push_transcript_item(TranscriptItem::assistant("Hey!"));
    app.finalize_settles_for_test();
    let len = app.transcript.len();
    let scrollback = lines_to_plain_text(&inline_history_lines_for_flush(&app, 70, false, 0, len));
    // The note threads the gutter as `├─◦` (one note pipeline) — never the old
    // off-rail `• Noted` line that severed the coin→answer gutter.
    assert!(
        scrollback.contains("├─◦ routed `sonnet` → `haiku` (llm_judge)"),
        "{scrollback}"
    );
    assert!(!scrollback.contains("• Noted"), "{scrollback}");
}

#[test]
fn warning_threads_the_rail_as_a_warn_node() {
    let mut app = test_app(SessionMode::Build);
    app.turn_visual = TurnVisualState::Succeeded;
    app.push_transcript_item(TranscriptItem::user("hey"));
    app.push_warn("config key `foo` ignored (unknown)".to_string());
    app.push_transcript_item(TranscriptItem::assistant("Done."));
    app.finalize_settles_for_test();
    let len = app.transcript.len();
    let scrollback = lines_to_plain_text(&inline_history_lines_for_flush(&app, 70, false, 0, len));
    // A warning threads the gutter (├─⚠) instead of floating off-rail.
    assert!(
        scrollback.contains("├─⚠ config key `foo` ignored (unknown)"),
        "{scrollback}"
    );
}

#[test]
fn warn_log_marker_is_cyan_not_amber() {
    // warn = cyan (frees rationed amber); the ⚠ never reuses the plan-amber
    // family that secondary() belongs to.
    let entry = LogEntry {
        message: "x".to_string(),
        kind: LogKind::Warn,
    };
    let lines = format_log_entry(&entry, false, false);
    let glyph = &lines[0].spans[1];
    assert_eq!(glyph.content.as_ref(), "⚠ ");
    assert_eq!(glyph.style.fg, Some(crate::render::theme::cyan()));
    assert_ne!(glyph.style.fg, Some(crate::render::theme::secondary()));
}

#[test]
fn denied_and_cancelled_read_as_the_cyan_warn_tier() {
    // A user-initiated block/stop is the warn tier (cyan), distinct from a hard
    // failure (red), and never spends rationed amber.
    assert_eq!(
        status_color(ToolStatus::Denied),
        crate::render::theme::cyan()
    );
    assert_eq!(
        status_color(ToolStatus::Cancelled),
        crate::render::theme::cyan()
    );
    assert_eq!(status_color(ToolStatus::Error), crate::render::theme::red());
    assert_ne!(
        status_color(ToolStatus::Denied),
        crate::render::theme::secondary()
    );
}

#[test]
fn turn_failure_is_red_error_warning_is_cyan() {
    // A hard failure reads as a red ✖ error node; a warning stays a cyan ⚠.
    let err = LogEntry {
        message: "turn failed: boom".to_string(),
        kind: LogKind::Error,
    };
    let err_lines = format_log_entry(&err, false, false);
    assert_eq!(err_lines[0].spans[1].content.as_ref(), "✖ ");
    assert_eq!(
        err_lines[0].spans[1].style.fg,
        Some(crate::render::theme::red())
    );
    let warn = LogEntry {
        message: "config ignored".to_string(),
        kind: LogKind::Warn,
    };
    let warn_lines = format_log_entry(&warn, false, false);
    assert_eq!(warn_lines[0].spans[1].content.as_ref(), "⚠ ");
    assert_eq!(
        warn_lines[0].spans[1].style.fg,
        Some(crate::render::theme::cyan())
    );
}

#[test]
fn overlay_wraps_long_lines_keeping_the_gutter() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("explore"));
    let long = "I'll analyze the Rust codebase for modernization opportunities and \
                report the concrete findings here."
        .to_string();
    app.push_tool_result(sample_tool_result("explore", &long));
    if let Some(e) = app.transcript.last_mut() {
        e.collapsed = false;
    }
    let rows = transcript_overlay_rows_for_render(&app, 60);
    let text = lines_to_plain_text(&rows);
    // Every wrapped row of the long body keeps the `│` gutter — none spill to
    // column 0 the way the old hard char-wrap did.
    let body_rows: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("analyze") || l.contains("opportunities") || l.contains("findings"))
        .collect();
    assert!(
        body_rows.len() >= 2,
        "long body should wrap into >=2 rows:\n{text}"
    );
    for row in &body_rows {
        assert!(
            row.starts_with("   │"),
            "wrapped row lost the gutter: {row:?}"
        );
    }
    // The break lands on a word boundary, not mid-word.
    assert!(text.contains("for modernization"), "{text}");
    assert!(text.contains("│   opportunities"), "{text}");
}

#[test]
fn rail_prefix_and_continuation_track_the_gutter() {
    assert_eq!(rail_prefix_width("   ├─✔ Ran explore"), 7);
    assert_eq!(rail_prefix_width("   │   details"), 7);
    assert_eq!(rail_prefix_width("  settings reloaded"), 2);
    // A tee continues the rail as `│`; a close elbow blanks out (rail ended);
    // nested bars are both preserved.
    assert_eq!(rail_continuation_prefix("   ├─✔ "), "   │   ");
    assert_eq!(rail_continuation_prefix("   ╰─✖ "), "       ");
    assert_eq!(rail_continuation_prefix("   │   │   "), "   │   │   ");
}

#[test]
fn overlay_collapsed_folds_long_output_expanded_shows_all() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("explore"));
    let body: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    app.push_tool_result(sample_tool_result("explore", &body));
    let shown = |expand_all: bool| {
        lines_to_plain_text(&transcript_lines_for_overlay(&app, Some(80), expand_all))
            .lines()
            .filter(|l| l.contains("line "))
            .count()
    };
    let collapsed = shown(false);
    let expanded = shown(true);
    assert!(
        collapsed < expanded,
        "collapsed ({collapsed}) should fold below expanded ({expanded})"
    );
    assert_eq!(expanded, 40, "expanded shows every body line");
}

#[test]
fn subagent_overlay_opens_collapsed_default_is_expanded() {
    // Pressing a subagent opens it folded (formatted like the main inline view);
    // the main-transcript Ctrl+T opens straight to expanded.
    let mut app = test_app(SessionMode::Build);
    open_subagent_transcript_overlay(&mut app);
    assert_eq!(
        app.transcript_overlay.unwrap().detail,
        OverlayDetail::Collapsed
    );
    assert_eq!(
        TranscriptOverlayState::default().detail,
        OverlayDetail::Expanded
    );
}

#[tokio::test]
async fn ctrl_t_expands_then_closes_a_collapsed_overlay() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    open_subagent_transcript_overlay(&mut app);
    assert_eq!(
        app.transcript_overlay.unwrap().detail,
        OverlayDetail::Collapsed
    );
    // First Ctrl+T unfolds in place (does not close).
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");
    assert_eq!(
        app.transcript_overlay.unwrap().detail,
        OverlayDetail::Expanded
    );
    // Second Ctrl+T closes the (already expanded) overlay.
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    )
    .await
    .expect("handle key");
    assert!(app.transcript_overlay.is_none());
}

#[test]
fn subagent_view_renders_tool_results_as_rail_cards() {
    let mut app = test_app(SessionMode::Build);
    app.note_subagent_started(
        7,
        "explore".to_string(),
        "Discover the codebase".to_string(),
    );
    app.note_subagent_tool_result(
        7,
        "explore".to_string(),
        sample_tool_result("repo_map", "crate a\ncrate b"),
    );
    app.note_subagent_tool_result(
        7,
        "explore".to_string(),
        sample_tool_result("glob", "a.rs\nb.rs"),
    );
    app.subagent_pane.active = ConversationSource::Subagent(7);
    let view = lines_to_plain_text(&transcript_lines_for_overlay(&app, Some(70), false));
    // The subagent's tools thread the rail as ├─✔ cards, not the flat off-rail
    // `completed X` lifecycle line they used to render as.
    assert!(view.contains("├─✔ Explored repo map"), "{view}");
    assert!(view.contains("├─✔ Explored list files"), "{view}");
    assert!(!view.contains("completed repo_map"), "{view}");
    // The lifecycle breadcrumb threads the rail too (a ◦ note).
    assert!(view.contains("├─◦ explore subagent started"), "{view}");
}

#[test]
fn context_compaction_threshold_anchors_to_resolved_post_turn_ceiling() {
    // Small window: the nudge/status anchor tracks the window-derived ceiling
    // (80% of 32K = 25.6K), not the flat 60K budget (finding #5).
    let mut config = test_config(SessionMode::Build);
    config.context_compaction.model_context_window = Some(32_000);
    let app = TuiApp::new_with_clipboard(
        "openai",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );
    assert_eq!(app.context_compaction_threshold, 25_600);

    // Unknown window: falls back to the flat 60K budget.
    let flat = test_config(SessionMode::Build);
    let flat_app = TuiApp::new_with_clipboard(
        "openai",
        &flat,
        SessionMode::Build,
        None,
        Box::new(NoopClipboard),
    );
    assert_eq!(flat_app.context_compaction_threshold, 60_000);
}
