use std::collections::BTreeMap;

use squeezy_agent::{
    AgentEvent, JobEvent, JobId, JobNotification, JobSnapshot, MAX_JOB_NOTIFICATIONS,
    MAX_JOBS_RETAINED, RequestUserInputResponse,
};
use squeezy_core::SessionMode;
use squeezy_tools::{McpElicitationResponse, McpStatusSnapshot, ToolStatus};
use tokio::sync::broadcast;

use crate::{
    CONTEXT_NUDGE_THRESHOLD_RATIO_PCT, PendingApproval, PendingMcpElicitation, PendingPlanChoice,
    PendingRequestUserInput, TranscriptItem, TuiApp, TurnVisualState, compact_text,
    compaction_status_line, context_window_pct, dedupe_assistant_repeated_tool_output,
    format_approval_status_line, format_error_status, format_mcp_elicitation_status_line,
    format_mcp_status_snapshot, is_control_tool_name, proposed_plan, render, tool_call_label,
    tool_result_status_text,
};

pub(crate) async fn drain_agent_events(app: &mut TuiApp) {
    if let Some(mut rx) = app.turn_rx.take() {
        let mut keep_rx = true;
        let mut processed = false;
        while let Ok(event) = rx.try_recv() {
            processed = true;
            match event {
                AgentEvent::UserMessage { message, .. } => {
                    app.push_transcript_item(message);
                    app.pending_assistant.clear();
                    app.transcript_scroll_from_bottom = 0;
                }
                AgentEvent::Started { .. } => {
                    app.status = "thinking".to_string();
                    app.turn_visual = TurnVisualState::Running;
                    app.pending_reasoning.clear();
                    app.note_turn_started();
                }
                AgentEvent::ReasoningDelta { delta, .. } => {
                    // Always accumulate, even when show_reasoning_usage is
                    // off: the live render gates on the toggle separately,
                    // and the ReasoningSegment handler uses this buffer as a
                    // fallback when the provider's reasoning payload arrives
                    // with an empty summary (OpenAI's standard case — text
                    // streams via reasoning_text.delta events only, and
                    // response.output_item.done ships summary = []).
                    app.pending_reasoning.push_str(&delta);
                }
                AgentEvent::ReasoningSegment { mut snapshot, .. } => {
                    // Each reasoning block ends with its own segment event.
                    // OpenAI's payload arrives with an empty summary in the
                    // common case; fall back to the live streamed buffer so
                    // the persisted transcript entry isn't blank.
                    if snapshot.display_text.trim().is_empty()
                        && !app.pending_reasoning.trim().is_empty()
                    {
                        snapshot.display_text = std::mem::take(&mut app.pending_reasoning);
                    } else {
                        app.pending_reasoning.clear();
                    }
                    // Always persist to the transcript. The agent loop
                    // already records reasoning into the conversation
                    // history (LlmInputItem::Reasoning) independent of UI
                    // state; this transcript entry is the *display* record.
                    // Rendering gates on `show_reasoning_usage`, so toggling
                    // the option off hides the entry without dropping the
                    // data — flipping it back on reveals historical blocks.
                    if !snapshot.display_text.trim().is_empty() {
                        app.push_reasoning_segment(snapshot);
                    }
                }
                AgentEvent::AssistantDelta { delta, .. } => {
                    let extracted = app.proposed_plan.feed(&delta);
                    if !extracted.passthrough.is_empty() {
                        app.pending_assistant.push_delta(&extracted.passthrough);
                    }
                    for plan_body in extracted.completed {
                        let sid = app.plan_session_id().to_string();
                        // A non-None `current_plan_id` at the time a
                        // fresh block lands means this body is a
                        // refinement of the active plan, not a first-
                        // time draft. Captured for the styled card /
                        // diff renderer (PR-F).
                        let parent_plan_id = app.current_plan_id.clone();
                        let meta = proposed_plan::PlanMeta {
                            parent_plan_id: parent_plan_id.clone(),
                            model: Some(app.model.clone()),
                        };
                        match proposed_plan::persist_plan(
                            &app.workspace_root,
                            &sid,
                            &plan_body,
                            &meta,
                        ) {
                            Ok((plan_id, path)) => {
                                app.current_plan_id = Some(plan_id.clone());
                                app.push_plan_card(render::plan_card::PlanCardData {
                                    plan_id: plan_id.clone(),
                                    path: path.clone(),
                                    parent_plan_id,
                                });
                                app.pending_plan_choice = Some(PendingPlanChoice {
                                    plan_id,
                                    plan_path: path,
                                    selection_index: 0,
                                });
                            }
                            Err(err) => app.push_log(format!(
                                "proposed plan (could not persist under {}/{}: {err}):\n{plan_body}",
                                proposed_plan::PLAN_DIR,
                                sid
                            )),
                        }
                    }
                    // Intentionally preserve `transcript_scroll_from_bottom`
                    // here: if the user paged up to read history we would
                    // otherwise yank them back to the bottom on every delta.
                    // The End key (or any tool/status event that explicitly
                    // resets) brings them back to live view.
                }
                AgentEvent::ToolCallQueued { call, .. } => {
                    if is_control_tool_name(&call.name) {
                        app.status = "planning".to_string();
                    } else {
                        app.status = format!("queued {}", tool_call_label(&call));
                        app.remember_active_tool_call(call);
                    }
                }
                AgentEvent::ToolCallStarted { call, .. } => {
                    if is_control_tool_name(&call.name) {
                        app.status = "planning".to_string();
                    } else {
                        let label = tool_call_label(&call);
                        app.status = format!("running {label}");
                        // Surface in-flight tool work in the transcript so a
                        // long-running call is visible while it runs, not
                        // only after ToolCallCompleted lands. The result
                        // entry appears below this log line on completion.
                        app.push_log(format!("→ {label}"));
                        app.remember_active_tool_call(call);
                    }
                }
                AgentEvent::ToolCallCompleted { result, .. } => {
                    app.status = tool_result_status_text(&result);
                    if result.status == ToolStatus::Success
                        && matches!(result.tool_name.as_str(), "apply_patch" | "write_file")
                    {
                        app.last_turn_had_edits = true;
                        // First successful edit after a Plan→Build handoff
                        // means the plan is "in motion" — re-attaching it
                        // on later Build turns is just noise. Clear the
                        // handoff so the marker stops firing (issue 16).
                        if app.mode == SessionMode::Build && app.pending_plan_handoff.is_some() {
                            app.pending_plan_handoff = None;
                            app.plan_handoff_turns_seen = 0;
                            app.push_log("plan handoff cleared: plan is in motion".to_string());
                        }
                        // Plan-mode in-place refinement (issue 2): the model
                        // edited the active plan file via apply_patch. Re-
                        // surface the post-plan choice prompt so the user
                        // sees Execute/Refine/Discard/View against the new
                        // body without having to wait for another
                        // <proposed_plan> emission.
                        if app.mode == SessionMode::Plan
                            && let Some(plan_id) = app.current_plan_id.clone()
                        {
                            let sid = app.plan_session_id().to_string();
                            let plan_path =
                                proposed_plan::plan_file_for(&app.workspace_root, &sid, &plan_id);
                            if plan_path.exists() {
                                app.push_log(format!(
                                    "plan {plan_id} refined in place (apply_patch)"
                                ));
                                app.pending_plan_choice = Some(PendingPlanChoice {
                                    plan_id,
                                    plan_path,
                                    selection_index: 0,
                                });
                            }
                        }
                    }
                    let call = app.active_tool_calls.remove(&result.call_id);
                    app.refresh_active_tool_name();
                    app.push_tool_result_with_call(result, call);
                }
                AgentEvent::TaskStateUpdated { snapshot, .. } => {
                    app.task_state = Some(snapshot);
                    if app.active_tool_calls.is_empty() {
                        app.status = "planning".to_string();
                    }
                }
                AgentEvent::McpStatusUpdated { snapshot, .. } => {
                    apply_mcp_status_update(app, snapshot);
                }
                AgentEvent::JobUpdated { job } => {
                    apply_job_update(app, job);
                }
                AgentEvent::JobNotification { notification } => {
                    apply_job_notification(app, notification);
                }
                AgentEvent::ContextCompacted { report, .. } => {
                    app.context_compaction.last = Some(report.record.clone());
                    app.context_compaction.generation = report.record.generation;
                    app.context_compaction.summary = Some(report.summary.clone());
                    app.context_compaction.history.push(report.record.clone());
                    app.context_estimate = report.record.after.clone();
                    app.context_compaction_nudge_shown = false;
                    app.status = compaction_status_line(&report.record);
                    app.push_log(format!(
                        "context compacted gen={} trigger={} items={} tok {}->{}",
                        report.record.generation,
                        report.record.trigger.as_str(),
                        report.record.dropped_items,
                        report.record.before.estimated_tokens,
                        report.record.after.estimated_tokens
                    ));
                }
                AgentEvent::SubagentStarted { agent, prompt, .. } => {
                    app.status = format!("{agent} subagent running");
                    app.push_log(format!("{agent} subagent started: {prompt}"));
                }
                AgentEvent::SubagentCompleted {
                    agent,
                    summary,
                    metrics,
                    ..
                } => {
                    app.status = format!("{agent} subagent completed");
                    app.push_log(format!(
                        "{agent} subagent completed tools={} bytes={} summary={}",
                        metrics.subagent_tool_calls.max(metrics.tool_calls),
                        metrics.subagent_bytes_read.max(metrics.bytes_read),
                        compact_text(&summary, 180)
                    ));
                }
                AgentEvent::SubagentFailed {
                    agent,
                    error,
                    metrics,
                    ..
                } => {
                    app.status = format!("{agent} subagent failed");
                    app.push_log(format!(
                        "{agent} subagent failed tools={} bytes={} error={}",
                        metrics.subagent_tool_calls.max(metrics.tool_calls),
                        metrics.subagent_bytes_read.max(metrics.bytes_read),
                        compact_text(&error, 180)
                    ));
                }
                AgentEvent::AiReviewerTripped { reason, .. } => {
                    app.status = "approval review paused".to_string();
                    app.push_log(format!("AI approval reviewer paused: {reason}"));
                }
                AgentEvent::ApprovalRequested {
                    request,
                    decision_tx,
                    ..
                } => {
                    app.status = format_approval_status_line(&request);
                    app.approval_selection_index = 0;
                    app.pending_approval = Some(PendingApproval {
                        request,
                        decision_tx,
                    });
                    break;
                }
                AgentEvent::McpElicitationRequested {
                    request,
                    response_tx,
                    ..
                } => {
                    if let Some(previous) = app.pending_mcp_elicitation.take() {
                        let _ = previous.response_tx.send(McpElicitationResponse::cancel());
                    }
                    app.status = format_mcp_elicitation_status_line(&request);
                    app.mcp_elicitation_selection_index = 0;
                    app.pending_mcp_elicitation = Some(PendingMcpElicitation {
                        request,
                        response_tx,
                    });
                    break;
                }
                AgentEvent::RequestUserInputRequested {
                    request,
                    response_tx,
                    ..
                } => {
                    if let Some(previous) = app.pending_request_user_input.take() {
                        let _ = previous
                            .response_tx
                            .send(RequestUserInputResponse::cancelled());
                    }
                    app.status = format!("plan-mode question: {}", request.question);
                    app.pending_request_user_input = Some(PendingRequestUserInput {
                        request,
                        response_tx,
                        selection_index: 0,
                    });
                    break;
                }
                AgentEvent::Completed {
                    message,
                    cost,
                    metrics,
                    context_estimate,
                    ..
                } => {
                    if let Some(message) = dedupe_assistant_repeated_tool_output(app, message) {
                        app.push_transcript_item(message);
                    }
                    app.pending_assistant.clear();
                    app.pending_reasoning.clear();
                    finalize_proposed_plan(app);
                    app.context_estimate = context_estimate;
                    app.cancelled_prompt = None;
                    if app.last_turn_had_edits {
                        app.push_log(
                            "turn complete · /diff to inspect changes · /undo to revert this turn"
                                .to_string(),
                        );
                        app.last_turn_had_edits = false;
                    }
                    maybe_push_context_compaction_nudge(app);
                    app.cost = cost;
                    app.metrics = metrics;
                    app.status = "ready".to_string();
                    app.turn_visual = TurnVisualState::Succeeded;
                    app.clear_active_tools();
                    app.pending_mcp_elicitation = None;
                    cancel_pending_request_user_input(app);
                    app.note_turn_finished();
                    // Preserve the user's scroll position; if they paged up
                    // mid-turn we shouldn't snap them down on completion.
                    app.cancel = None;
                    keep_rx = false;
                    break;
                }
                AgentEvent::CostWarning { status, .. } => {
                    let notice = format!(
                        "session cost crossed warning threshold: spent ${:.4} of ${:.2} cap ({}%)",
                        status.spent_usd_micros as f64 / 1_000_000.0,
                        status.cap_usd_micros as f64 / 1_000_000.0,
                        status.percent
                    );
                    app.push_transcript_item(TranscriptItem::system(notice));
                }
                AgentEvent::ShellSandboxBestEffortFallback {
                    backend,
                    fallback_count,
                    ..
                } => {
                    // Fires at most once per session — the agent's
                    // `maybe_emit_shell_sandbox_fallback_warning` gates on
                    // the tool-layer one-shot latch. Surface both the
                    // banner (status-bar pane) and a transcript notice
                    // so users notice mid-turn AND have a durable record.
                    let banner = format!(
                        "shell sandbox degraded: {backend} unavailable, running without OS isolation (best_effort)"
                    );
                    app.app_notifications
                        .push(banner, crate::notification::Severity::Warn);
                    let notice = format!(
                        "shell sandbox degraded: backend `{backend}` unavailable; subsequent shell calls run without OS isolation under mode=best_effort (fallback #{fallback_count})"
                    );
                    app.push_transcript_item(TranscriptItem::system(notice));
                }
                AgentEvent::CostUpdate {
                    tool_count,
                    input_tokens,
                    micro_usd,
                    ..
                } => {
                    // Progressive per-turn cost lives in the status bar so
                    // the transcript stays free of running-total noise.
                    // Suppress identical resends (the broker fires on a
                    // tool-count stride, not a token-delta).
                    app.update_turn_progress(tool_count, input_tokens, micro_usd);
                }
                AgentEvent::ToolProgress {
                    tool_name,
                    elapsed_ms,
                    ..
                } => {
                    // Heartbeat events feed the active-tool elapsed clock
                    // in the status bar — never the transcript log, where
                    // one append per second drowns the actual output.
                    app.note_active_tool_progress(&tool_name, elapsed_ms);
                }
                AgentEvent::Cancelled { .. } => {
                    let mut message = "cancelled; edit prompt or retry".to_string();
                    if app.last_turn_had_edits {
                        message.push_str(" · /undo to revert this turn's edits");
                    }
                    app.status = message;
                    app.turn_visual = TurnVisualState::Failed;
                    app.push_log("turn cancelled".to_string());
                    if app.last_turn_had_edits {
                        app.push_log(
                            "/diff to inspect changes · /undo to revert this turn".to_string(),
                        );
                        app.last_turn_had_edits = false;
                    }
                    app.pending_assistant.clear();
                    app.pending_reasoning.clear();
                    finalize_proposed_plan(app);
                    app.clear_active_tools();
                    app.pending_mcp_elicitation = None;
                    cancel_pending_request_user_input(app);
                    app.note_turn_finished();
                    app.cancel = None;
                    keep_rx = false;
                    break;
                }
                AgentEvent::Failed { error, .. } => {
                    let mut status = format_error_status(&error);
                    if app.last_turn_had_edits {
                        status.push_str(" · /undo to revert this turn's edits");
                    }
                    app.status = status;
                    app.turn_visual = TurnVisualState::Failed;
                    app.push_log(format!("turn failed: {}", app.status));
                    if app.last_turn_had_edits {
                        app.last_turn_had_edits = false;
                    }
                    app.pending_assistant.clear();
                    app.pending_reasoning.clear();
                    finalize_proposed_plan(app);
                    app.clear_active_tools();
                    app.pending_mcp_elicitation = None;
                    cancel_pending_request_user_input(app);
                    app.note_turn_finished();
                    app.cancel = None;
                    keep_rx = false;
                    break;
                }
            }
        }
        if keep_rx {
            app.turn_rx = Some(rx);
        }
        if processed {
            app.needs_redraw = true;
        }
    }
}

