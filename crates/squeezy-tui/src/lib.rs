use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env,
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use squeezy_agent::{
    Agent, AgentEvent, JobEvent, JobId, JobNotification, JobSnapshot, MAX_JOB_NOTIFICATIONS,
    MAX_JOBS_RETAINED, SessionAccountingSnapshot, ToolApprovalDecision, ToolApprovalRequest,
};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextCompactionRecord, ContextCompactionState, ContextEstimate,
    PermissionPolicy, ResponseVerbosity, Result, Role, SessionMode, SqueezyError, StatusVerbosity,
    TaskStateSnapshot, TaskStepStatus, TelemetryConfig, ToolOutputVerbosity, TranscriptDefault,
    TranscriptItem,
};
use squeezy_llm::{LlmProvider, RequestTokenEstimate};
use squeezy_skills::{HelpStatus, SqueezyHelp};
use squeezy_store::{BugReportBundle, BugReportOptions, SessionQuery, parse_bug_report_section};
use squeezy_telemetry::PreparedFeedback;
use squeezy_tools::{ToolCall, ToolResult, ToolStatus};
use squeezy_vcs::{DiffMode, DiffOptions, GitVcs, VcsKind};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const INLINE_PASTE_MAX_BYTES: usize = 512;
const LONG_ASSISTANT_CHARS: usize = 1_200;
const TOOL_PREVIEW_COMPACT_BYTES: usize = 300;
const TOOL_PREVIEW_NORMAL_BYTES: usize = 1_200;
const TOOL_PREVIEW_VERBOSE_BYTES: usize = 4_000;
const AMBER: Color = Color::Rgb(245, 158, 11);
const AMBER_DIM: Color = Color::Rgb(180, 83, 9);
const GOLD: Color = Color::Rgb(251, 191, 36);
const QUIET: Color = Color::DarkGray;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupProfile {
    pub onboarding_summary: Option<String>,
    pub languages: String,
}

pub async fn run(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Result<()> {
    run_inner(config, provider, None, StartupProfile::default()).await
}

pub async fn run_with_onboarding(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    onboarding_summary: Option<String>,
) -> Result<()> {
    run_inner(
        config,
        provider,
        None,
        StartupProfile {
            onboarding_summary,
            languages: String::new(),
        },
    )
    .await
}

pub async fn run_with_startup_profile(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    startup: StartupProfile,
) -> Result<()> {
    run_inner(config, provider, None, startup).await
}

pub async fn resume(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    session_id: String,
) -> Result<()> {
    // Resume reuses the transcript already on disk, so it intentionally
    // doesn't seed a fresh onboarding summary on top.
    run_inner(
        config,
        provider,
        Some(session_id),
        StartupProfile::default(),
    )
    .await
}

async fn run_inner(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    resume_session_id: Option<String>,
    startup: StartupProfile,
) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let (mut agent, initial_transcript) = if let Some(session_id) = resume_session_id {
        Agent::resume(config.clone(), provider, &session_id)?
    } else {
        (Agent::new(config.clone(), provider), Vec::new())
    };
    let mut app = TuiApp::new(
        agent.provider_name(),
        &config,
        agent.session_mode(),
        startup,
    );
    for item in initial_transcript {
        app.push_transcript_item(item);
    }
    app.attachments = agent.context_attachments_snapshot().await;
    app.context_compaction = agent.context_compaction_snapshot().await;
    app.context_estimate = agent.context_estimate_snapshot().await;
    app.job_rx = Some(agent.subscribe_jobs());
    app.jobs = agent
        .jobs_snapshot()
        .into_iter()
        .map(|job| (job.id, job))
        .collect();
    app.notifications = agent.job_notifications().into_iter().collect();
    if let Some(session_id) = agent.session_id() {
        app.status = format!("session {session_id}");
    }

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        drain_job_events(&mut app);
        drain_agent_events(&mut app).await;
        if poll_input(&mut app, &mut agent, config.tick_rate).await? {
            break;
        }
    }

    agent
        .finish_session(squeezy_store::SessionStatus::Completed)
        .await;
    agent.flush_telemetry().await;

    Ok(())
}

async fn drain_agent_events(app: &mut TuiApp) {
    if let Some(mut rx) = app.turn_rx.take() {
        let mut keep_rx = true;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::UserMessage { message, .. } => {
                    app.push_transcript_item(message);
                    app.pending_assistant.clear();
                    app.transcript_scroll_from_bottom = 0;
                }
                AgentEvent::Started { .. } => {
                    app.status = "thinking".to_string();
                }
                AgentEvent::AssistantDelta { delta, .. } => {
                    app.pending_assistant.push_str(&delta);
                    // Intentionally preserve `transcript_scroll_from_bottom`
                    // here: if the user paged up to read history we would
                    // otherwise yank them back to the bottom on every delta.
                    // The End key (or any tool/status event that explicitly
                    // resets) brings them back to live view.
                }
                AgentEvent::ToolCallQueued { call, .. } => {
                    app.status = format!("queued {}", call.name);
                    app.push_tool_call(call);
                }
                AgentEvent::ToolCallStarted { call, .. } => {
                    app.status = format!("running {}", call.name);
                }
                AgentEvent::ToolCallCompleted { result, .. } => {
                    app.status = format!(
                        "{} {:?} {}B{}",
                        result.tool_name,
                        result.status,
                        result.cost_hint.output_bytes,
                        if result.cost_hint.truncated {
                            " truncated"
                        } else {
                            ""
                        }
                    );
                    if result.cost_hint.redactions > 0 {
                        app.status
                            .push_str(&format!(" redacted={}", result.cost_hint.redactions));
                    }
                    app.push_tool_result(result);
                }
                AgentEvent::TaskStateUpdated { snapshot, .. } => {
                    app.task_state = Some(snapshot);
                    app.status = "task state updated".to_string();
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
                AgentEvent::ApprovalRequested {
                    request,
                    decision_tx,
                    ..
                } => {
                    app.status = format_approval_status_line(&request);
                    app.pending_approval = Some(PendingApproval {
                        request,
                        decision_tx,
                    });
                    break;
                }
                AgentEvent::Completed {
                    message,
                    cost,
                    metrics,
                    ..
                } => {
                    app.push_transcript_item(message);
                    app.pending_assistant.clear();
                    app.cost = cost;
                    app.metrics = metrics;
                    app.status = "ready".to_string();
                    // Preserve the user's scroll position; if they paged up
                    // mid-turn we shouldn't snap them down on completion.
                    app.cancel = None;
                    keep_rx = false;
                    break;
                }
                AgentEvent::Cancelled { .. } => {
                    app.status = "cancelled; edit prompt or retry".to_string();
                    app.push_log("turn cancelled".to_string());
                    app.pending_assistant.clear();
                    app.cancel = None;
                    keep_rx = false;
                    break;
                }
                AgentEvent::Failed { error, .. } => {
                    app.status = format_error_status(&error);
                    app.push_log(format!("turn failed: {}", app.status));
                    app.pending_assistant.clear();
                    app.cancel = None;
                    keep_rx = false;
                    break;
                }
            }
        }
        if keep_rx {
            app.turn_rx = Some(rx);
        }
    }
}

fn drain_job_events(app: &mut TuiApp) {
    loop {
        let event = match app.job_rx.as_mut() {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match event {
            Ok(JobEvent::Updated(job)) => apply_job_update(app, job),
            Ok(JobEvent::Notification(notification)) => apply_job_notification(app, notification),
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                app.status = format!("skipped {skipped} job updates");
            }
            Err(broadcast::error::TryRecvError::Closed) => {
                app.job_rx = None;
                break;
            }
        }
    }
}

