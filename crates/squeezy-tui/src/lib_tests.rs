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
    TaskVerificationState, ToolOutputVerbosity, TuiAlternateScreen, TuiConfig, TurnId, TurnMetrics,
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
    assert_eq!(label_span.style.fg, Some(MODE_PURPLE));
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
async fn raw_ctrl_e_dispatches_expand_action() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));
    assert!(app.transcript[0].collapsed);

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('\u{05}'), KeyModifiers::NONE),
    )
    .await
    .expect("raw ctrl+e");
    assert!(
        !app.transcript[0].collapsed,
        "raw \\x05 must reach the ExpandSelectedTranscriptEntry keymap arm",
    );
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
    // Inline mode → alternate_scroll_enabled is false. Pre-fix this
    // dropped wheel events entirely. After the fix, wheel scrolls the
    // transcript regardless.
    app.alternate_scroll_enabled = false;
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
        "wheel must scroll transcript even when alternate_scroll_enabled is false",
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
async fn slash_command_does_not_enqueue_mid_turn() {
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

    assert!(
        app.prompt_queue.is_empty(),
        "slash commands should execute immediately, never queue",
    );
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
    assert!(rendered.contains("[v] View"));

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
async fn plan_choice_view_keeps_prompt_open_and_logs_path() {
    let root = temp_workspace("plan_choice_view");
    let plans_dir = root
        .join(proposed_plan::PLAN_DIR)
        .join(proposed_plan::FALLBACK_SESSION_ID);
    fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let plan_id = "plan-view0001".to_string();
    let plan_path = plans_dir.join(format!("{plan_id}.md"));
    fs::write(&plan_path, "step\n").expect("write plan");

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.pending_plan_choice = Some(PendingPlanChoice {
        plan_id: plan_id.clone(),
        plan_path: plan_path.clone(),
        selection_index: 0,
    });

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
    )
    .await
    .expect("handle key");

    assert!(plan_path.exists(), "view must not delete the plan file");
    assert!(
        app.pending_plan_choice.is_some(),
        "View keeps the prompt open"
    );
    assert!(
        app.transcript.iter().any(|entry| matches!(
            &entry.kind,
            TranscriptEntryKind::Log(LogEntry { message, .. }) if message.contains(&plan_id)
        )),
        "expected a 'plan {plan_id} file:' log entry"
    );

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
    // SAFETY: tests run sequentially per file with --test-threads=1 if needed;
    // this env var is read at save time only.
    unsafe {
        std::env::set_var("SQUEEZY_SETTINGS_PATH", &settings_path);
    }
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
    unsafe {
        std::env::remove_var("SQUEEZY_SETTINGS_PATH");
    }
}