/// Update `app` for an MCP status snapshot. The transcript log line is
/// pushed only when the status text actually changed AND either the new
/// or prior snapshot had configured servers — without that gate, users
/// with no MCP configured see "mcp status none" stamped on every turn.
pub(crate) fn apply_mcp_status_update(app: &mut TuiApp, snapshot: McpStatusSnapshot) {
    let summary = format_mcp_status_snapshot(&snapshot);
    let prior_summary = app.mcp_status.as_ref().map(format_mcp_status_snapshot);
    let prior_had_servers = app
        .mcp_status
        .as_ref()
        .is_some_and(|prior| !prior.per_server.is_empty());
    let now_has_servers = !snapshot.per_server.is_empty();
    app.mcp_status = Some(snapshot);
    app.status = format!("mcp {summary}");
    let changed = prior_summary.as_deref() != Some(&summary);
    if changed && (now_has_servers || prior_had_servers) {
        app.push_log(format!("mcp status {summary}"));
    }
}

pub(crate) fn cancel_pending_request_user_input(app: &mut TuiApp) {
    if let Some(pending) = app.pending_request_user_input.take() {
        let _ = pending
            .response_tx
            .send(RequestUserInputResponse::cancelled());
    }
}

/// Push a one-shot system transcript advisory when the post-turn context
/// estimate is closing in on the auto-compaction threshold. We surface
/// this *before* the next turn so the user has a chance to `/pin` or
/// `/compact` deliberately rather than discovering after the fact that
/// the conversation has been rewritten.
///
/// The nudge fires at [`CONTEXT_NUDGE_THRESHOLD_RATIO_PCT`] of the
/// compaction threshold (well before the 100% mark that triggers
/// auto-compaction) and is suppressed once the estimate has already
/// reached the threshold — by that point the auto-compaction surface
/// is the actionable signal and a "consider /compact" advisory is
/// stale advice.
pub(crate) fn maybe_push_context_compaction_nudge(app: &mut TuiApp) {
    if app.context_compaction_threshold == 0 || app.context_compaction_nudge_shown {
        return;
    }
    let pct = context_window_pct(
        app.context_estimate.estimated_tokens,
        app.context_compaction_threshold,
    );
    if !(CONTEXT_NUDGE_THRESHOLD_RATIO_PCT..100).contains(&pct) {
        return;
    }
    app.context_compaction_nudge_shown = true;
    app.push_log(format!(
        "context {pct}% of auto-compact threshold ({used}/{threshold} tok) — /pin to keep what matters · /compact to summarize before auto-compaction triggers",
        used = app.context_estimate.estimated_tokens,
        threshold = app.context_compaction_threshold,
    ));
}