fn apply_job_update(app: &mut TuiApp, job: JobSnapshot) {
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

fn apply_job_notification(app: &mut TuiApp, notification: JobNotification) {
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

async fn poll_input(app: &mut TuiApp, agent: &mut Agent, tick_rate: Duration) -> Result<bool> {
    if !event::poll(tick_rate).map_err(|err| SqueezyError::Terminal(err.to_string()))? {
        return Ok(false);
    }

    match event::read().map_err(|err| SqueezyError::Terminal(err.to_string()))? {
        Event::Key(key) => handle_key(app, agent, key).await,
        Event::Paste(text) => {
            handle_paste(app, agent, text).await?;
            Ok(false)
        }
        _ => Ok(false),
    }
}

async fn handle_key(app: &mut TuiApp, agent: &mut Agent, key: KeyEvent) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if cancel_active_turn(app) {
            return Ok(false);
        }
        return Ok(true);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
        copy_to_clipboard(app, ClipboardTarget::LastAssistant);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
        toggle_selected_transcript_entry(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
        if app.task_state.is_some() {
            app.task_panel_collapsed = !app.task_panel_collapsed;
            app.status = if app.task_panel_collapsed {
                "task panel collapsed".to_string()
            } else {
                "task panel expanded".to_string()
            };
        }
        return Ok(false);
    }

    if key.code == KeyCode::BackTab {
        switch_mode(app, agent, None, "tui_shift_tab");
        return Ok(false);
    }

    // The /plan and /build shortcuts intentionally fire before
    // `handle_approval_key` and the regular Enter handler so a user can flip
    // modes between turns without first clearing the input buffer. When an
    // approval prompt is pending or a turn is in flight, `switch_mode`
    // refuses the change and the input is preserved so it survives the
    // current interaction.
    if key.code == KeyCode::Enter
        && let Some(mode) = mode_command(app.input.trim())
    {
        switch_mode(app, agent, Some(mode), "tui_command");
        if app.turn_rx.is_none() && app.pending_approval.is_none() {
            app.input.clear();
        }
        return Ok(false);
    }

    if key.code == KeyCode::Esc && app.cancel.is_some() {
        cancel_active_turn(app);
        return Ok(false);
    }

    if handle_approval_key(app, key) {
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc => {
            if cancel_active_turn(app) {
                Ok(false)
            } else {
                Ok(true)
            }
        }
        // Scroll keys intentionally leave `app.status` alone so that
        // useful messages (tool results, errors, approval prompts) stay
        // visible while the user navigates history. The status footer
        // already surfaces a "scrolled" marker when off the bottom.
        KeyCode::PageUp => {
            app.transcript_scroll_from_bottom = app.transcript_scroll_from_bottom.saturating_add(8);
            Ok(false)
        }
        KeyCode::PageDown => {
            app.transcript_scroll_from_bottom = app.transcript_scroll_from_bottom.saturating_sub(8);
            Ok(false)
        }
        KeyCode::Home => {
            app.transcript_scroll_from_bottom = u16::MAX;
            Ok(false)
        }
        KeyCode::End => {
            app.transcript_scroll_from_bottom = 0;
            Ok(false)
        }
        KeyCode::Up => {
            select_previous_transcript_entry(app);
            Ok(false)
        }
        KeyCode::Down => {
            select_next_transcript_entry(app);
            Ok(false)
        }
        KeyCode::Enter => {
            if app.turn_rx.is_some() {
                app.status = "turn already running; press Ctrl-C to cancel".to_string();
                return Ok(false);
            }
            let input = app.input.trim().to_string();
            if input.is_empty() {
                app.status = "enter a prompt first".to_string();
                return Ok(false);
            }
            app.input.clear();
            if handle_slash_command(app, agent, &input).await {
                return Ok(false);
            }
            let cancel = CancellationToken::new();
            app.task_state = None;
            app.task_panel_collapsed = false;
            app.turn_rx = Some(agent.start_turn_with_response_verbosity(
                input,
                cancel.clone(),
                app.response_verbosity,
            ));
            app.cancel = Some(cancel);
            app.status = "starting turn".to_string();
            Ok(false)
        }
        KeyCode::Backspace => {
            app.input.pop();
            Ok(false)
        }
        KeyCode::Char(ch) => {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                app.input.push(ch);
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

async fn handle_paste(app: &mut TuiApp, agent: &mut Agent, text: String) -> Result<()> {
    if app.turn_rx.is_some() || app.pending_approval.is_some() {
        app.status = "paste unavailable during active turn".to_string();
        return Ok(());
    }
    if is_inline_paste(&text) {
        app.input.push_str(&text);
        return Ok(());
    }
    match agent.attach_pasted_context(text).await {
        Ok(update) => {
            app.attachments = agent.context_attachments_snapshot().await;
            app.status = attachment_update_status("paste", &update);
        }
        Err(error) => app.status = format!("paste attach failed: {error}"),
    }
    Ok(())
}

fn is_inline_paste(text: &str) -> bool {
    text.len() <= INLINE_PASTE_MAX_BYTES && !text.contains('\n') && !text.contains('\r')
}

fn cancel_active_turn(app: &mut TuiApp) -> bool {
    let Some(cancel) = &app.cancel else {
        return false;
    };
    cancel.cancel();
    if let Some(pending) = app.pending_approval.take() {
        let _ = pending.decision_tx.send(ToolApprovalDecision::Cancelled);
    }
    app.status = "cancelling".to_string();
    true
}

async fn handle_slash_command(app: &mut TuiApp, agent: &mut Agent, input: &str) -> bool {
    let mut parts = input.split_whitespace();
    let Some(command) = parts.next() else {
        return false;
    };
    let rest = input
        .strip_prefix(command)
        .map(str::trim)
        .unwrap_or_default();
    match command {
        "/cost" => {
            let snapshot = agent.session_accounting_snapshot().await;
            app.status = "cost snapshot".to_string();
            app.push_transcript_item(TranscriptItem::system(format_cost_command(&snapshot)));
            return true;
        }
        "/context" => {
            let snapshot = agent.session_accounting_snapshot().await;
            app.status = "context snapshot".to_string();
            app.push_transcript_item(TranscriptItem::system(format_context_command(&snapshot)));
            return true;
        }
        "/help" => {
            handle_help_command(app, rest);
            return true;
        }
        "/feedback" => {
            handle_feedback_command(app, agent, rest).await;
            return true;
        }
        "/report" => {
            handle_report_command(app, agent, rest).await;
            return true;
        }
        "/attach" => {
            let path = input.trim_start_matches("/attach").trim();
            if path.is_empty() {
                app.status = "usage: /attach <path>".to_string();
                return true;
            }
            match agent.attach_file_context(PathBuf::from(path)).await {
                Ok(update) => {
                    app.attachments = agent.context_attachments_snapshot().await;
                    app.status = attachment_update_status("file", &update);
                }
                Err(error) => app.status = format!("attach failed: {error}"),
            }
            return true;
        }
        "/attachments" => {
            app.attachments = agent.context_attachments_snapshot().await;
            if app.attachments.is_empty() {
                app.status = "no attached context".to_string();
            } else {
                app.status = format!("{} attached context item(s)", app.attachments.len());
                app.push_transcript_item(TranscriptItem::system(format_attachment_list(
                    &app.attachments,
                )));
            }
            return true;
        }
        "/detach" => {
            let Some(id) = parts.next() else {
                app.status = "usage: /detach <attachment_id>".to_string();
                return true;
            };
            match agent.detach_context_attachment(id).await {
                Ok(attachment) => {
                    app.attachments = agent.context_attachments_snapshot().await;
                    app.status = format!("detached {}", attachment.id);
                }
                Err(error) => app.status = format!("detach failed: {error}"),
            }
            return true;
        }
        "/compact" => {
            match agent.compact_context_manual().await {
                Ok(report) => {
                    app.context_compaction = agent.context_compaction_snapshot().await;
                    app.context_estimate = report.record.after.clone();
                    app.status = compaction_status_line(&report.record);
                    app.push_log(format!(
                        "context compacted gen={} items={} tok {}->{}",
                        report.record.generation,
                        report.record.dropped_items,
                        report.record.before.estimated_tokens,
                        report.record.after.estimated_tokens
                    ));
                }
                Err(error) => app.status = format!("compact failed: {error}"),
            }
            return true;
        }
        "/pins" => {
            app.context_compaction = agent.context_compaction_snapshot().await;
            if app.context_compaction.pinned.is_empty() {
                app.status = "no pinned context".to_string();
            } else {
                app.status = format!(
                    "{} pinned context item(s)",
                    app.context_compaction.pinned.len()
                );
                app.push_transcript_item(TranscriptItem::system(format_pin_list(
                    &app.context_compaction,
                )));
            }
            return true;
        }
        "/pin" => {
            let target = parts.next().unwrap_or("selected");
            match pin_source(app, target) {
                PinSourceResult::Found(label, summary, source) => {
                    match agent.pin_context_entry(label, summary, source).await {
                        Ok(pin) => {
                            app.context_compaction = agent.context_compaction_snapshot().await;
                            app.status = format!("pinned {}", pin.id);
                        }
                        Err(error) => app.status = format!("pin failed: {error}"),
                    }
                }
                PinSourceResult::NoEntry => {
                    app.status = "no transcript entry to pin".to_string();
                }
                PinSourceResult::UnknownTarget => {
                    app.status = "usage: /pin selected|last".to_string();
                }
            }
            return true;
        }
        "/unpin" => {
            let id = parts.next().map(str::trim).filter(|raw| !raw.is_empty());
            let Some(id) = id else {
                app.status = "usage: /unpin <pin_id>".to_string();
                return true;
            };
            match agent.unpin_context_entry(id).await {
                Ok(pin) => {
                    app.context_compaction = agent.context_compaction_snapshot().await;
                    app.status = format!("unpinned {}", pin.id);
                }
                Err(error) => app.status = format!("unpin failed: {error}"),
            }
            return true;
        }
        "/collapse" | "/expand" => {
            let category = match parts.next() {
                Some(value) => match parse_transcript_category(value) {
                    Some(category) => category,
                    None => {
                        app.status = "usage: /collapse [all|tools|logs|diffs|receipts|assistant]"
                            .to_string();
                        return true;
                    }
                },
                None => TranscriptCategory::All,
            };
            let collapsed = command == "/collapse";
            let changed = set_transcript_collapsed(app, category, collapsed);
            app.status = format!(
                "{} {} transcript entr{}",
                if collapsed { "collapsed" } else { "expanded" },
                changed,
                if changed == 1 { "y" } else { "ies" }
            );
            return true;
        }
        "/verbosity" => {
            let Some(value) = parts.next() else {
                app.status = "usage: /verbosity concise|normal|verbose".to_string();
                return true;
            };
            let Some(verbosity) = parse_response_verbosity(value) else {
                app.status = "usage: /verbosity concise|normal|verbose".to_string();
                return true;
            };
            app.response_verbosity = verbosity;
            app.status = format!("response verbosity {}", verbosity.as_str());
            return true;
        }
        "/tool-verbosity" => {
            let Some(value) = parts.next() else {
                app.status = "usage: /tool-verbosity compact|normal|verbose".to_string();
                return true;
            };
            let Some(verbosity) = parse_tool_output_verbosity(value) else {
                app.status = "usage: /tool-verbosity compact|normal|verbose".to_string();
                return true;
            };
            app.tool_output_verbosity = verbosity;
            app.status = format!("tool output verbosity {}", verbosity.as_str());
            return true;
        }
        "/jobs" => {
            sync_jobs_from_agent(app, agent);
            let jobs = format_jobs_list(app);
            app.status = format!("{} jobs", app.jobs.len());
            app.push_transcript_item(TranscriptItem::system(jobs));
            return true;
        }
        "/job" => {
            let Some(raw_id) = parts.next() else {
                app.status = "usage: /job <job_id>".to_string();
                return true;
            };
            let Some(id) = parse_job_id(raw_id) else {
                app.status = "job id must be a number".to_string();
                return true;
            };
            sync_jobs_from_agent(app, agent);
            match app
                .jobs
                .get(&id)
                .cloned()
                .or_else(|| agent.job_snapshot(id))
            {
                Some(job) => {
                    app.status = format!("job {} {}", job.id, job.status.as_str());
                    app.push_transcript_item(TranscriptItem::system(format_job_detail(&job)));
                }
                None => app.status = format!("job {id} not found"),
            }
            return true;
        }
        "/job-cancel" => {
            let Some(raw_id) = parts.next() else {
                app.status = "usage: /job-cancel <job_id>".to_string();
                return true;
            };
            let Some(id) = parse_job_id(raw_id) else {
                app.status = "job id must be a number".to_string();
                return true;
            };
            if agent.cancel_job(id) {
                app.status = format!("cancelling job {id}");
                sync_jobs_from_agent(app, agent);
            } else {
                app.status = format!("job {id} not active");
            }
            return true;
        }
        "/sessions" => {
            match agent.list_sessions(&SessionQuery::default()) {
                Ok(sessions) => {
                    app.status = format!("{} sessions", sessions.len());
                    app.push_transcript_item(TranscriptItem::system(
                        sessions
                            .into_iter()
                            .take(10)
                            .map(|session| {
                                format!(
                                    "{} {} {}",
                                    session.session_id,
                                    session.status.as_str(),
                                    session
                                        .first_user_task
                                        .or(session.latest_summary)
                                        .unwrap_or_default()
                                        .replace('\n', " ")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ));
                }
                Err(error) => app.status = format!("session list failed: {error}"),
            }
            return true;
        }
        "/session" => {
            let Some(session_id) = parts.next() else {
                app.status = "usage: /session <session_id>".to_string();
                return true;
            };
            match agent.show_session(session_id) {
                Ok(record) => {
                    app.status = format!(
                        "session {}: {} events={} redactions={}",
                        record.metadata.session_id,
                        record.metadata.status.as_str(),
                        record.metadata.event_count,
                        record.metadata.redactions
                    );
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "{}\nstatus={} started={} branch={} task={}",
                        record.metadata.session_id,
                        record.metadata.status.as_str(),
                        record.metadata.started_at_ms,
                        record.metadata.branch.unwrap_or_else(|| "-".to_string()),
                        record.metadata.first_user_task.unwrap_or_default()
                    )));
                }
                Err(error) => app.status = format!("session show failed: {error}"),
            }
            return true;
        }
        "/resume" => {
            let Some(session_id) = parts.next() else {
                app.status = "usage: /resume <session_id>".to_string();
                return true;
            };
            match agent.resume_current(session_id) {
                Ok(transcript) => {
                    app.transcript.clear();
                    app.selected_entry = None;
                    app.next_entry_id = 0;
                    for item in transcript {
                        app.push_transcript_item(item);
                    }
                    app.attachments = agent.context_attachments_snapshot().await;
                    app.pending_assistant.clear();
                    app.task_state = None;
                    app.task_panel_collapsed = false;
                    app.turn_rx = None;
                    app.cancel = None;
                    app.status = format!("resumed session {session_id}");
                }
                Err(error) => app.status = format!("resume failed: {error}"),
            }
            return true;
        }
        "/session-export" => {
            let Some(session_id) = parts.next() else {
                app.status = "usage: /session-export <session_id>".to_string();
                return true;
            };
            match agent.export_session(session_id) {
                Ok(value) => {
                    app.status = format!(
                        "session export {} bytes",
                        serde_json::to_string(&value).map_or(0, |text| text.len())
                    );
                }
                Err(error) => app.status = format!("session export failed: {error}"),
            }
            return true;
        }
        "/session-cleanup" => {
            let ids = parts.map(str::to_string).collect::<Vec<_>>();
            match agent.cleanup_sessions(&ids) {
                Ok(report) => app.status = format!("removed {} sessions", report.removed.len()),
                Err(error) => app.status = format!("session cleanup failed: {error}"),
            }
            return true;
        }
        "/copy" => {
            match parts.next() {
                None => copy_to_clipboard(app, ClipboardTarget::LastAssistant),
                Some("transcript") => copy_to_clipboard(app, ClipboardTarget::Transcript),
                Some(_) => app.status = "usage: /copy [transcript]".to_string(),
            }
            return true;
        }
        _ => {}
    }
    let (name, arguments) = match command {
        "/checkpoints" => ("checkpoint_list", serde_json::json!({})),
        "/undo" => ("checkpoint_undo", serde_json::json!({})),
        "/checkpoint" => {
            let Some(checkpoint_id) = parts.next() else {
                app.status = "usage: /checkpoint <checkpoint_id>".to_string();
                return true;
            };
            (
                "checkpoint_show",
                serde_json::json!({ "checkpoint_id": checkpoint_id }),
            )
        }
        "/revert-turn" => {
            let Some(group_id) = parts.next() else {
                app.status = "usage: /revert-turn <turn_id>".to_string();
                return true;
            };
            (
                "checkpoint_revert",
                serde_json::json!({ "group_id": group_id }),
            )
        }
        _ => return false,
    };
    let job = agent.start_local_tool_job(ToolCall {
        call_id: format!("tui-{name}"),
        name: name.to_string(),
        arguments,
    });
    app.jobs.insert(job.id, job.clone());
    app.status = format!("started job {} {}", job.id, job.title);
    true
}

fn handle_help_command(app: &mut TuiApp, rest: &str) {
    let help = SqueezyHelp::new(app.help_config_inspect.clone());
    let answer = if rest.trim().is_empty() {
        help.topic_index()
    } else {
        help.answer_topic(rest)
    };
    app.status = match answer.status {
        HelpStatus::Answered => format!("help {}", answer.topic),
        HelpStatus::Unsupported => "help topic not covered locally".to_string(),
    };
    app.push_transcript_item(TranscriptItem::system(answer.render_markdown()));
}

async fn handle_feedback_command(app: &mut TuiApp, agent: &Agent, rest: &str) {
    match rest {
        "send" => {
            let Some(feedback) = app.pending_feedback.take() else {
                app.status = "no feedback pending".to_string();
                return;
            };
            match agent.submit_feedback(&feedback).await {
                Ok(result) => {
                    app.status = format!("feedback sent {}", result.id);
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "feedback sent\nfeedback_id={}",
                        result.id
                    )));
                }
                Err(error) => {
                    app.pending_feedback = Some(feedback);
                    app.status = format!("feedback send failed: {error}");
                }
            }
        }
        "cancel" => {
            app.pending_feedback = None;
            app.status = "feedback cancelled".to_string();
        }
        "" => {
            app.status =
                "usage: /feedback <what happened> | /feedback send | /feedback cancel".to_string();
        }
        message => match agent.prepare_feedback(message) {
            Ok(feedback) => {
                let preview = format!(
                    "feedback preview\nfeedback_id={}\nbytes={} redactions={}\n\n{}\n\nRun /feedback send to submit or /feedback cancel.",
                    feedback.feedback_id,
                    feedback.message_bytes,
                    feedback.redactions,
                    feedback.message
                );
                app.pending_feedback = Some(feedback);
                app.status = "feedback preview ready".to_string();
                app.push_transcript_item(TranscriptItem::system(preview));
            }
            Err(error) => app.status = format!("feedback preview failed: {error}"),
        },
    }
}

async fn handle_report_command(app: &mut TuiApp, agent: &Agent, rest: &str) {
    match rest {
        "send" => {
            let Some(report) = app.pending_report.take() else {
                app.status = "no report pending".to_string();
                return;
            };
            match agent.submit_bug_report(&report).await {
                Ok(result) => {
                    app.status = format!("report sent {}", result.id);
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "report sent\nreport_id={}",
                        result.id
                    )));
                }
                Err(error) => {
                    app.pending_report = Some(report);
                    app.status = format!("report send failed: {error}");
                }
            }
        }
        "cancel" => {
            app.pending_report = None;
            app.status = "report cancelled".to_string();
        }
        _ => {
            let (session_id, excluded_sections) = match parse_report_preview_args(agent, rest) {
                Ok(value) => value,
                Err(error) => {
                    app.status = error;
                    return;
                }
            };
            let options = BugReportOptions {
                excluded_sections,
                ..BugReportOptions::default()
            };
            match agent.build_bug_report(&session_id, options) {
                Ok(report) => {
                    let mut preview = report.preview_text();
                    preview.push_str("\nRun /report send to upload or /report cancel.");
                    app.pending_report = Some(report);
                    app.status = "report preview ready".to_string();
                    app.push_transcript_item(TranscriptItem::system(preview));
                }
                Err(error) => app.status = format!("report preview failed: {error}"),
            }
        }
    }
}