#[tokio::test]
async fn statusline_save_closes_picker_and_paints_detail_row() {
    // Open the picker, then press Enter to save the pre-checked defaults.
    // The picker must close and the status row must start showing the
    // chosen items.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
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
    set_input(&mut app, "/config permissions".to_string());
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

    app.input_cursor = "alpha\nbr".len();
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-e");
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
async fn prompt_ctrl_e_keeps_expansion_shortcut_when_prompt_is_empty() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-e expands");
    assert!(!app.transcript[0].collapsed);

    set_input(&mut app, "abc".to_string());
    app.input_cursor = 0;
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-e moves to end");
    assert_eq!(app.input_cursor, app.input.len());
    assert!(!app.transcript[0].collapsed);
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

#[test]
fn explicit_alternate_screen_mouse_wheel_scrolls_transcript_without_prompt_history() {
    let mut config = test_config(SessionMode::Build);
    config.tui.alternate_screen = TuiAlternateScreen::Always;
    let mut app = test_app_with_config(&config, SessionMode::Build);
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
    set_input(&mut app, "/session-cleanup".to_string());
    let rendered = render_to_string(&app, 120, 12);
    assert!(
        rendered.contains("[destructive]"),
        "expected /session-cleanup badge in slash menu:\n{rendered}"
    );
}

#[test]
fn slash_suggestion_line_contents_match_command_capabilities() {
    // Build the menu lines directly and assert the badge follows the
    // declared capabilities — covers both presence (`/help` → `net`) and
    // absence (`/cost` → no badge).
    let mut app = test_app(SessionMode::Build);
    set_input(&mut app, "/help".to_string());
    let lines = slash_suggestion_lines(&app);
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
    let lines = slash_suggestion_lines(&app);
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
    // Empty buckets are suppressed so a fresh session is a short report,
    // not a wall of zero-valued counters.
    assert!(!output.contains("tools calls="), "{output}");
    assert!(!output.contains("subagents calls="), "{output}");
    assert!(!output.contains("receipts stub_hits="), "{output}");
    assert!(!output.contains("spills writes="), "{output}");
    assert!(!output.contains("\nio bytes_read="), "{output}");
    assert!(!output.contains("\nredactions="), "{output}");
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

    let output = commands::format_cost_command(&snapshot);
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
async fn slash_context_uses_registry_fallback_for_unknown_models() {
    // The registry now ships a fallback metadata path for unknown model
    // ids, so /context produces concrete numbers (drawn from the fallback
    // window) rather than `unknown` placeholders.
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);

    assert!(handle_slash_command(&mut app, &mut agent, "/context").await);

    let output = last_message_content(&app).expect("context output");
    assert!(output.contains("context_window=272000"), "{output}");
    assert!(output.contains("max_output_reserve=64000"), "{output}");
    assert!(output.contains("input_budget="), "{output}");
    assert!(output.contains("used="), "{output}");
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
        "2026-05-24 ERROR failed\r\nOPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz\r".to_string(),
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
    assert!(
        app.attachments[0]
            .preview
            .contains("2026-05-24 ERROR failed"),
        "{}",
        app.attachments[0].preview
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

    wait_for_turn_completion(&mut app).await;
    let content = last_message_content(&app).expect("help transcript");
    assert!(
        transcript_message_contents(&app).contains(&"/help"),
        "user prompt should remain in the transcript"
    );
    assert!(content.contains("Supported topics"), "{content}");
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
    // Long-output regression: under the codex-style 5-line cap, a short
    // grep result fits inside the preview window so the body shows even
    // in the collapsed state — but a long body must be head-tail truncated
    // with the Ctrl-E ellipsis, then fully expand when toggled.
    let mut app = test_app(SessionMode::Build);
    let payload = (0..30)
        .map(|i| format!("match-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_tool_result(sample_tool_result("grep", &payload));

    assert!(app.transcript[0].collapsed);
    let collapsed = render_to_string(&app, 100, 24);
    assert!(collapsed.contains("✔ Explored"), "{collapsed}");
    assert!(collapsed.contains("grep"), "{collapsed}");
    assert!(!collapsed.contains("receipt="), "{collapsed}");
    assert!(!collapsed.contains("B receipt"), "{collapsed}");
    assert!(
        collapsed.contains("Ctrl-E to expand"),
        "collapsed view should show truncation hint: {collapsed}"
    );
    assert!(
        !collapsed.contains("match-15"),
        "middle of the body must be elided: {collapsed}"
    );

    select_previous_transcript_entry(&mut app);
    toggle_selected_transcript_entry(&mut app);

    assert!(!app.transcript[0].collapsed);
    let expanded = render_to_string(&app, 100, 40);
    assert!(expanded.contains("match-15"), "{expanded}");
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
    assert_eq!(app.status, "expanded transcript entry 1 · Alt+E expand all");

    toggle_selected_transcript_entry(&mut app);
    assert!(app.transcript[0].collapsed);
    assert_eq!(
        app.status,
        "collapsed transcript entry 1 · Alt+E expand all"
    );
}

#[test]
fn ctrl_e_without_selection_skips_prompt_rows_and_expands_collapsed_content() {
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));
    app.push_transcript_item(TranscriptItem::user("next prompt"));

    assert!(app.transcript[0].collapsed);
    assert!(!app.transcript[1].is_toggleable());

    toggle_selected_transcript_entry(&mut app);

    assert!(!app.transcript[0].collapsed);
    assert_eq!(app.status, "expanded transcript entry 1 · Alt+E expand all");
}

#[test]
fn toggle_expand_all_expands_every_collapsed_entry_when_any_are_collapsed() {
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));
    app.push_tool_result(sample_tool_result("read", "file body"));
    app.push_tool_result(sample_tool_result("glob", "file list"));

    assert!(app.transcript.iter().all(|e| e.collapsed));

    toggle_expand_all_transcript_entries(&mut app);

    assert!(
        app.transcript.iter().all(|e| !e.collapsed),
        "every toggleable entry should be expanded"
    );
    assert!(app.status.contains("expanded 3 of 3"));
}

