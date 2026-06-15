use std::fs;

use squeezy_agent::AgentEvent;
use squeezy_core::{
    ContextEstimate, CostSnapshot, SessionMode, TranscriptItem, TurnId, TurnMetrics,
};
use tokio::sync::mpsc;

use super::*;
use crate::test_support::*;
use crate::*;

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
        .plan
        .pending_choice
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
async fn build_mode_proposed_plan_tag_passes_through_untouched() {
    // Outside Plan mode a literal <proposed_plan> tag is ordinary prose: it
    // must stream verbatim with no plan card, no plan-choice modal, and no
    // stripping of the surrounding narration.
    let root = temp_workspace("build_plan_passthrough");
    let config = test_config_with_root(SessionMode::Build, root.clone());
    let mut app = test_app_with_config(&config, SessionMode::Build);
    let (tx, rx) = mpsc::channel(8);
    app.turn_rx = Some(rx);
    tx.send(AgentEvent::AssistantDelta {
        turn_id: TurnId::new(1),
        delta: "before <proposed_plan>\nstep 1\n</proposed_plan> after".to_string(),
    })
    .await
    .expect("send delta");
    drop(tx);
    drain_agent_events(&mut app).await;

    assert_eq!(
        app.pending_assistant.text(),
        "before <proposed_plan>\nstep 1\n</proposed_plan> after",
        "Build-mode delta must stream through verbatim"
    );
    assert!(
        app.plan.pending_choice.is_none(),
        "Build mode must not open a plan-choice modal"
    );
    let plan_cards = app
        .transcript
        .iter()
        .filter(|entry| matches!(entry.kind, TranscriptEntryKind::PlanCard(_)))
        .count();
    assert_eq!(plan_cards, 0, "Build mode must not push a plan card");

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
        session_cost: None,
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
        session_cost: None,
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
    app.plan.current_id = Some(plan_id.clone());
    app.plan.pending_choice = Some(PendingPlanChoice {
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
    assert!(app.plan.pending_choice.is_none());
    // Execute auto-submits a "begin executing the plan" prompt. Under
    // per-turn re-attach (issue 16), the handoff stays set across turns
    // — the body is delivered on turn 1, then the lighter marker on
    // turns 2+, until a successful apply_patch clears it. The counter
    // advancing to 1 is the proof the body went out.
    assert_eq!(
        app.plan.pending_handoff.as_deref(),
        Some(plan_path.as_path()),
        "handoff persists for per-turn re-attach"
    );
    assert_eq!(
        app.plan.handoff_turns_seen, 1,
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
    app.plan.current_id = Some(plan_id.clone());
    app.plan.pending_choice = Some(PendingPlanChoice {
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
    assert!(app.plan.pending_choice.is_none());
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

    // Point the on-disk `current` pointer at the plan so the discard path is
    // exercised against a live pointer it must clean up.
    proposed_plan::set_active_plan(&root, proposed_plan::FALLBACK_SESSION_ID, &plan_id)
        .expect("set active plan");
    assert_eq!(
        proposed_plan::read_current_plan_id(&root, proposed_plan::FALLBACK_SESSION_ID).as_deref(),
        Some(plan_id.as_str()),
        "pointer is live before discard"
    );

    let config = test_config_with_root(SessionMode::Plan, root.clone());
    let mut agent = test_agent_with_config(config.clone());
    let mut app = test_app_with_config(&config, SessionMode::Plan);
    app.plan.current_id = Some(plan_id.clone());
    app.plan.pending_handoff = Some(plan_path.clone());
    app.plan.pending_choice = Some(PendingPlanChoice {
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
    assert!(app.plan.pending_choice.is_none());
    assert!(app.plan.current_id.is_none());
    assert!(app.plan.pending_handoff.is_none());
    assert!(
        proposed_plan::read_current_plan_id(&root, proposed_plan::FALLBACK_SESSION_ID).is_none(),
        "discard must clear the on-disk current pointer for the deleted plan"
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
    app.plan.current_id = Some(plan_id.clone());
    app.plan.pending_choice = Some(PendingPlanChoice {
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

    assert!(app.plan.pending_choice.is_none());
    assert!(plan_path.exists(), "refine must keep the plan file");
    assert_eq!(app.mode, SessionMode::Plan);
    assert_eq!(
        app.plan.current_id.as_deref(),
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
    app.plan.pending_choice = Some(PendingPlanChoice {
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
    assert_eq!(app.plan.pending_choice.as_ref().unwrap().selection_index, 1);
    handle_key(
        &mut app,
        &mut agent,
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    )
    .await
    .expect("handle key");
    assert_eq!(app.plan.pending_choice.as_ref().unwrap().selection_index, 0);

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
    app.plan.pending_choice = Some(PendingPlanChoice {
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
        app.plan.pending_choice.is_none(),
        "mode switch supersedes the post-plan choice prompt"
    );

    let _ = fs::remove_dir_all(&root);
}