fn parse_report_preview_args(
    agent: &Agent,
    rest: &str,
) -> std::result::Result<(String, BTreeSet<String>), String> {
    let mut session_id = None;
    let mut excluded_sections = BTreeSet::new();
    for part in rest.split_whitespace() {
        if let Some(raw) = part.strip_prefix("exclude=") {
            for section in raw.split(',').filter(|section| !section.trim().is_empty()) {
                let Some(parsed) = parse_bug_report_section(section) else {
                    return Err(format!("unknown report section {section:?}"));
                };
                excluded_sections.insert(parsed.to_string());
            }
        } else if session_id.is_none() {
            session_id = Some(part.to_string());
        } else {
            return Err(
                "usage: /report [session_id] [exclude=a,b] | /report send | /report cancel"
                    .to_string(),
            );
        }
    }
    let session_id = session_id
        .or_else(|| agent.session_id())
        .ok_or_else(|| "usage: /report <session_id> [exclude=a,b]".to_string())?;
    Ok((session_id, excluded_sections))
}

fn attachment_update_status(
    source: &str,
    update: &squeezy_agent::ContextAttachmentUpdate,
) -> String {
    let attachment = &update.attachment;
    if update.duplicate {
        return format!("deduped {source} as {}", attachment.id);
    }
    if !update.active {
        return format!(
            "unsupported {source}: {} ({})",
            attachment.kind.as_str(),
            attachment.original_bytes
        );
    }
    format!(
        "attached {source} {} kind={} bytes={} preview={} redactions={}",
        attachment.id,
        attachment.kind.as_str(),
        attachment.original_bytes,
        attachment.preview_bytes,
        attachment.redactions,
    )
}