#[test]
fn toggle_expand_all_collapses_all_when_already_expanded() {
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));
    app.push_tool_result(sample_tool_result("read", "file body"));

    toggle_expand_all_transcript_entries(&mut app);
    assert!(app.transcript.iter().all(|e| !e.collapsed));

    toggle_expand_all_transcript_entries(&mut app);
    assert!(
        app.transcript.iter().all(|e| e.collapsed),
        "second press should collapse every entry"
    );
    assert!(app.status.contains("collapsed 2 of 2"));
}

#[test]
fn toggle_expand_all_reports_nothing_to_expand_when_transcript_empty() {
    let mut app = test_app(SessionMode::Build);
    app.push_transcript_item(TranscriptItem::user("just a prompt"));

    toggle_expand_all_transcript_entries(&mut app);
    assert_eq!(app.status, "nothing expandable yet");
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
async fn alt_e_dispatches_expand_all() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));
    app.push_tool_result(sample_tool_result("read", "file body"));

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT),
    )
    .await
    .expect("alt+e fires expand-all");

    assert!(
        app.transcript.iter().all(|e| !e.collapsed),
        "Alt+E should expand both transcript entries"
    );
    assert!(app.status.contains("expanded"));
}

#[test]
fn parse_transcript_category_accepts_reasoning_and_thinking_aliases() {
    assert!(matches!(
        parse_transcript_category("reasoning"),
        Some(TranscriptCategory::Reasoning)
    ));
    assert!(matches!(
        parse_transcript_category("thinking"),
        Some(TranscriptCategory::Reasoning)
    ));
    assert!(parse_transcript_category("rambling").is_none());
}