/// Reset the `<proposed_plan>` extractor at a turn boundary. The
/// agent's Completed message is the single rendered artifact for the
/// turn — re-injecting leftover bytes from an unterminated block here
/// duplicated content that the assistant message already contained. If
/// the extractor was still inside an open block we log an advisory so
/// the model-output bug is visible without polluting the transcript.
pub(crate) fn finalize_proposed_plan(app: &mut TuiApp) {
    let leftover = app.proposed_plan.finalize();
    if !leftover.is_empty() {
        app.push_log("ignored unterminated <proposed_plan> block from the model".to_string());
    }
    app.proposed_plan = proposed_plan::ProposedPlanExtractor::new();
}

pub(crate) fn drain_job_events(app: &mut TuiApp) {
    let mut processed = false;
    while let Some(rx) = app.job_rx.as_mut() {
        // Release the borrow on `app.job_rx` before any branch that
        // mutates other `app` fields — `apply_job_update` and
        // `apply_job_notification` both take `&mut TuiApp`.
        let event = rx.try_recv();
        match event {
            Ok(JobEvent::Updated(job)) => {
                apply_job_update(app, job);
                processed = true;
            }
            Ok(JobEvent::Notification(notification)) => {
                apply_job_notification(app, notification);
                processed = true;
            }
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                app.status = format!("skipped {skipped} job updates");
                processed = true;
            }
            Err(broadcast::error::TryRecvError::Closed) => {
                app.job_rx = None;
                break;
            }
        }
    }
    if processed {
        app.needs_redraw = true;
    }
}