fn format_attachment_list(attachments: &[ContextAttachment]) -> String {
    if attachments.is_empty() {
        return "No attached context.".to_string();
    }
    attachments
        .iter()
        .map(format_attachment_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_attachment_line(attachment: &ContextAttachment) -> String {
    let preview = attachment.preview.replace('\n', " ");
    let preview = preview.chars().take(80).collect::<String>();
    format!(
        "{} {} {} {}B {}",
        attachment.id,
        attachment.source.as_str(),
        attachment.kind.as_str(),
        attachment.original_bytes,
        preview
    )
}

fn format_cost_command(snapshot: &SessionAccountingSnapshot) -> String {
    let cost = &snapshot.cost;
    let metrics = &snapshot.metrics;
    format!(
        "Cost accounting\n\
session={}\n\
provider={} model={} mode={}\n\
estimated_usd={} (estimated from provider-reported usage and local pricing metadata)\n\
provider_tokens input={} output={} reasoning={} cached_input={} cache_write_input={}\n\
tools calls={} successes={} errors={} denials={} cancellations={} budget_denials={}\n\
receipts stub_hits={} negative_stub_hits={} total_hits={}\n\
spills writes={} reads={}\n\
io bytes_read={} files_scanned={} matches_returned={} model_output_bytes={}\n\
redactions={}\n\
accuracy=provider token counters are provider-reported when available; USD is an estimate, not a billing authority.",
        snapshot.session_id.as_deref().unwrap_or("-"),
        snapshot.provider,
        snapshot.model,
        snapshot.mode.as_str(),
        format_cost(cost),
        format_optional_u64(cost.input_tokens),
        format_optional_u64(cost.output_tokens),
        format_optional_u64(cost.reasoning_output_tokens),
        format_optional_u64(cost.cached_input_tokens),
        format_optional_u64(cost.cache_write_input_tokens),
        metrics.tool_calls,
        metrics.tool_successes,
        metrics.tool_errors,
        metrics.tool_denials,
        metrics.tool_cancellations,
        metrics.budget_denials,
        metrics.receipt_stub_hits,
        metrics.negative_receipt_hits,
        metrics.receipt_stub_hits + metrics.negative_receipt_hits,
        metrics.spill_writes,
        metrics.spill_reads,
        metrics.bytes_read,
        metrics.files_scanned,
        metrics.matches_returned,
        metrics.model_output_bytes,
        snapshot.redactions,
    )
}

fn format_context_command(snapshot: &SessionAccountingSnapshot) -> String {
    let response_state = if snapshot.store_responses {
        if snapshot.previous_response_id.is_some() {
            "store_responses=true previous_response_id=present"
        } else {
            "store_responses=true previous_response_id=absent"
        }
    } else {
        "store_responses=false"
    };
    let provider_gap = if snapshot.provider_stored_context_active() {
        "provider_stored_context=active; exact provider-side current-window use is unknown, so compare transmitted request with the local full-history estimate"
    } else {
        "provider_stored_context=inactive"
    };
    format!(
        "Context accounting\n\
session={}\n\
provider={} model={} mode={}\n\
response_state={}\n\
{}\n\
completed_turns={} provider_tokens input={} output={} reasoning={} cached_input={} cache_write_input={}\n\
transcript items={} user={} assistant={} system={} bytes={}\n\
local_history items={} user_text={} assistant_text={} function_calls={} function_outputs={} text_bytes={} tool_output_bytes={}\n\
attached_context total={} active={} removed={} unsupported={} stored_bytes={} redactions={}\n\
tool_volume calls={} results={} receipt_hits={} spill_writes={} spill_reads={} budget_denials={}\n\
{}\n\
{}\n\
accuracy=context tokens are deterministic local estimates of assembled request content; percentages and remaining input budget are shown only when a model context limit is known.",
        snapshot.session_id.as_deref().unwrap_or("-"),
        snapshot.provider,
        snapshot.model,
        snapshot.mode.as_str(),
        response_state,
        provider_gap,
        snapshot.metrics.turns,
        format_optional_u64(snapshot.cost.input_tokens),
        format_optional_u64(snapshot.cost.output_tokens),
        format_optional_u64(snapshot.cost.reasoning_output_tokens),
        format_optional_u64(snapshot.cost.cached_input_tokens),
        format_optional_u64(snapshot.cost.cache_write_input_tokens),
        snapshot.transcript.items,
        snapshot.transcript.user,
        snapshot.transcript.assistant,
        snapshot.transcript.system,
        snapshot.transcript.bytes,
        snapshot.conversation.items,
        snapshot.conversation.user_text,
        snapshot.conversation.assistant_text,
        snapshot.conversation.function_calls,
        snapshot.conversation.function_outputs,
        snapshot.conversation.text_bytes,
        snapshot.conversation.tool_output_bytes,
        snapshot.attachments.total,
        snapshot.attachments.active,
        snapshot.attachments.removed,
        snapshot.attachments.unsupported,
        snapshot.attachments.stored_bytes,
        snapshot.attachments.redactions,
        snapshot.metrics.tool_calls,
        snapshot.metrics.tool_successes
            + snapshot.metrics.tool_errors
            + snapshot.metrics.tool_denials
            + snapshot.metrics.tool_cancellations,
        snapshot.metrics.receipt_stub_hits + snapshot.metrics.negative_receipt_hits,
        snapshot.metrics.spill_writes,
        snapshot.metrics.spill_reads,
        snapshot.metrics.budget_denials,
        format_request_estimate("transmitted_request", &snapshot.transmitted_request),
        format_request_estimate("local_full_history", &snapshot.full_history_request),
    )
}

fn format_request_estimate(label: &str, estimate: &RequestTokenEstimate) -> String {
    let mut output = format!(
        "{} input_tokens={} tokenizer={} accuracy={}",
        label,
        estimate.input_tokens,
        estimate.tokenizer.as_str(),
        if estimate.estimated {
            "estimated"
        } else {
            "exact"
        }
    );
    if let Some(context_window) = estimate.context_window_tokens {
        output.push_str(&format!(" context_window={context_window}"));
    } else {
        output.push_str(" context_window=unknown");
    }
    if let Some(max_output) = estimate.max_output_tokens {
        output.push_str(&format!(" max_output_reserve={max_output}"));
    } else {
        output.push_str(" max_output_reserve=unknown");
    }
    if let Some(input_budget) = estimate.input_budget_tokens {
        output.push_str(&format!(" input_budget={input_budget}"));
    } else {
        output.push_str(" input_budget=unknown");
    }
    if let Some(remaining) = estimate.remaining_input_tokens {
        output.push_str(&format!(" remaining_input_budget={remaining}"));
    } else {
        output.push_str(" remaining_input_budget=unknown");
    }
    if let Some(percent) = estimate.used_input_percent_x100 {
        output.push_str(&format!(" used={}", format_percent_x100(percent)));
    } else {
        output.push_str(" used=unknown");
    }
    output
}

fn format_percent_x100(value: u32) -> String {
    format!("{}.{:02}%", value / 100, value % 100)
}

fn format_pin_list(context: &ContextCompactionState) -> String {
    if context.pinned.is_empty() {
        return "No pinned context.".to_string();
    }
    context
        .pinned
        .iter()
        .map(|pin| {
            format!(
                "{} {} {}",
                pin.id,
                pin.label,
                compact_text(&pin.summary, 120)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compaction_status_line(record: &ContextCompactionRecord) -> String {
    format!(
        "compacted context {}->{} tok",
        record.before.estimated_tokens, record.after.estimated_tokens
    )
}

enum PinSourceResult {
    Found(String, String, String),
    NoEntry,
    UnknownTarget,
}

fn pin_source(app: &TuiApp, target: &str) -> PinSourceResult {
    let entry = match target {
        "selected" => app
            .selected_entry
            .and_then(|index| app.transcript.get(index))
            .or_else(|| app.transcript.last()),
        "last" => app.transcript.last(),
        _ => return PinSourceResult::UnknownTarget,
    };
    match entry {
        Some(entry) => {
            let (label, summary, source) = entry.pin_payload();
            PinSourceResult::Found(label, summary, source)
        }
        None => PinSourceResult::NoEntry,
    }
}

fn sync_jobs_from_agent(app: &mut TuiApp, agent: &Agent) {
    app.jobs = agent
        .jobs_snapshot()
        .into_iter()
        .map(|job| (job.id, job))
        .collect();
    app.notifications = agent.job_notifications().into_iter().collect();
}

fn parse_job_id(raw: &str) -> Option<JobId> {
    raw.parse().ok()
}

fn format_jobs_list(app: &TuiApp) -> String {
    if app.jobs.is_empty() {
        return "no jobs".to_string();
    }
    app.jobs
        .values()
        .rev()
        .take(MAX_JOB_NOTIFICATIONS)
        .map(|job| {
            format!(
                "{} {} {} {}",
                job.id,
                job.status.as_str(),
                job.kind.as_str(),
                sanitize_inline(&job.title)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_job_detail(job: &JobSnapshot) -> String {
    let progress = job
        .progress
        .as_ref()
        .map(|progress| progress.message.as_str())
        .unwrap_or("-");
    let summary = job.result_summary.as_deref().unwrap_or("-");
    let handle = job.output_handle.as_deref().unwrap_or("-");
    let tool_name = job.tool_name.as_deref().unwrap_or("-");
    let call_id = job.call_id.as_deref().unwrap_or("-");
    format!(
        "job {id}\nstatus={status}\nkind={kind}\ntool={tool}\ncall_id={call_id}\ntitle={title}\nprogress={progress}\nsummary={summary}\noutput_handle={handle}",
        id = job.id,
        status = job.status.as_str(),
        kind = job.kind.as_str(),
        tool = tool_name,
        call_id = call_id,
        title = sanitize_inline(&job.title),
        progress = sanitize_inline(progress),
        summary = sanitize_inline(summary),
        handle = sanitize_inline(handle),
    )
}

fn sanitize_inline(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

fn copy_to_clipboard(app: &mut TuiApp, target: ClipboardTarget) {
    let Some(text) = clipboard_text(app, target) else {
        app.status = match target {
            ClipboardTarget::LastAssistant => "nothing to copy yet".to_string(),
            ClipboardTarget::Transcript => "transcript is empty".to_string(),
        };
        return;
    };
    match app.clipboard.copy_text(&text) {
        Ok(()) => {
            app.status = match target {
                ClipboardTarget::LastAssistant => {
                    format!("copied assistant message ({} chars)", text.chars().count())
                }
                ClipboardTarget::Transcript => {
                    format!("copied transcript ({} chars)", text.chars().count())
                }
            };
        }
        Err(error) => {
            app.status = format!("copy failed: {error}");
        }
    }
}

fn clipboard_text(app: &TuiApp, target: ClipboardTarget) -> Option<String> {
    match target {
        ClipboardTarget::LastAssistant => {
            if !app.pending_assistant.trim().is_empty() {
                return Some(app.pending_assistant.clone());
            }
            app.transcript
                .iter()
                .rev()
                .find_map(TranscriptEntry::assistant_content)
        }
        ClipboardTarget::Transcript => {
            let text = transcript_plain_text(app);
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        }
    }
}

fn transcript_plain_text(app: &TuiApp) -> String {
    let mut lines = Vec::new();
    for item in &app.transcript {
        lines.extend(item.plain_text_lines());
    }
    if !app.pending_assistant.is_empty() {
        lines.push(format!("assistant: {}", app.pending_assistant));
    }
    lines.join("\n")
}

fn mode_command(input: &str) -> Option<SessionMode> {
    match input {
        "/plan" => Some(SessionMode::Plan),
        "/build" => Some(SessionMode::Build),
        _ => None,
    }
}

fn parse_transcript_category(value: &str) -> Option<TranscriptCategory> {
    match value {
        "all" => Some(TranscriptCategory::All),
        "tools" => Some(TranscriptCategory::Tools),
        "logs" => Some(TranscriptCategory::Logs),
        "diffs" => Some(TranscriptCategory::Diffs),
        "receipts" => Some(TranscriptCategory::Receipts),
        "assistant" => Some(TranscriptCategory::Assistant),
        _ => None,
    }
}

fn parse_response_verbosity(value: &str) -> Option<ResponseVerbosity> {
    match value {
        "concise" => Some(ResponseVerbosity::Concise),
        "normal" => Some(ResponseVerbosity::Normal),
        "verbose" => Some(ResponseVerbosity::Verbose),
        _ => None,
    }
}

fn parse_tool_output_verbosity(value: &str) -> Option<ToolOutputVerbosity> {
    match value {
        "compact" => Some(ToolOutputVerbosity::Compact),
        "normal" => Some(ToolOutputVerbosity::Normal),
        "verbose" => Some(ToolOutputVerbosity::Verbose),
        _ => None,
    }
}

fn set_transcript_collapsed(
    app: &mut TuiApp,
    category: TranscriptCategory,
    collapsed: bool,
) -> usize {
    let mut changed = 0;
    for entry in &mut app.transcript {
        if entry.matches_category(category) && entry.collapsed != collapsed {
            entry.collapsed = collapsed;
            changed += 1;
        }
    }
    changed
}

fn select_previous_transcript_entry(app: &mut TuiApp) {
    if app.transcript.is_empty() {
        app.status = "transcript is empty".to_string();
        return;
    }
    app.selected_entry = Some(match app.selected_entry {
        Some(index) => index.saturating_sub(1),
        None => app.transcript.len() - 1,
    });
    let entry = &app.transcript[app.selected_entry.unwrap()];
    app.status = format!("selected transcript entry {}", entry.id + 1);
}

fn select_next_transcript_entry(app: &mut TuiApp) {
    if app.transcript.is_empty() {
        app.status = "transcript is empty".to_string();
        return;
    }
    app.selected_entry = Some(match app.selected_entry {
        Some(index) => (index + 1).min(app.transcript.len() - 1),
        None => 0,
    });
    let entry = &app.transcript[app.selected_entry.unwrap()];
    app.status = format!("selected transcript entry {}", entry.id + 1);
}

fn toggle_selected_transcript_entry(app: &mut TuiApp) {
    let Some(index) = app.selected_entry else {
        app.status = "select a transcript entry first".to_string();
        return;
    };
    let Some(entry) = app.transcript.get_mut(index) else {
        app.selected_entry = None;
        app.status = "select a transcript entry first".to_string();
        return;
    };
    entry.collapsed = !entry.collapsed;
    app.status = format!(
        "{} transcript entry {}",
        if entry.collapsed {
            "collapsed"
        } else {
            "expanded"
        },
        entry.id + 1
    );
}

fn switch_mode(
    app: &mut TuiApp,
    agent: &Agent,
    requested: Option<SessionMode>,
    source: &'static str,
) {
    if app.turn_rx.is_some() || app.pending_approval.is_some() {
        app.status = "mode switch unavailable during active turn".to_string();
        return;
    }

    let target = requested.unwrap_or(match app.mode {
        SessionMode::Plan => SessionMode::Build,
        SessionMode::Build => SessionMode::Plan,
    });
    if target == app.mode {
        app.status = format!("already in {} mode", app.mode.as_str());
        return;
    }
    if agent.set_session_mode(target, source) {
        app.mode = target;
        app.status = format!("mode switched to {}", app.mode.as_str());
    } else {
        // Agent saw no change (lock-free path is infallible, so this only
        // fires when the agent observed the same mode we requested). Resync
        // the visible status with the underlying truth so the user sees the
        // authoritative state.
        app.mode = agent.session_mode();
        app.status = format!("already in {} mode", app.mode.as_str());
    }
}

fn handle_approval_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    let Some(pending) = app.pending_approval.take() else {
        return false;
    };

    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let _ = pending.decision_tx.send(ToolApprovalDecision::AllowOnce);
            app.status = format!("approved {}", pending.request.tool_name);
            true
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            let _ = pending
                .decision_tx
                .send(ToolApprovalDecision::AllowRuleUser);
            app.status = format!("approved user rule for {}", pending.request.tool_name);
            true
        }
        KeyCode::Char('p') | KeyCode::Char('P') => {
            let _ = pending
                .decision_tx
                .send(ToolApprovalDecision::AllowRuleProject);
            app.status = format!("approved project rule for {}", pending.request.tool_name);
            true
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            let _ = pending.decision_tx.send(ToolApprovalDecision::DenyOnce);
            app.status = format!("denied {}", pending.request.tool_name);
            true
        }
        KeyCode::Char('d') | KeyCode::Char('D') => {
            let _ = pending
                .decision_tx
                .send(ToolApprovalDecision::DenyRuleProject);
            app.status = format!("denied project rule for {}", pending.request.tool_name);
            true
        }
        KeyCode::Char('u') | KeyCode::Char('U') => {
            let _ = pending.decision_tx.send(ToolApprovalDecision::DenyRuleUser);
            app.status = format!("denied user rule for {}", pending.request.tool_name);
            true
        }
        _ => {
            app.status = format_approval_status_line(&pending.request);
            app.pending_approval = Some(pending);
            true
        }
    }
}

/// Keys we surface in the approval prompt, in display order. The list
/// matches the metadata emitted by `ToolRegistry::permission_request` so a
/// future field becomes visible by adding it here AND in the tool
/// registry; the doc in `docs/external/CONFIGURATION.md` references this contract.
pub(crate) const APPROVAL_PROMPT_KEYS: &[&str] = &[
    "server",
    "tool",
    "transport",
    "target",
    "arguments",
    "command",
    "cwd",
    "description",
    "env",
    "network",
    "destructive",
    "timeout_ms",
    "output_byte_cap",
    "sandbox",
    "sandbox_network",
    "sandbox_read_roots",
    "sandbox_write_roots",
    "parser_backed",
    "dynamic",
];

/// Single-line status banner shown in the 1-line status bar. Compact by
/// design so the status bar remains useful for non-approval traffic.
pub(crate) fn format_approval_status_line(request: &ToolApprovalRequest) -> String {
    let permission = &request.permission;
    format!(
        "approval pending: {tool} risk={risk} target={target} | y once | a user allow | p project allow | u user deny | d project deny | n deny once",
        tool = request.tool_name,
        risk = permission.risk.as_str(),
        target = permission.target,
    )
}

/// Multi-line approval prompt rendered on its own dedicated TUI panel.
/// Each metadata field gets its own line so long commands wrap cleanly
/// instead of being truncated off the right edge of the screen.
pub(crate) fn format_approval_prompt(request: &ToolApprovalRequest) -> String {
    let permission = &request.permission;
    let mut lines = Vec::new();
    lines.push(format!("approve {}", permission.summary.trim()));
    lines.push(format!(
        "  risk={risk} target={target}",
        risk = permission.risk.as_str(),
        target = permission.target,
    ));
    if !request.reason.is_empty() {
        lines.push(format!("  reason={}", request.reason));
    }
    for key in APPROVAL_PROMPT_KEYS {
        if let Some(value) = permission.metadata.get(*key) {
            lines.push(format!("  {key}={value:?}"));
        }
    }
    lines.push(
        "  [y] once  [a] user allow  [p] project allow  [u] user deny  [d] project deny  [n] deny once"
            .to_string(),
    );
    lines.join("\n")
}

fn render(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    let attachment_height = attachment_panel_height(app);
    let approval_prompt = app.pending_approval.as_ref().map(|pending| {
        // When an approval is pending, reserve a dedicated panel large
        // enough to show every metadata line of `format_approval_prompt`.
        let prompt = format_approval_prompt(&pending.request);
        let line_count = prompt.matches('\n').count() as u16 + 1;
        let approval_height = line_count.saturating_add(2).clamp(6, 18);
        (prompt, approval_height)
    });
    let task_height = if should_show_task_panel(app) {
        let h = if approval_prompt.is_some() {
            task_panel_height(app).min(5)
        } else {
            task_panel_height(app)
        };
        Some(h)
    } else {
        None
    };
    let transcript_min = if approval_prompt.is_some() { 3 } else { 5 };
    let mut constraints = vec![Constraint::Min(transcript_min)];
    if let Some(h) = task_height {
        constraints.push(Constraint::Length(h));
    }
    if let Some((_, approval_height)) = &approval_prompt {
        constraints.push(Constraint::Length(*approval_height));
    }
    if attachment_height > 0 {
        constraints.push(Constraint::Length(attachment_height));
    }
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(2));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut index = 0;
    render_transcript(frame, chunks[index], app);
    index += 1;
    if task_height.is_some() {
        render_task_state(frame, chunks[index], app);
        index += 1;
    }
    if let Some((prompt, _)) = approval_prompt.as_ref() {
        render_approval(frame, chunks[index], prompt);
        index += 1;
    }
    if attachment_height > 0 {
        render_attachments(frame, chunks[index], app);
        index += 1;
    }
    render_input(frame, chunks[index], app);
    index += 1;
    render_status(frame, chunks[index], app);
}

fn should_show_task_panel(app: &TuiApp) -> bool {
    app.task_state.is_some()
}

fn task_panel_height(app: &TuiApp) -> u16 {
    if app.task_panel_collapsed {
        return 1;
    }
    let line_count = app
        .task_state
        .as_ref()
        .map(|snapshot| format_task_state_lines(snapshot, false).len() as u16)
        .unwrap_or(1);
    line_count.clamp(2, 7)
}

fn render_task_state(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let Some(snapshot) = app.task_state.as_ref() else {
        return;
    };
    let lines = format_task_state_lines(snapshot, app.task_panel_collapsed);
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(QUIET))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn format_task_state_lines(snapshot: &TaskStateSnapshot, collapsed: bool) -> Vec<Line<'static>> {
    if collapsed {
        return vec![Line::from(format!(
            "◆ {} · active {} · blocker {} · next {} · verify {}",
            task_title(snapshot),
            snapshot.active_step_title().unwrap_or("-"),
            snapshot.blocker.as_deref().unwrap_or("-"),
            snapshot.next_action.as_deref().unwrap_or("-"),
            snapshot.verification.as_str(),
        ))];
    }

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "◆ ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            task_title(snapshot).to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", snapshot.status.as_str()),
            Style::default().fg(QUIET),
        ),
    ]));
    if let Some(summary) = &snapshot.summary {
        lines.push(Line::from(format!("  {summary}")));
    }
    if snapshot.steps.is_empty() {
        lines.push(Line::from("  -"));
    } else {
        for step in snapshot
            .steps
            .iter()
            .filter(|step| step.status == TaskStepStatus::Active)
            .take(1)
        {
            let detail = step
                .detail
                .as_ref()
                .map(|detail| format!(" - {detail}"))
                .unwrap_or_default();
            lines.push(Line::from(format!(
                "  {} {}{}",
                step.status.as_str(),
                step.title,
                detail
            )));
        }
    }
    if let Some(blocker) = &snapshot.blocker {
        lines.push(Line::from(format!("  blocker {blocker}")));
    }
    if !snapshot.recent_changes.is_empty() {
        lines.push(Line::from(format!(
            "  recent {}",
            snapshot.recent_changes.join("; ")
        )));
    }
    let next = snapshot.next_action.as_deref().unwrap_or("-");
    lines.push(Line::from(format!(
        "  next {next} · verify {}",
        snapshot.verification.as_str()
    )));
    if let Some(reason) = &snapshot.replan_reason {
        lines.push(Line::from(format!("  replan {reason}")));
    }
    lines
}

fn task_title(snapshot: &TaskStateSnapshot) -> &str {
    if snapshot.task.is_empty() {
        "current turn"
    } else {
        snapshot.task.as_str()
    }
}

fn render_approval(frame: &mut Frame<'_>, area: Rect, prompt: &str) {
    let paragraph = Paragraph::new(prompt)
        .block(
            Block::default()
                .title("Approval required")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut lines = Vec::new();
    for (index, item) in app.transcript.iter().enumerate() {
        lines.extend(format_transcript_entry(
            item,
            app.selected_entry == Some(index),
            app.tool_output_verbosity,
        ));
    }
    if !app.pending_assistant.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("◐ ", Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
            Span::raw(&app.pending_assistant),
        ]));
    }
    if lines.is_empty() {
        lines.extend(startup_card_lines(app, area.width));
    }

    let scroll =
        transcript_scroll_offset(lines.len(), area.height, app.transcript_scroll_from_bottom);
    let paragraph = Paragraph::new(lines)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn startup_card_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let card_width = width.clamp(36, 64) as usize;
    let inner = card_width.saturating_sub(2);
    let border = "─".repeat(inner);
    vec![
        Line::from(Span::styled(
            format!("╭{border}╮"),
            Style::default().fg(AMBER_DIM),
        )),
        startup_card_row(
            inner,
            "",
            format!(">_ Squeezy v{}", app.version),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        startup_card_row(inner, "model", app.model.clone(), Style::default().fg(GOLD)),
        startup_card_row(
            inner,
            "directory",
            app.directory.clone(),
            Style::default().fg(Color::White),
        ),
        startup_card_row(
            inner,
            "languages",
            app.language_summary.clone(),
            Style::default().fg(Color::White),
        ),
        Line::from(Span::styled(
            format!("╰{border}╯"),
            Style::default().fg(AMBER_DIM),
        )),
    ]
}