#[tokio::test]
async fn ctrl_e_with_composer_text_keeps_line_end_and_emits_hint() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.push_tool_result(sample_tool_result("grep", "needle found"));
    set_input(&mut app, "abc".to_string());
    app.input_cursor = 0;

    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    )
    .await
    .expect("ctrl-e with text moves cursor and emits hint");

    assert_eq!(app.input_cursor, app.input.len(), "cursor moved to end");
    assert!(
        app.transcript[0].collapsed,
        "transcript should not change when composer has text"
    );
    assert!(
        app.status.contains("Alt+E"),
        "status should hint Alt+E for transcript expansion"
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
        "stderr": "error: could not find `Cargo.toml` in `/tmp/example-workspace` or any parent directory",
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
        output.contains("│ cargo test -p squeezy-tui in .:"),
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
        output.contains("│ inspect workspace --details in /tmp/project:"),
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

    // Each card now shows a short body preview by default (codex-style
    // 5-line cap), so the rendered height is taller than the old empty-
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
        true,
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
    // apply_patch cards bypass the codex-style 5-line cap (the diff *is*
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
    );
    let rendered = lines_to_plain_text(&lines);
    assert!(!rendered.contains("diff --git"), "{rendered}");
    assert!(!rendered.contains("index 123"), "{rendered}");

    // Patch content is "old" / "new" — short strings that the highlighter
    // labels as plain identifiers, so the sign + body fall back to the
    // diff-foreground color. The bg tint, however, is always applied on
    // +/- rows.
    let add_sign = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "+")
        .expect("add sign span");
    assert_eq!(
        add_sign.style.fg,
        Some(render::palette::best_color(
            render::palette::rgb_components(DIFF_ADD_FG,)
        ))
    );
    assert_eq!(add_sign.style.bg, Some(render::diff::diff_add_bg()));

    let del_sign = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "-")
        .expect("delete sign span");
    assert_eq!(
        del_sign.style.fg,
        Some(render::palette::best_color(
            render::palette::rgb_components(DIFF_DEL_FG,)
        ))
    );
    assert_eq!(del_sign.style.bg, Some(render::diff::diff_del_bg()));
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
        output.contains("› Keep the current layout"),
        "selected marker missing on second choice: {output}"
    );
    assert!(
        output.contains("freeform"),
        "freeform hint missing: {output}"
    );
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
    // The sign character carries the line's foreground colour; gutter,
    // sign and content are split into separate spans so per-token syntax
    // highlighting can attach colours to the body.
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

    assert_eq!(
        add_sign.style.fg,
        Some(render::palette::best_color(
            render::palette::rgb_components(DIFF_ADD_FG,)
        ))
    );
    assert_eq!(
        del_sign.style.fg,
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
        ("exact_syntax", render::palette::SUCCESS_GREEN),
        ("import_resolved", render::palette::AMBER),
        ("candidate_set", render::palette::GOLD),
        ("external", render::palette::QUIET),
        ("unknown", render::palette::QUIET),
        ("label_missing", render::palette::ERROR_RED),
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
    assert_eq!(span.style.fg, Some(render::palette::GOLD));
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
            && span.style.fg == Some(render::palette::SUCCESS_GREEN)
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

    let block = reasoning_block_lines("first thought\nsecond thought", false, false);
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
        header.spans.iter().any(|s| s.content.contains("thinking…")
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
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    ));
    assert_eq!(app.approval_selection_index, 2);
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
    assert!(output.contains("Approve for this session"), "{output}");
    assert!(
        output.contains("Always approve this command in this repo"),
        "{output}"
    );
    assert!(output.contains("Deny for this session"), "{output}");
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
fn footer_mentions_expand_and_transcript_shortcuts() {
    let app = test_app(SessionMode::Build);

    let output = render_to_string(&app, 140, 16);

    assert!(output.contains("Ctrl-E expand"), "{output}");
    assert!(output.contains("Ctrl-T transcript"), "{output}");
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
fn accounting_block_colors_labels_values_and_dollar_amounts() {
    let content = "Cost accounting\n\
session=abc\n\
provider=openai model=gpt-5.5 mode=build\n\
estimated_usd=$0.415300 (estimated from provider-reported usage and local pricing metadata)\n\
provider_tokens input=1200 output=340 reasoning=- cached_input=0 cache_write_input=-\n\
tools calls=4 successes=3 errors=1 denials=0 cancellations=0 budget_denials=0\n\
accuracy=provider token counters are provider-reported when available.";
    let item = TranscriptItem::system(content);

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal);
    assert_eq!(lines.len(), 7, "{lines:?}");

    let span_for = |line: &Line<'static>, text: &str| -> Style {
        line.spans
            .iter()
            .find(|span| span.content.as_ref() == text)
            .unwrap_or_else(|| panic!("missing span {text:?} in {line:?}"))
            .style
    };

    // Header still renders the `• Noted` chrome plus the bolded
    // "Cost accounting" body, in GOLD.
    let header_style = span_for(&lines[0], "Cost accounting");
    assert_eq!(header_style.fg, Some(GOLD));
    assert!(header_style.add_modifier.contains(Modifier::BOLD));

    // `session=` is the dim label, `abc` is the bright value.
    let session_line = &lines[1];
    assert_eq!(span_for(session_line, "session=").fg, Some(QUIET));
    assert_eq!(span_for(session_line, "abc").fg, None);

    // The dollar amount pops in AMBER; the trailing parenthetical fades.
    let usd_line = &lines[3];
    assert_eq!(span_for(usd_line, "estimated_usd=").fg, Some(QUIET));
    assert_eq!(span_for(usd_line, "$0.415300").fg, Some(AMBER));
    assert_eq!(span_for(usd_line, "(estimated").fg, Some(QUIET));

    // Zero / dash values fade so real numbers carry the eye.
    let tokens_line = &lines[4];
    assert_eq!(span_for(tokens_line, "provider_tokens").fg, Some(GOLD));
    assert_eq!(span_for(tokens_line, "1200").fg, None);
    assert_eq!(span_for(tokens_line, "-").fg, Some(QUIET));
    assert_eq!(span_for(tokens_line, "0").fg, Some(QUIET));

    // The leading group word on tool rows is GOLD.
    let tools_line = &lines[5];
    assert_eq!(span_for(tools_line, "tools").fg, Some(GOLD));
    assert_eq!(span_for(tools_line, "4").fg, None);

    // The accuracy epilogue is wholly dimmed.
    let accuracy_line = &lines[6];
    assert!(
        accuracy_line
            .spans
            .iter()
            .all(|span| span.style.fg.is_none()
                || span.style.fg == Some(QUIET)
                || span.content.as_ref().chars().all(char::is_whitespace)),
        "{accuracy_line:?}"
    );
}