pub(crate) fn apply_job_update(app: &mut TuiApp, job: JobSnapshot) {
    app.jobs.insert(job.id, job);
    prune_tui_jobs(&mut app.jobs);
}

fn prune_tui_jobs(jobs: &mut BTreeMap<JobId, JobSnapshot>) {
    if jobs.len() <= MAX_JOBS_RETAINED {
        return;
    }
    let mut terminal: Vec<(JobId, u64)> = jobs
        .iter()
        .filter(|(_, job)| job.status.is_terminal())
        .map(|(id, job)| (*id, job.ended_at_ms.unwrap_or(0)))
        .collect();
    terminal.sort_by_key(|(_, ended_at)| *ended_at);
    let mut to_remove = jobs.len().saturating_sub(MAX_JOBS_RETAINED);
    for (id, _) in terminal {
        if to_remove == 0 {
            break;
        }
        jobs.remove(&id);
        to_remove -= 1;
    }
}

pub(crate) fn apply_job_notification(app: &mut TuiApp, notification: JobNotification) {
    app.status = format!(
        "job {} {}: {}",
        notification.job_id,
        notification.status.as_str(),
        notification.summary
    );
    if app.notifications.back().is_some_and(|previous| {
        previous.job_id == notification.job_id
            && previous.status == notification.status
            && previous.summary == notification.summary
    }) {
        return;
    }
    app.notifications.push_back(notification);
    while app.notifications.len() > MAX_JOB_NOTIFICATIONS {
        app.notifications.pop_front();
    }
}