fn startup_card_row(
    inner_width: usize,
    label: &'static str,
    value: String,
    value_style: Style,
) -> Line<'static> {
    let label_width = if label.is_empty() { 0 } else { 11 };
    let value_width = inner_width.saturating_sub(label_width + 2);
    let value = fit_chars(&value, value_width);
    let used = 1 + label_width + value.chars().count();
    let padding = " ".repeat(inner_width.saturating_sub(used));
    if label.is_empty() {
        return Line::from(vec![
            Span::styled("│ ", Style::default().fg(AMBER_DIM)),
            Span::styled(value, value_style),
            Span::raw(padding),
            Span::styled("│", Style::default().fg(AMBER_DIM)),
        ]);
    }
    Line::from(vec![
        Span::styled("│ ", Style::default().fg(AMBER_DIM)),
        Span::styled(
            format!("{label}:"),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(label_width.saturating_sub(label.len() + 1))),
        Span::styled(value, value_style),
        Span::raw(padding),
        Span::styled("│", Style::default().fg(AMBER_DIM)),
    ])
}

fn attachment_panel_height(app: &TuiApp) -> u16 {
    if app.attachments.is_empty() {
        0
    } else {
        (app.attachments.len() as u16).clamp(1, 4)
    }
}

fn render_attachments(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let lines = app
        .attachments
        .iter()
        .map(|attachment| Line::from(format_attachment_line(attachment)))
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(QUIET))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn transcript_scroll_offset(line_count: usize, area_height: u16, from_bottom: u16) -> u16 {
    let visible_lines = area_height as usize;
    let max_scroll = line_count.saturating_sub(visible_lines);
    max_scroll.saturating_sub(from_bottom as usize) as u16
}