#[test]
fn accounting_block_dispatch_skips_unrelated_system_messages() {
    let item = TranscriptItem::system("Random system note\nwith multiple\nlines");
    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal);
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
fn pending_assistant_uses_rotating_coin_marker() {
    let mut app = test_app(SessionMode::Build);
    app.pending_assistant.push_delta("streaming");
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

    let lines = format_message_entry_with_width(
        &item,
        false,
        false,
        MessageOutcome::Normal,
        Some(40),
        true,
    );
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

    let lines = format_message_entry_with_width(
        &item,
        false,
        false,
        MessageOutcome::Normal,
        Some(30),
        true,
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
        output.contains("│ turn failed: provider stream failed"),
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
    assert!(output.contains("• Working ("), "{output}");
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

    assert!(output.contains("• Working ("), "{output}");
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
fn submitted_bang_prompt_marks_first_nonempty_bang_dark_red() {
    let item = TranscriptItem::user("  !ls");

    let lines = format_message_entry(&item, false, false, MessageOutcome::Normal);
    let bang = lines[1]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "!")
        .expect("bang marker span");
    let rest = lines[1]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "ls")
        .expect("command body span");

    assert_eq!(bang.style.fg, Some(BANG_RED));
    assert_eq!(bang.style.bg, Some(PROMPT_BG));
    assert_eq!(rest.style.fg, Some(Color::White));
    assert_eq!(rest.style.bg, Some(PROMPT_BG));
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

    assert_eq!(bang.style.fg, Some(BANG_RED));
    assert_eq!(bang.style.bg, Some(PROMPT_BG));
    assert_eq!(rest.style.fg, Some(Color::White));
    assert_eq!(rest.style.bg, Some(PROMPT_BG));
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

        #[tokio::test]
        async fn jobs_alias_renders_same_surface() {
            let mut agent = test_agent(SessionMode::Build);
            let mut app = test_app(SessionMode::Build);
            assert!(handle_slash_command(&mut app, &mut agent, "/jobs").await);
            let body = last_message_content(&app).unwrap_or_default().to_string();
            assert!(
                body.contains("reviewer"),
                "expected /jobs alias to render the /tasks surface: {body}"
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
async fn successful_edit_turn_pushes_diff_undo_hint() {
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
    let mut app = test_app(SessionMode::Build);
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
        hint.contains("Ctrl-R"),
        "idle hint must advertise Ctrl-R when a cancelled prompt is stashed; got: {hint}"
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
        "stash stays so Ctrl-R can still recover the original prompt",
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

// ---- /verbosity inline back-compat ----
//
// `/model`, `/permissions`, `/verbosity`, and `/tool-verbosity` open the
// `/config` screen focused on the matching section; the original overlay
// flow was replaced by the full editor. Section-routing coverage lives in
// `slash_model_opens_config_at_models_section` and friends below.

#[tokio::test]
async fn slash_verbosity_with_inline_arg_applies_immediately() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    app.response_verbosity = ResponseVerbosity::Normal;
    let ran = handle_slash_command(&mut app, &mut agent, "/verbosity verbose").await;
    assert!(ran);
    assert!(
        app.config_screen.is_none(),
        "inline form should not open the screen"
    );
    assert_eq!(app.response_verbosity, ResponseVerbosity::Verbose);
    assert_eq!(
        agent.config_snapshot().tui.response_verbosity,
        ResponseVerbosity::Verbose
    );
}

// ---- /effort session-level reasoning-effort setter ----

#[tokio::test]
async fn slash_effort_low_sets_session_reasoning_effort() {
    let mut agent = test_agent(SessionMode::Build);
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
    let mut agent = test_agent(SessionMode::Build);
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
    let mut agent = test_agent(SessionMode::Build);
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

#[tokio::test]
async fn slash_verbosity_opens_config_when_called_without_arg() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let ran = handle_slash_command(&mut app, &mut agent, "/verbosity").await;
    assert!(ran);
    let state = app
        .config_screen
        .as_ref()
        .expect("config screen should be open");
    assert_eq!(
        state.current_section().id,
        squeezy_core::config_schema::SectionId::Verbosity
    );
}

// ---- /theme palette switch ----

/// `/theme dark` and `/theme light` flip the runtime palette override and
/// mirror the choice into the agent's config so a settings-screen open
/// reflects the new value.
#[tokio::test]
async fn slash_theme_dark_flips_palette_and_config() {
    use crate::render::palette;

    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    // Point settings writes at a tempfile so the test never touches HOME.
    let dir = temp_workspace("theme_dark");
    let settings_path = dir.join("settings.toml");
    let _guard = ScopedSettingsPath::new(settings_path.clone());

    let ran = handle_slash_command(&mut app, &mut agent, "/theme dark").await;
    assert!(ran, "/theme dark should dispatch");
    assert_eq!(palette::palette_tone(), palette::PaletteTone::Dark);
    assert_eq!(
        agent.config_snapshot().tui.theme,
        squeezy_core::TuiTheme::Dark
    );
    let saved = std::fs::read_to_string(&settings_path).expect("settings file written");
    assert!(
        saved.contains("theme = \"dark\""),
        "settings.toml should record the theme; got {saved}"
    );
}

/// `/theme light` flips the override to the light tone.
#[tokio::test]
async fn slash_theme_light_flips_palette() {
    use crate::render::palette;

    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_light");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let ran = handle_slash_command(&mut app, &mut agent, "/theme light").await;
    assert!(ran);
    assert_eq!(palette::palette_tone(), palette::PaletteTone::Light);
    assert_eq!(
        agent.config_snapshot().tui.theme,
        squeezy_core::TuiTheme::Light
    );
}

/// `/theme system` clears the override so terminal detection wins again.
#[tokio::test]
async fn slash_theme_system_clears_override() {
    use crate::render::palette;

    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_system");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    // Pin to Dark first so the System branch has something visible to clear.
    palette::set_palette_tone_override(Some(palette::PaletteTone::Dark));
    assert_eq!(palette::palette_tone(), palette::PaletteTone::Dark);

    let ran = handle_slash_command(&mut app, &mut agent, "/theme system").await;
    assert!(ran);
    // With the override cleared the visible tone is whichever the detector
    // returns for the current process — we only assert that the override
    // really was reset back to the detector's value.
    assert_eq!(palette::palette_tone(), palette::detected_palette_tone());
    assert_eq!(
        agent.config_snapshot().tui.theme,
        squeezy_core::TuiTheme::System
    );
}

/// Unknown sub-arguments don't mutate anything — the user sees a usage hint
/// instead of a silent tone change.
#[tokio::test]
async fn slash_theme_unknown_value_does_not_change_palette() {
    use crate::render::palette;

    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_bad");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let before_tone = palette::palette_tone();
    let before_theme = agent.config_snapshot().tui.theme;
    let ran = handle_slash_command(&mut app, &mut agent, "/theme zebra").await;
    assert!(ran);
    assert_eq!(palette::palette_tone(), before_tone);
    assert_eq!(agent.config_snapshot().tui.theme, before_theme);
    assert!(
        app.status.contains("unknown theme"),
        "status should mention the bad value, got: {}",
        app.status
    );
}

/// Bare `/theme` (no sub-arg) shows usage — used to remind the user of the
/// allowed values without forcing them to open the config screen.
#[tokio::test]
async fn slash_theme_without_arg_shows_usage() {
    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_usage");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let ran = handle_slash_command(&mut app, &mut agent, "/theme").await;
    assert!(ran);
    assert!(
        app.status.starts_with("usage:") && app.status.contains("/theme"),
        "expected usage hint, got: {}",
        app.status
    );
}

/// `/theme catppuccin` pins the Dark tone *and* flips the accent variant
/// so the working-shimmer and prompt cursor render in mauve. Verifies that
/// each named theme drives both the tone and the accent override — the two
/// must move together or the result is a mismatched palette.
#[tokio::test]
async fn slash_theme_catppuccin_pins_dark_tone_and_mauve_accent() {
    use crate::render::palette;

    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_catppuccin");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let ran = handle_slash_command(&mut app, &mut agent, "/theme catppuccin").await;
    assert!(ran, "/theme catppuccin should dispatch");
    assert_eq!(palette::palette_tone(), palette::PaletteTone::Dark);
    assert_eq!(
        palette::accent_variant(),
        palette::AccentVariant::Catppuccin
    );
    assert_ne!(
        palette::accent_primary(),
        palette::AMBER,
        "catppuccin must override the amber default to the mauve accent",
    );
    assert_eq!(
        agent.config_snapshot().tui.theme,
        squeezy_core::TuiTheme::Catppuccin
    );
}

/// `/theme high-contrast` pins the Light tone with a bright accessibility
/// accent. Validates the hyphenated-argument path users will type and that
/// the accent override flips to HighContrast independently of `dark`/
/// `light` so reverting via `/theme system` restores the default.
#[tokio::test]
async fn slash_theme_high_contrast_pins_light_tone_and_yellow_accent() {
    use crate::render::palette;

    let mut agent = test_agent(SessionMode::Build);
    let mut app = test_app(SessionMode::Build);
    let dir = temp_workspace("theme_hc");
    let _guard = ScopedSettingsPath::new(dir.join("settings.toml"));

    let ran = handle_slash_command(&mut app, &mut agent, "/theme high-contrast").await;
    assert!(ran);
    assert_eq!(palette::palette_tone(), palette::PaletteTone::Light);
    assert_eq!(
        palette::accent_variant(),
        palette::AccentVariant::HighContrast
    );
    assert_eq!(
        agent.config_snapshot().tui.theme,
        squeezy_core::TuiTheme::HighContrast,
    );

    // Reverting clears both overrides so the next session paints the
    // detector-derived tone with the amber/gold default again.
    let ran = handle_slash_command(&mut app, &mut agent, "/theme system").await;
    assert!(ran);
    assert_eq!(palette::accent_variant(), palette::AccentVariant::Default);
    assert_eq!(palette::accent_primary(), palette::AMBER);
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
        // Start every theme test from a known palette state — even if a
        // previous test left an override behind.
        crate::render::palette::set_palette_tone_override(None);
        crate::render::palette::set_accent_variant(crate::render::palette::AccentVariant::Default);
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
        crate::render::palette::set_palette_tone_override(None);
        crate::render::palette::set_accent_variant(crate::render::palette::AccentVariant::Default);
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
        "/attach",
        "/detach",
        "/pin",
        "/unpin",
        "/resume",
        "/fork",
        "/session-export",
        "/session-cleanup",
        "/undo",
        "/revert-turn",
        "/effort",
        "/verbosity",
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
fn slash_compact_unavailable_during_turn_renders_dim_hint() {
    let mut app = test_app(SessionMode::Build);
    app.cancel = Some(CancellationToken::new());
    set_input(&mut app, "/compact".to_string());

    let output = render_to_string(&app, 120, 16);
    assert!(
        output.contains("/compact"),
        "expected /compact in output: {output}"
    );
    assert!(
        output.contains("unavailable during turn"),
        "expected unavailable hint: {output}"
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
    set_input(&mut app, "/verbosity".to_string());
    let output = render_to_string(&app, 120, 16);
    assert!(
        output.contains("concise|normal|verbose"),
        "expected parameter hint to render the response-verbosity options actually accepted \
         by `/verbosity`: {output}"
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
    // F3-4: the TUI must surface the silent sandbox degradation
    // through the notification banner AND a transcript notice on the
    // first fallback; the agent's once-per-session gate means this
    // event only ever fires once, so we assert both surfaces hold a
    // single entry afterwards.
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

    let banner_message = app
        .app_notifications
        .current()
        .map(|notification| notification.message.clone())
        .expect("banner notification should be queued");
    assert!(
        banner_message.contains("shell sandbox degraded"),
        "banner text mismatch: {banner_message}"
    );
    assert!(
        banner_message.contains("macos-sandbox-exec"),
        "banner must name the degraded backend: {banner_message}"
    );

    let needle = "shell sandbox degraded";
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
        "fallback warning must render exactly once; transcript: {:?}",
        app.transcript
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

    let output = render_to_string(&app, 140, 18);
    // First and last lines must survive head-tail truncation; middle is elided.
    assert!(output.contains("line-00"), "head missing: {output}");
    assert!(output.contains("line-29"), "tail missing: {output}");
    assert!(
        !output.contains("line-14"),
        "middle should be elided: {output}"
    );
    assert!(
        output.contains("Ctrl-E to expand"),
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
        !output.contains("Ctrl-E to expand"),
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
        !output.contains("Ctrl-E to expand"),
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
        "Ctrl-T should open the overlay"
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
        !output.contains("Ctrl-E to expand"),
        "overlay should not show truncation ellipsis: {output}"
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
            "●",
            "idle prompt coin glyph must stay '●' at tick {tick}"
        );
        let span = prompt_coin_span(&app);
        assert_eq!(span.content.as_ref(), "●");
        assert_eq!(span.style.fg, Some(crate::render::palette::AMBER));
    }
}

#[test]
fn status_line_unset_keeps_legacy_two_row_layout() {
    let app = test_app(SessionMode::Build);
    // No /statusline configured ⇒ row 2 is the hints line, not a detail
    // line. Existing users see no visible change until they opt in.
    let lines = format_status_lines(&app, 120);
    assert_eq!(lines.len(), 2);
    let row2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(row2.contains("Enter send"), "row 2 should be hints: {row2}");
    assert!(
        !row2.contains("openai:"),
        "row 2 should not include the detail line: {row2}"
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
        Some(crate::render::palette::ACCENT_CYAN),
        "provider-and-model should paint with the Model accent (cyan)"
    );
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
        .start_session(squeezy_store::SessionMetadata::new(&config, "scripted"))
        .expect("seed older session");
    let newer = store
        .start_session(squeezy_store::SessionMetadata::new(&config, "scripted"))
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
async fn alt_nine_reports_no_session_when_slot_is_empty() {
    // With only one peer session in the store, Alt+9 has nothing to land
    // on; the handler still claims the keypress (so it doesn't fall
    // through to other Alt handlers) but reports the empty slot.
    let root = temp_workspace("quick_switch_empty");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let store = squeezy_store::SessionStore::open(&config);
    let only = store
        .start_session(squeezy_store::SessionMetadata::new(&config, "scripted"))
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

#[tokio::test]
async fn alt_one_skips_when_an_approval_is_pending() {
    // Modal-blocking states (approval, plan choice, config screen, …)
    // must not consume the Alt+1 press so the user can still type into
    // the prompt; the handler returns `false` and leaves status alone.
    let root = temp_workspace("quick_switch_blocked");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let store = squeezy_store::SessionStore::open(&config);
    store
        .start_session(squeezy_store::SessionMetadata::new(&config, "scripted"))
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