#[cfg(test)]
fn format_transcript_item(item: &TranscriptItem) -> Line<'_> {
    let (label, color) = match &item.role {
        Role::User => ("> ", AMBER),
        Role::Assistant => ("● ", Color::Green),
        Role::System => ("• ", GOLD),
    };
    Line::from(vec![
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(item.content.as_str()),
    ])
}

fn format_transcript_entry(
    entry: &TranscriptEntry,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    match &entry.kind {
        TranscriptEntryKind::Message(item) => format_message_entry(item, entry.collapsed, selected),
        TranscriptEntryKind::ToolCall(call) => {
            format_tool_call_entry(call, entry.collapsed, selected)
        }
        TranscriptEntryKind::ToolResult(result) => {
            format_tool_result_entry(result, entry.collapsed, selected, tool_output_verbosity)
        }
        TranscriptEntryKind::Log(message) => format_log_entry(message, entry.collapsed, selected),
    }
}

fn format_message_entry(
    item: &TranscriptItem,
    collapsed: bool,
    selected: bool,
) -> Vec<Line<'static>> {
    let (label, color) = role_style(&item.role);
    if collapsed {
        return vec![line_with_label(
            selected,
            label,
            color,
            format!(
                "… {} chars  {}",
                item.content.chars().count(),
                compact_text(&item.content, 140)
            ),
        )];
    }
    text_lines(selected, label, color, &item.content)
}

fn format_tool_call_entry(call: &ToolCall, collapsed: bool, selected: bool) -> Vec<Line<'static>> {
    let summary = format!(
        "{} queued args={}",
        call.name,
        compact_text(&call.arguments.to_string(), 140)
    );
    if collapsed {
        return vec![line_with_label(selected, "◌ ", AMBER, summary)];
    }
    let mut lines = vec![line_with_label(
        selected,
        "◌ ",
        AMBER,
        format!("{} queued", call.name),
    )];
    lines.extend(indented_text_lines(
        serde_json::to_string_pretty(&call.arguments)
            .unwrap_or_else(|_| call.arguments.to_string())
            .as_str(),
    ));
    lines
}

fn format_tool_result_entry(
    result: &ToolResult,
    collapsed: bool,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let summary = tool_result_summary(result);
    if collapsed {
        return vec![line_with_label(
            selected,
            "● ",
            status_color(result.status),
            summary,
        )];
    }
    let mut lines = vec![line_with_label(
        selected,
        "● ",
        status_color(result.status),
        summary,
    )];
    lines.push(Line::from(format!(
        "  receipt output={} content={}",
        short_hash(&result.receipt.output_sha256),
        result
            .receipt
            .content_sha256
            .as_deref()
            .map(short_hash)
            .unwrap_or("-")
    )));
    let preview = preview_tool_result(result, tool_output_verbosity);
    lines.extend(indented_text_lines(&preview));
    lines
}

fn format_log_entry(message: &str, collapsed: bool, selected: bool) -> Vec<Line<'static>> {
    if collapsed {
        let preview = compact_text(message, 140);
        return vec![line_with_label(
            selected,
            "! ",
            GOLD,
            format!("… {} chars  {}", message.chars().count(), preview),
        )];
    }
    text_lines(selected, "! ", GOLD, message)
}

fn role_style(role: &Role) -> (&'static str, Color) {
    match role {
        Role::User => ("> ", AMBER),
        Role::Assistant => ("● ", Color::Green),
        Role::System => ("• ", GOLD),
    }
}

fn line_with_label(
    selected: bool,
    label: &'static str,
    color: Color,
    content: impl Into<String>,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    Line::from(vec![
        Span::raw(marker),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(content.into()),
    ])
}

fn text_lines(
    selected: bool,
    label: &'static str,
    color: Color,
    content: &str,
) -> Vec<Line<'static>> {
    if content.is_empty() {
        return vec![line_with_label(selected, label, color, "")];
    }
    content
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                line_with_label(selected, label, color, line.to_string())
            } else {
                Line::from(format!("  {line}"))
            }
        })
        .collect()
}

fn indented_text_lines(content: &str) -> Vec<Line<'static>> {
    content
        .lines()
        .map(|line| Line::from(format!("  {line}")))
        .collect()
}

fn tool_result_summary(result: &ToolResult) -> String {
    format!(
        "{} {} {}B{} receipt={}",
        result.tool_name,
        tool_status_label(result.status),
        result.cost_hint.output_bytes,
        if result.cost_hint.truncated {
            " truncated"
        } else {
            ""
        },
        short_hash(&result.receipt.output_sha256),
    )
}

fn preview_tool_result(result: &ToolResult, verbosity: ToolOutputVerbosity) -> String {
    let limit = match verbosity {
        ToolOutputVerbosity::Compact => TOOL_PREVIEW_COMPACT_BYTES,
        ToolOutputVerbosity::Normal => TOOL_PREVIEW_NORMAL_BYTES,
        ToolOutputVerbosity::Verbose => TOOL_PREVIEW_VERBOSE_BYTES,
    };
    let text = serde_json::to_string_pretty(&result.content)
        .unwrap_or_else(|_| result.content.to_string());
    truncate_bytes(&text, limit)
}

fn status_color(status: ToolStatus) -> Color {
    match status {
        ToolStatus::Success => Color::Green,
        ToolStatus::Error | ToolStatus::Stale => Color::Red,
        ToolStatus::Denied | ToolStatus::Cancelled => GOLD,
    }
}

fn tool_status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Success => "success",
        ToolStatus::Error => "error",
        ToolStatus::Denied => "denied",
        ToolStatus::Stale => "stale",
        ToolStatus::Cancelled => "cancelled",
    }
}

fn compact_text(text: &str, limit: usize) -> String {
    truncate_bytes(&text.replace('\n', " "), limit)
}

fn fit_chars(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut output = text.chars().take(width - 3).collect::<String>();
    output.push_str("...");
    output
}

fn truncate_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn render_input(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph = Paragraph::new(Line::from(vec![
        Span::styled(
            "◆ ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::raw(app.input.as_str()),
    ]));
    frame.render_widget(paragraph, area);
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph = Paragraph::new(format_status_lines(app)).style(Style::default().fg(QUIET));
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
fn format_status_tokens(app: &TuiApp) -> String {
    let mut lines = vec![
        format_status_context(app),
        format_status_hints(app).to_string(),
    ];
    if app.status_verbosity == StatusVerbosity::Verbose {
        lines.push(format_status_details(app));
    }
    lines.join("\n")
}

fn format_status_lines(app: &TuiApp) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(app.repo.branch_display(), Style::default().fg(AMBER)),
            Span::styled("  ", Style::default().fg(QUIET)),
            Span::styled(
                format!("{}:{}", app.provider_name, app.model),
                Style::default().fg(GOLD),
            ),
            Span::styled("  ", Style::default().fg(QUIET)),
            Span::styled(app.mode.as_str().to_string(), Style::default().fg(AMBER)),
            Span::styled("  ", Style::default().fg(QUIET)),
            Span::styled(app.status.clone(), Style::default().fg(Color::White)),
            Span::styled(format_status_activity(app), Style::default().fg(QUIET)),
        ]),
        Line::from(Span::styled(
            format_status_hints(app),
            Style::default().fg(QUIET),
        )),
    ];
    if app.status_verbosity == StatusVerbosity::Verbose {
        lines[1] = Line::from(Span::styled(
            format!(
                "{} · {}",
                format_status_details(app),
                format_status_hints(app)
            ),
            Style::default().fg(QUIET),
        ));
    }
    lines
}

#[cfg(test)]
fn format_status_context(app: &TuiApp) -> String {
    format!(
        "{}  {}:{}  {}  {}{}",
        app.repo.branch_display(),
        app.provider_name,
        app.model,
        app.mode.as_str(),
        app.status,
        format_status_activity(app),
    )
}

fn format_status_activity(app: &TuiApp) -> String {
    let scroll_marker = if app.transcript_scroll_from_bottom > 0 {
        "  history"
    } else {
        ""
    };
    let active_jobs = app
        .jobs
        .values()
        .filter(|job| job.status.is_active())
        .count();
    let latest_notification = app
        .notifications
        .back()
        .map(|notification| {
            format!(
                "  job{} {}",
                notification.job_id,
                notification.status.as_str()
            )
        })
        .unwrap_or_default();
    let jobs = if app.jobs.is_empty() {
        String::new()
    } else {
        format!("  jobs {active_jobs}/{}", app.jobs.len())
    };
    format!("{jobs}{latest_notification}{scroll_marker}")
}

fn format_status_details(app: &TuiApp) -> String {
    format!(
        "{}  sandbox {}  telemetry {}  cost {}  tok {}/{}{}  ctx {}  pins {}  compact {}  tools {}  budget {}  cfg {}  read {}B  receipts {}  redactions {}  cached {}  cache_write {}",
        app.permissions.compact(),
        app.permissions.sandbox,
        app.telemetry.as_str(),
        format_cost(&app.cost),
        format_optional_u64(app.cost.input_tokens),
        format_optional_u64(app.cost.output_tokens),
        reasoning_status_fragment(app),
        app.context_estimate.estimated_tokens,
        app.context_compaction.pinned.len(),
        app.context_compaction.generation,
        app.metrics.tool_calls,
        if app.metrics.budget_denials == 0 {
            "ok".to_string()
        } else {
            format!("denied:{}", app.metrics.budget_denials)
        },
        app.config_sources,
        app.metrics.bytes_read,
        app.metrics.receipt_stub_hits + app.metrics.negative_receipt_hits,
        app.metrics.redactions,
        format_optional_u64(app.cost.cached_input_tokens),
        format_optional_u64(app.cost.cache_write_input_tokens),
    )
}

fn format_status_hints(app: &TuiApp) -> &'static str {
    if app.pending_approval.is_some() {
        "Y allow once | A user | P project | N deny | U/D deny rule | Ctrl-C/Esc cancel | Ctrl-P task"
    } else if app.cancel.is_some() {
        "Enter send · Shift-Tab mode · Ctrl-P task · Ctrl-Y copy · /help · Ctrl-C cancel"
    } else {
        "Enter send · Shift-Tab mode · Ctrl-P task · Ctrl-E fold · /help · Esc quit"
    }
}

fn reasoning_status_fragment(app: &TuiApp) -> String {
    if !app.show_reasoning_usage {
        return String::new();
    }
    app.cost
        .reasoning_output_tokens
        .map(|tokens| format!(" reasoning={tokens}"))
        .unwrap_or_default()
}

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or("-".to_string(), |value| value.to_string())
}

fn format_cost(cost: &squeezy_core::CostSnapshot) -> String {
    cost.estimated_usd_micros.map_or("-".to_string(), |value| {
        format!("${:.6}", value as f64 / 1_000_000.0)
    })
}

fn format_error_status(error: &SqueezyError) -> String {
    match error {
        SqueezyError::ProviderNotConfigured(_) => {
            format!("{error}; configure provider credentials or pick another provider")
        }
        SqueezyError::ProviderRequest(_) | SqueezyError::ProviderStream(_) => {
            format!("{error}; retry or check provider/network status")
        }
        SqueezyError::Permission(_) => {
            format!("{error}; approve, adjust policy, or change request")
        }
        SqueezyError::Config(_) => format!("{error}; run squeezy config inspect"),
        _ => format!("{error}"),
    }
}

fn role_label(role: &Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

#[derive(Debug, Clone, Copy)]
enum ClipboardTarget {
    LastAssistant,
    Transcript,
}

trait Clipboard {
    fn copy_text(&mut self, text: &str) -> std::result::Result<(), String>;
}

struct Osc52Clipboard;

/// Conservative cap on OSC52 clipboard payloads. xterm's default
/// `selectToClipboard` buffer is 8 KiB; many other emulators silently
/// drop sequences past their (usually undocumented) limit. We refuse
/// oversized copies up-front so the status line reports an actionable
/// error instead of claiming "copied N chars" while the terminal
/// quietly discarded the escape.
pub(crate) const OSC52_MAX_PAYLOAD_BYTES: usize = 8 * 1024;

impl Clipboard for Osc52Clipboard {
    fn copy_text(&mut self, text: &str) -> std::result::Result<(), String> {
        if text.len() > OSC52_MAX_PAYLOAD_BYTES {
            return Err(format!(
                "payload {} bytes exceeds terminal clipboard cap of {} bytes",
                text.len(),
                OSC52_MAX_PAYLOAD_BYTES,
            ));
        }
        let sequence = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
        let mut stdout = io::stdout();
        stdout
            .write_all(sequence.as_bytes())
            .and_then(|()| stdout.flush())
            .map_err(|err| format!("terminal clipboard write failed: {err}"))
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        output.push(TABLE[(b0 >> 2) as usize] as char);
        output.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoStatus {
    branch: Option<String>,
    changed_files: usize,
    operation: Option<String>,
    available: bool,
}

impl RepoStatus {
    fn detect(config: &AppConfig) -> Self {
        let Ok(vcs) = GitVcs::open(&config.workspace_root) else {
            return Self::none();
        };
        let snapshot = vcs.snapshot(DiffMode::Worktree, DiffOptions::default());
        if snapshot.vcs.kind != VcsKind::Git {
            return Self::none();
        }
        Self {
            branch: snapshot
                .vcs
                .branch
                .or_else(|| snapshot.vcs.head.map(|head| short_commit(&head))),
            changed_files: snapshot.summary.files_changed,
            operation: snapshot.vcs.operation_state,
            available: true,
        }
    }

    fn none() -> Self {
        Self {
            branch: None,
            changed_files: 0,
            operation: None,
            available: false,
        }
    }

    #[cfg(test)]
    fn compact(&self) -> String {
        if !self.available {
            return "repo=none".to_string();
        }
        let mut value = format!("repo={}", self.branch.as_deref().unwrap_or("detached"));
        if self.changed_files > 0 {
            value.push_str(&format!("*{}", self.changed_files));
        }
        if let Some(operation) = &self.operation {
            value.push_str(&format!(":{operation}"));
        }
        value
    }

    fn branch_display(&self) -> String {
        if !self.available {
            return "no repo".to_string();
        }
        let mut value = self.branch.as_deref().unwrap_or("detached").to_string();
        if self.changed_files > 0 {
            value.push_str(&format!("*{}", self.changed_files));
        }
        if let Some(operation) = &self.operation {
            value.push_str(&format!(":{operation}"));
        }
        value
    }
}

fn short_commit(head: &str) -> String {
    head.chars().take(7).collect()
}

fn compact_path(path: &std::path::Path) -> String {
    let display = path.display().to_string();
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return display;
    };
    if let Ok(stripped) = path.strip_prefix(&home) {
        if stripped.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", stripped.display())
        }
    } else {
        display
    }
}

fn configured_language_summary(config: &AppConfig) -> String {
    if config.graph.languages.is_empty() {
        "none".to_string()
    } else {
        config.graph.languages.join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionStatus {
    read: String,
    edit: String,
    shell: String,
    web: String,
    sandbox: String,
}

impl PermissionStatus {
    fn from_policy(policy: &PermissionPolicy) -> Self {
        Self {
            read: policy.read.as_str().to_string(),
            edit: policy.edit.as_str().to_string(),
            shell: policy.shell.as_str().to_string(),
            web: policy.web.as_str().to_string(),
            sandbox: format!(
                "{}/net={}",
                policy.shell_sandbox.mode.as_str(),
                policy.shell_sandbox.network.as_str()
            ),
        }
    }

    fn compact(&self) -> String {
        format!(
            "perm=r:{} e:{} sh:{} web:{}",
            self.read, self.edit, self.shell, self.web
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TelemetryStatus {
    enabled: bool,
}

impl TelemetryStatus {
    fn from_config(config: &TelemetryConfig) -> Self {
        Self {
            enabled: config.enabled,
        }
    }

    fn as_str(self) -> &'static str {
        if self.enabled { "on" } else { "off" }
    }
}

struct TuiApp {
    provider_name: &'static str,
    version: &'static str,
    model: String,
    directory: String,
    language_summary: String,
    mode: SessionMode,
    config_sources: String,
    status_verbosity: StatusVerbosity,
    response_verbosity: ResponseVerbosity,
    tool_output_verbosity: ToolOutputVerbosity,
    transcript_default: TranscriptDefault,
    show_reasoning_usage: bool,
    help_config_inspect: String,
    repo: RepoStatus,
    permissions: PermissionStatus,
    telemetry: TelemetryStatus,
    input: String,
    attachments: Vec<ContextAttachment>,
    context_compaction: ContextCompactionState,
    context_estimate: ContextEstimate,
    transcript: Vec<TranscriptEntry>,
    selected_entry: Option<usize>,
    next_entry_id: u64,
    transcript_scroll_from_bottom: u16,
    pending_assistant: String,
    task_state: Option<TaskStateSnapshot>,
    task_panel_collapsed: bool,
    status: String,
    cost: squeezy_core::CostSnapshot,
    metrics: squeezy_core::TurnMetrics,
    turn_rx: Option<mpsc::Receiver<AgentEvent>>,
    job_rx: Option<broadcast::Receiver<JobEvent>>,
    jobs: BTreeMap<JobId, JobSnapshot>,
    notifications: VecDeque<JobNotification>,
    cancel: Option<CancellationToken>,
    pending_approval: Option<PendingApproval>,
    pending_feedback: Option<PreparedFeedback>,
    pending_report: Option<BugReportBundle>,
    clipboard: Box<dyn Clipboard>,
}

impl TuiApp {
    fn new(
        provider_name: &'static str,
        config: &AppConfig,
        mode: SessionMode,
        startup: StartupProfile,
    ) -> Self {
        Self::new_with_startup(
            provider_name,
            config,
            mode,
            startup,
            Box::new(Osc52Clipboard),
        )
    }

    #[cfg(test)]
    fn new_with_clipboard(
        provider_name: &'static str,
        config: &AppConfig,
        mode: SessionMode,
        onboarding_summary: Option<String>,
        clipboard: Box<dyn Clipboard>,
    ) -> Self {
        Self::new_with_startup(
            provider_name,
            config,
            mode,
            StartupProfile {
                onboarding_summary,
                languages: String::new(),
            },
            clipboard,
        )
    }

    fn new_with_startup(
        provider_name: &'static str,
        config: &AppConfig,
        mode: SessionMode,
        startup: StartupProfile,
        clipboard: Box<dyn Clipboard>,
    ) -> Self {
        let mut transcript = Vec::new();
        let status = if let Some(summary) = startup.onboarding_summary {
            // The onboarding summary is generated by Squeezy itself, not the
            // model, so it belongs to the System role. Using Assistant would
            // both mislabel provenance and mix the same color with later
            // assistant turns, making the seam ambiguous in the transcript.
            let entry = TranscriptEntry::message(
                0,
                TranscriptItem {
                    role: Role::System,
                    content: summary,
                },
                config.tui.transcript_default,
            );
            transcript.push(entry);
            "repo profile ready".to_string()
        } else {
            "ready".to_string()
        };
        let next_entry_id = transcript.len() as u64;
        Self {
            provider_name,
            version: env!("CARGO_PKG_VERSION"),
            model: config.model.clone(),
            directory: compact_path(&config.workspace_root),
            language_summary: if startup.languages.trim().is_empty() {
                configured_language_summary(config)
            } else {
                startup.languages
            },
            mode,
            config_sources: config.config_source_labels().join(","),
            status_verbosity: config.tui.status_verbosity,
            response_verbosity: config.tui.response_verbosity,
            tool_output_verbosity: config.tui.tool_output_verbosity,
            transcript_default: config.tui.transcript_default,
            show_reasoning_usage: config.tui.show_reasoning_usage,
            help_config_inspect: config.inspect_redacted(),
            repo: RepoStatus::detect(config),
            permissions: PermissionStatus::from_policy(&config.permissions),
            telemetry: TelemetryStatus::from_config(&config.telemetry),
            input: String::new(),
            attachments: Vec::new(),
            context_compaction: ContextCompactionState::default(),
            context_estimate: ContextEstimate::default(),
            transcript,
            selected_entry: None,
            next_entry_id,
            transcript_scroll_from_bottom: 0,
            pending_assistant: String::new(),
            task_state: None,
            task_panel_collapsed: false,
            status,
            cost: squeezy_core::CostSnapshot::default(),
            metrics: squeezy_core::TurnMetrics::default(),
            turn_rx: None,
            job_rx: None,
            jobs: BTreeMap::new(),
            notifications: VecDeque::new(),
            cancel: None,
            pending_approval: None,
            pending_feedback: None,
            pending_report: None,
            clipboard,
        }
    }

    fn push_transcript_item(&mut self, item: TranscriptItem) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::message(id, item, self.transcript_default));
    }

    fn push_tool_call(&mut self, call: ToolCall) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::tool_call(
            id,
            call,
            self.transcript_default,
        ));
    }

    fn push_tool_result(&mut self, result: ToolResult) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::tool_result(
            id,
            result,
            self.transcript_default,
        ));
    }

    fn push_log(&mut self, message: String) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::log(id, message, self.transcript_default));
    }

    fn push_entry(&mut self, entry: TranscriptEntry) {
        self.transcript.push(entry);
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_entry_id;
        self.next_entry_id += 1;
        id
    }
}

#[derive(Debug, Clone)]
struct TranscriptEntry {
    id: u64,
    kind: TranscriptEntryKind,
    collapsed: bool,
}

impl TranscriptEntry {
    fn message(id: u64, item: TranscriptItem, transcript_default: TranscriptDefault) -> Self {
        let collapsed = transcript_default == TranscriptDefault::Compact
            && item.role == Role::Assistant
            && item.content.chars().count() > LONG_ASSISTANT_CHARS;
        Self {
            id,
            kind: TranscriptEntryKind::Message(item),
            collapsed,
        }
    }

    fn tool_call(id: u64, call: ToolCall, transcript_default: TranscriptDefault) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::ToolCall(call),
            collapsed: transcript_default == TranscriptDefault::Compact,
        }
    }

    fn tool_result(id: u64, result: ToolResult, transcript_default: TranscriptDefault) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::ToolResult(result),
            collapsed: transcript_default == TranscriptDefault::Compact,
        }
    }

    fn log(id: u64, message: String, transcript_default: TranscriptDefault) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::Log(message),
            collapsed: transcript_default == TranscriptDefault::Compact,
        }
    }

    fn matches_category(&self, category: TranscriptCategory) -> bool {
        match category {
            TranscriptCategory::All => true,
            TranscriptCategory::Tools => matches!(
                self.kind,
                TranscriptEntryKind::ToolCall(_) | TranscriptEntryKind::ToolResult(_)
            ),
            TranscriptCategory::Logs => match &self.kind {
                TranscriptEntryKind::Log(_) => true,
                TranscriptEntryKind::Message(item) => item.role == Role::System,
                _ => false,
            },
            TranscriptCategory::Diffs => match &self.kind {
                TranscriptEntryKind::ToolResult(result) => result.tool_name.contains("diff"),
                _ => false,
            },
            TranscriptCategory::Receipts => match &self.kind {
                TranscriptEntryKind::ToolResult(result) => !result.receipt.output_sha256.is_empty(),
                _ => false,
            },
            TranscriptCategory::Assistant => match &self.kind {
                TranscriptEntryKind::Message(item) => item.role == Role::Assistant,
                _ => false,
            },
        }
    }

    fn plain_text_lines(&self) -> Vec<String> {
        match &self.kind {
            TranscriptEntryKind::Message(item) => {
                vec![format!("{}: {}", role_label(&item.role), item.content)]
            }
            TranscriptEntryKind::ToolCall(call) => {
                vec![format!("tool call: {}", call.name)]
            }
            TranscriptEntryKind::ToolResult(result) => {
                vec![format!("tool result: {}", tool_result_summary(result))]
            }
            TranscriptEntryKind::Log(message) => vec![format!("log: {message}")],
        }
    }

    fn assistant_content(&self) -> Option<String> {
        match &self.kind {
            TranscriptEntryKind::Message(item)
                if item.role == Role::Assistant && !item.content.trim().is_empty() =>
            {
                Some(item.content.clone())
            }
            _ => None,
        }
    }

    fn pin_payload(&self) -> (String, String, String) {
        match &self.kind {
            TranscriptEntryKind::Message(item) => (
                format!("{} message", role_label(&item.role)),
                item.content.clone(),
                format!("transcript:{}", self.id),
            ),
            TranscriptEntryKind::ToolCall(call) => (
                format!("tool call {}", call.name),
                serde_json::to_string(&call.arguments)
                    .unwrap_or_else(|_| call.arguments.to_string()),
                format!("transcript:{}", self.id),
            ),
            TranscriptEntryKind::ToolResult(result) => (
                format!("tool result {}", result.tool_name),
                tool_result_summary(result),
                format!("transcript:{}", self.id),
            ),
            TranscriptEntryKind::Log(message) => (
                "log entry".to_string(),
                message.clone(),
                format!("transcript:{}", self.id),
            ),
        }
    }
}

#[derive(Debug, Clone)]
enum TranscriptEntryKind {
    Message(TranscriptItem),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Log(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptCategory {
    All,
    Tools,
    Logs,
    Diffs,
    Receipts,
    Assistant,
}

struct PendingApproval {
    request: ToolApprovalRequest,
    decision_tx: oneshot::Sender<ToolApprovalDecision>,
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
            .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))
            .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        Ok(Self { terminal })
    }

    fn draw<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.terminal
            .draw(f)
            .map(|_| ())
            .map_err(|err| SqueezyError::Terminal(err.to_string()))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
