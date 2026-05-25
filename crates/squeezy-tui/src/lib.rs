use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fmt,
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::{
    Command,
    cursor::MoveTo,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseEventKind,
    },
    execute,
    style::Print,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use squeezy_agent::{
    Agent, AgentEvent, JobEvent, JobId, JobNotification, JobSnapshot, MAX_JOB_NOTIFICATIONS,
    MAX_JOBS_RETAINED, SessionAccountingSnapshot, ToolApprovalDecision, ToolApprovalRequest,
};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextCompactionRecord, ContextCompactionState, ContextEstimate,
    PermissionPolicy, ResponseVerbosity, Result, Role, SessionMode, SqueezyError, StatusVerbosity,
    TaskStateSnapshot, TelemetryConfig, ToolOutputVerbosity, TranscriptDefault, TranscriptItem,
    TuiAlternateScreen,
};
use squeezy_llm::{LlmProvider, RequestTokenEstimate};
use squeezy_skills::{HelpStatus, SqueezyHelp};
use squeezy_store::{BugReportBundle, BugReportOptions, SessionQuery, parse_bug_report_section};
use squeezy_telemetry::PreparedFeedback;
use squeezy_tools::{ToolCall, ToolResult, ToolStatus};
use squeezy_vcs::{DiffMode, DiffOptions, GitVcs, VcsKind};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod render;

use render::palette::{
    AMBER, ERROR_RED, GOLD, MODE_BUILD_GREEN, MODE_PURPLE, PROMPT_BG, QUIET, SUCCESS_GREEN,
    WORKING_SHIMMER_HIGHLIGHT, blend_color,
};
#[cfg(test)]
use render::palette::{DIFF_ADD_FG, DIFF_DEL_FG};

const INLINE_PASTE_MAX_BYTES: usize = 512;
const LONG_ASSISTANT_CHARS: usize = 1_200;
const TOOL_PREVIEW_COMPACT_BYTES: usize = 300;
const TOOL_PREVIEW_NORMAL_BYTES: usize = 1_200;
const TOOL_PREVIEW_VERBOSE_BYTES: usize = 4_000;
const SHELL_COLLAPSED_OUTPUT_PREVIEW_LINES: usize = 80;
const PROMPT_MIN_HEIGHT: u16 = 3;
const PROMPT_MAX_HEIGHT: u16 = 8;
const INLINE_VIEWPORT_HEIGHT: u16 = 18;
const SLASH_MENU_MAX_ITEMS: usize = 5;
const DISABLE_MOUSE_MODES: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";
const CLEAR_SCROLLBACK_AND_VISIBLE: &str = "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "alternate scroll is only supported through ANSI escape sequences",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "alternate scroll is only supported through ANSI escape sequences",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlashCommand {
    name: &'static str,
    description: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        description: "show local Squeezy help topics",
    },
    SlashCommand {
        name: "/model",
        description: "show current provider and model",
    },
    SlashCommand {
        name: "/permissions",
        description: "show current permission policy",
    },
    SlashCommand {
        name: "/plan",
        description: "switch to Plan mode",
    },
    SlashCommand {
        name: "/build",
        description: "switch to Build mode",
    },
    SlashCommand {
        name: "/cost",
        description: "show token and cost accounting",
    },
    SlashCommand {
        name: "/context",
        description: "show context budget and compaction state",
    },
    SlashCommand {
        name: "/attach",
        description: "attach a file as prompt context",
    },
    SlashCommand {
        name: "/attachments",
        description: "list attached context",
    },
    SlashCommand {
        name: "/copy",
        description: "copy last answer or transcript",
    },
    SlashCommand {
        name: "/compact",
        description: "compact conversation context now",
    },
    SlashCommand {
        name: "/collapse",
        description: "collapse transcript entries",
    },
    SlashCommand {
        name: "/expand",
        description: "expand transcript entries",
    },
    SlashCommand {
        name: "/jobs",
        description: "list background jobs",
    },
    SlashCommand {
        name: "/job",
        description: "show a background job",
    },
    SlashCommand {
        name: "/job-cancel",
        description: "cancel a background job",
    },
    SlashCommand {
        name: "/pin",
        description: "pin transcript context",
    },
    SlashCommand {
        name: "/pins",
        description: "list pinned context",
    },
    SlashCommand {
        name: "/unpin",
        description: "remove pinned context",
    },
    SlashCommand {
        name: "/feedback",
        description: "preview or send product feedback",
    },
    SlashCommand {
        name: "/report",
        description: "preview or send a bug report",
    },
    SlashCommand {
        name: "/sessions",
        description: "list recent sessions",
    },
    SlashCommand {
        name: "/session",
        description: "show a saved session",
    },
    SlashCommand {
        name: "/resume",
        description: "resume a saved session",
    },
    SlashCommand {
        name: "/session-export",
        description: "export a saved session",
    },
    SlashCommand {
        name: "/session-cleanup",
        description: "remove old sessions",
    },
    SlashCommand {
        name: "/checkpoints",
        description: "list local checkpoints",
    },
    SlashCommand {
        name: "/checkpoint",
        description: "show a local checkpoint",
    },
    SlashCommand {
        name: "/undo",
        description: "undo the latest checkpoint",
    },
    SlashCommand {
        name: "/revert-turn",
        description: "revert a turn checkpoint",
    },
    SlashCommand {
        name: "/verbosity",
        description: "set answer verbosity",
    },
    SlashCommand {
        name: "/tool-verbosity",
        description: "set tool output verbosity",
    },
    SlashCommand {
        name: "/detach",
        description: "remove attached context",
    },
];

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
    let mut terminal = TerminalGuard::enter(config.tui.alternate_screen)?;
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
    terminal.set_exit_hint(exit_hint(agent.session_id().as_deref()));

    loop {
        app.animation_tick = app.animation_tick.wrapping_add(1);
        terminal.draw_app(&app)?;

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
                    app.turn_visual = TurnVisualState::Running;
                    app.note_turn_started();
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
                        app.status = format!("running {}", tool_call_label(&call));
                        app.remember_active_tool_call(call);
                    }
                }
                AgentEvent::ToolCallCompleted { result, .. } => {
                    app.status = tool_result_status_text(&result);
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
                AgentEvent::Completed {
                    message,
                    cost,
                    metrics,
                    ..
                } => {
                    if let Some(message) = dedupe_assistant_repeated_tool_output(app, message) {
                        app.push_transcript_item(message);
                    }
                    app.pending_assistant.clear();
                    app.cost = cost;
                    app.metrics = metrics;
                    app.status = "ready".to_string();
                    app.turn_visual = TurnVisualState::Succeeded;
                    app.clear_active_tools();
                    app.note_turn_finished();
                    // Preserve the user's scroll position; if they paged up
                    // mid-turn we shouldn't snap them down on completion.
                    app.cancel = None;
                    keep_rx = false;
                    break;
                }
                AgentEvent::Cancelled { .. } => {
                    app.status = "cancelled; edit prompt or retry".to_string();
                    app.turn_visual = TurnVisualState::Failed;
                    app.push_log("turn cancelled".to_string());
                    app.pending_assistant.clear();
                    app.clear_active_tools();
                    app.note_turn_finished();
                    app.cancel = None;
                    keep_rx = false;
                    break;
                }
                AgentEvent::Failed { error, .. } => {
                    app.status = format_error_status(&error);
                    app.turn_visual = TurnVisualState::Failed;
                    app.push_log(format!("turn failed: {}", app.status));
                    app.pending_assistant.clear();
                    app.clear_active_tools();
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
        Event::Mouse(mouse) => {
            handle_mouse(app, mouse.kind);
            Ok(false)
        }
        Event::Paste(text) => {
            handle_paste(app, agent, text).await?;
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn handle_mouse(app: &mut TuiApp, kind: MouseEventKind) {
    if !app.alternate_scroll_enabled {
        return;
    }
    match kind {
        MouseEventKind::ScrollUp => scroll_transcript_up(app, 3),
        MouseEventKind::ScrollDown => scroll_transcript_down(app, 3),
        _ => {}
    }
}

async fn handle_key(app: &mut TuiApp, agent: &mut Agent, key: KeyEvent) -> Result<bool> {
    if key.code != KeyCode::Esc {
        app.exit_armed = false;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if request_turn_interrupt(app) {
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

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && (key.code == KeyCode::Char('j') || key.code == KeyCode::Enter)
    {
        insert_input_char(app, '\n');
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
            clear_input(app);
        }
        return Ok(false);
    }

    if key.code == KeyCode::Esc && request_turn_interrupt(app) {
        return Ok(false);
    }

    if handle_approval_key(app, key) {
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc => {
            if request_turn_interrupt(app) {
                Ok(false)
            } else if app.exit_armed {
                Ok(true)
            } else {
                app.exit_armed = true;
                Ok(false)
            }
        }
        // Scroll keys intentionally leave `app.status` alone so command
        // handlers can keep their latest state even though the footer stays
        // context-only.
        KeyCode::PageUp => {
            scroll_transcript_up(app, 8);
            Ok(false)
        }
        KeyCode::PageDown => {
            scroll_transcript_down(app, 8);
            Ok(false)
        }
        KeyCode::Home => {
            if app.input.is_empty() {
                app.transcript_scroll_from_bottom = u16::MAX;
            } else {
                app.input_cursor = 0;
            }
            Ok(false)
        }
        KeyCode::End => {
            if app.input.is_empty() {
                app.transcript_scroll_from_bottom = 0;
            } else {
                app.input_cursor = app.input.len();
            }
            Ok(false)
        }
        KeyCode::Left => {
            move_input_cursor_left(app);
            Ok(false)
        }
        KeyCode::Right => {
            move_input_cursor_right(app);
            Ok(false)
        }
        KeyCode::Up => {
            if move_slash_menu_selection(app, SelectionDirection::Previous) {
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                select_previous_transcript_entry(app);
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                recall_prompt_history(app, HistoryDirection::Previous);
            } else if should_route_plain_arrow_to_scroll(app) {
                scroll_transcript_up(app, 3);
            } else {
                recall_prompt_history(app, HistoryDirection::Previous);
            }
            Ok(false)
        }
        KeyCode::Down => {
            if move_slash_menu_selection(app, SelectionDirection::Next) {
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                select_next_transcript_entry(app);
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                recall_prompt_history(app, HistoryDirection::Next);
            } else if should_route_plain_arrow_to_scroll(app) {
                scroll_transcript_down(app, 3);
            } else {
                recall_prompt_history(app, HistoryDirection::Next);
            }
            Ok(false)
        }
        KeyCode::Enter => {
            if app.turn_rx.is_some() {
                app.status = "turn already running; press Ctrl-C to cancel".to_string();
                return Ok(false);
            }
            if complete_selected_slash_command(app) {
                return Ok(false);
            }
            if input_cursor(app) == app.input.len() && app.input.ends_with('\\') {
                delete_before_cursor(app);
                insert_input_char(app, '\n');
                return Ok(false);
            }
            let input = app.input.trim().to_string();
            if input.is_empty() {
                app.status = "enter a prompt first".to_string();
                return Ok(false);
            }
            if handle_slash_command(app, agent, &input).await {
                clear_input(app);
                app.input_history_index = None;
                app.input_history_draft.clear();
                app.slash_menu_index = 0;
                return Ok(false);
            }
            if reject_unknown_slash_command(app, &input) {
                return Ok(false);
            }
            clear_input(app);
            push_input_history(app, input.clone());
            let cancel = CancellationToken::new();
            app.task_state = None;
            app.task_panel_collapsed = false;
            app.note_turn_started();
            app.turn_rx = Some(agent.start_turn_with_response_verbosity(
                input,
                cancel.clone(),
                app.response_verbosity,
            ));
            app.cancel = Some(cancel);
            app.status = "starting turn".to_string();
            app.turn_visual = TurnVisualState::Running;
            Ok(false)
        }
        KeyCode::Backspace => {
            delete_before_cursor(app);
            Ok(false)
        }
        KeyCode::Delete => {
            delete_at_cursor(app);
            Ok(false)
        }
        KeyCode::Char(ch) => {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                insert_input_char(app, ch);
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
        insert_input_text(app, &text);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionDirection {
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryDirection {
    Previous,
    Next,
}

fn note_input_edited(app: &mut TuiApp) {
    app.input_history_index = None;
    app.input_history_draft.clear();
    clamp_slash_menu_index(app);
}

fn clear_input(app: &mut TuiApp) {
    app.input.clear();
    app.input_cursor = 0;
    clamp_slash_menu_index(app);
}

fn set_input(app: &mut TuiApp, input: String) {
    app.input = input;
    app.input_cursor = app.input.len();
    clamp_input_cursor(app);
    clamp_slash_menu_index(app);
}

fn input_cursor(app: &TuiApp) -> usize {
    text_cursor(&app.input, app.input_cursor)
}

fn clamp_input_cursor(app: &mut TuiApp) {
    app.input_cursor = text_cursor(&app.input, app.input_cursor);
}

fn text_cursor(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn insert_input_char(app: &mut TuiApp, ch: char) {
    clamp_input_cursor(app);
    app.input.insert(app.input_cursor, ch);
    app.input_cursor += ch.len_utf8();
    note_input_edited(app);
}

fn insert_input_text(app: &mut TuiApp, text: &str) {
    if text.is_empty() {
        return;
    }
    clamp_input_cursor(app);
    app.input.insert_str(app.input_cursor, text);
    app.input_cursor += text.len();
    note_input_edited(app);
}

fn delete_before_cursor(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    if cursor == 0 {
        app.input_cursor = 0;
        return;
    }
    let previous = app.input[..cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
    app.input.drain(previous..cursor);
    app.input_cursor = previous;
    note_input_edited(app);
}

fn delete_at_cursor(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    if cursor >= app.input.len() {
        app.input_cursor = app.input.len();
        return;
    }
    let next = cursor
        + app.input[cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
    app.input.drain(cursor..next);
    app.input_cursor = cursor;
    note_input_edited(app);
}

fn move_input_cursor_left(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    app.input_cursor = app.input[..cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
}

fn move_input_cursor_right(app: &mut TuiApp) {
    let cursor = input_cursor(app);
    if cursor >= app.input.len() {
        app.input_cursor = app.input.len();
        return;
    }
    app.input_cursor = cursor
        + app.input[cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
}

fn scroll_transcript_up(app: &mut TuiApp, lines: u16) {
    app.transcript_scroll_from_bottom = app.transcript_scroll_from_bottom.saturating_add(lines);
}

fn scroll_transcript_down(app: &mut TuiApp, lines: u16) {
    app.transcript_scroll_from_bottom = app.transcript_scroll_from_bottom.saturating_sub(lines);
}

fn should_route_plain_arrow_to_scroll(app: &TuiApp) -> bool {
    app.alternate_scroll_enabled
        && app.input_history_index.is_none()
        && !app.transcript.is_empty()
        && (app.transcript_scroll_from_bottom > 0 || !app.input.trim().is_empty())
}

fn push_input_history(app: &mut TuiApp, input: String) {
    if input.trim().is_empty() || input.starts_with('/') {
        return;
    }
    if app.input_history.last().is_some_and(|last| last == &input) {
        return;
    }
    app.input_history.push(input);
    if app.input_history.len() > 100 {
        app.input_history.remove(0);
    }
}

fn reject_unknown_slash_command(app: &mut TuiApp, input: &str) -> bool {
    if !input.starts_with('/') {
        return false;
    }
    app.status = "unknown command; use Up/Down to choose a / command".to_string();
    true
}

fn recall_prompt_history(app: &mut TuiApp, direction: HistoryDirection) {
    if app.input_history.is_empty() {
        app.status = "no prompt history".to_string();
        return;
    }
    if app.input_history_index.is_none() && !app.input.trim().is_empty() {
        return;
    }
    let last = app.input_history.len() - 1;
    let next = match (app.input_history_index, direction) {
        (None, HistoryDirection::Previous) => {
            app.input_history_draft = if app.input.trim().is_empty() {
                String::new()
            } else {
                app.input.clone()
            };
            Some(last)
        }
        (None, HistoryDirection::Next) => return,
        (Some(0), HistoryDirection::Previous) => Some(0),
        (Some(index), HistoryDirection::Previous) => Some(index - 1),
        (Some(index), HistoryDirection::Next) if index >= last => {
            set_input(app, app.input_history_draft.clone());
            app.input_history_draft.clear();
            app.input_history_index = None;
            app.slash_menu_index = 0;
            return;
        }
        (Some(index), HistoryDirection::Next) => Some(index + 1),
    };
    if let Some(index) = next {
        set_input(app, app.input_history[index].clone());
        app.input_history_index = Some(index);
        app.selected_entry = None;
        app.slash_menu_index = 0;
    }
}

fn slash_suggestions(input: &str) -> Vec<SlashCommand> {
    if !is_slash_completion_input(input) {
        return Vec::new();
    }
    let prefix = input.trim();
    let mut suggestions = SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|command| command.name.starts_with(prefix))
        .collect::<Vec<_>>();
    suggestions.sort_by(|left, right| left.name.cmp(right.name));
    suggestions
}

fn is_slash_completion_input(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.starts_with('/')
        && !trimmed[1..].contains(char::is_whitespace)
        && !trimmed.contains('\n')
}

fn clamp_slash_menu_index(app: &mut TuiApp) {
    let count = slash_suggestions(&app.input).len();
    if count == 0 {
        app.slash_menu_index = 0;
    } else if app.slash_menu_index >= count {
        app.slash_menu_index = count - 1;
    }
}

fn move_slash_menu_selection(app: &mut TuiApp, direction: SelectionDirection) -> bool {
    let count = slash_suggestions(&app.input).len();
    if count == 0 {
        return false;
    }
    app.slash_menu_index = match direction {
        SelectionDirection::Previous => app.slash_menu_index.saturating_sub(1),
        SelectionDirection::Next => (app.slash_menu_index + 1).min(count - 1),
    };
    true
}

fn complete_selected_slash_command(app: &mut TuiApp) -> bool {
    let suggestions = slash_suggestions(&app.input);
    if suggestions.is_empty() {
        return false;
    }
    let selected = suggestions[app.slash_menu_index.min(suggestions.len() - 1)];
    if app.input.trim() == selected.name {
        return false;
    }
    set_input(app, format!("{} ", selected.name));
    app.slash_menu_index = 0;
    app.status = format!("selected {}", selected.name);
    true
}

fn request_turn_interrupt(app: &mut TuiApp) -> bool {
    let mut interrupted = false;
    if let Some(cancel) = &app.cancel {
        cancel.cancel();
        interrupted = true;
    }
    if let Some(pending) = app.pending_approval.take() {
        let _ = pending.decision_tx.send(ToolApprovalDecision::Cancelled);
        interrupted = true;
    }
    if interrupted {
        app.status = "interrupting".to_string();
        app.turn_visual = TurnVisualState::Failed;
        app.clear_active_tools();
    }
    interrupted
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
        "/model" => {
            app.status = "model settings".to_string();
            app.push_transcript_item(TranscriptItem::system(format!(
                "Model\nprovider={}\nmodel={}\nchange=exit and run `squeezy --no-default`, or start with `--provider <id> --model <id>`",
                app.provider_name, app.model
            )));
            return true;
        }
        "/permissions" => {
            app.status = "permission settings".to_string();
            app.push_transcript_item(TranscriptItem::system(format!(
                "Permissions\n{}\nsandbox={}",
                app.permissions.compact(),
                app.permissions.sandbox
            )));
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
subagents calls={} failures={} estimated_usd={} input={} output={} tool_calls={} budget_denials={}\n\
receipts stub_hits={} negative_stub_hits={} total_hits={}\n\
spills writes={} reads={}\n\
io bytes_read={} files_scanned={} matches_returned={} model_output_bytes={} subagent_bytes_read={} subagent_files_scanned={} subagent_model_output_bytes={}\n\
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
        metrics.subagent_calls,
        metrics.subagent_failures,
        format_cost(&metrics.subagent_provider),
        format_optional_u64(metrics.subagent_provider.input_tokens),
        format_optional_u64(metrics.subagent_provider.output_tokens),
        metrics.subagent_tool_calls,
        metrics.subagent_budget_denials,
        metrics.receipt_stub_hits,
        metrics.negative_receipt_hits,
        metrics.receipt_stub_hits + metrics.negative_receipt_hits,
        metrics.spill_writes,
        metrics.spill_reads,
        metrics.bytes_read,
        metrics.files_scanned,
        metrics.matches_returned,
        metrics.model_output_bytes,
        metrics.subagent_bytes_read,
        metrics.subagent_files_scanned,
        metrics.subagent_model_output_bytes,
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
subagent_volume calls={} failures={} tool_calls={} bytes_read={} files_scanned={} model_output_bytes={} budget_denials={}\n\
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
        snapshot.metrics.subagent_calls,
        snapshot.metrics.subagent_failures,
        snapshot.metrics.subagent_tool_calls,
        snapshot.metrics.subagent_bytes_read,
        snapshot.metrics.subagent_files_scanned,
        snapshot.metrics.subagent_model_output_bytes,
        snapshot.metrics.subagent_budget_denials,
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
    let Some(index) = app
        .selected_entry
        .or_else(|| latest_toggleable_transcript_entry(app))
    else {
        app.status = "transcript is empty".to_string();
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

fn latest_toggleable_transcript_entry(app: &TuiApp) -> Option<usize> {
    app.transcript
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| entry.is_toggleable())
        .map(|(index, _)| index)
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
        KeyCode::Up => {
            app.approval_selection_index = app.approval_selection_index.saturating_sub(1);
            app.status = format_approval_status_line(&pending.request);
            app.pending_approval = Some(pending);
            true
        }
        KeyCode::Down => {
            app.approval_selection_index =
                (app.approval_selection_index + 1).min(approval_options().len() - 1);
            app.status = format_approval_status_line(&pending.request);
            app.pending_approval = Some(pending);
            true
        }
        KeyCode::Enter => {
            let option = approval_options()
                .get(app.approval_selection_index)
                .copied()
                .unwrap_or(APPROVAL_ONCE);
            send_approval_decision(app, pending, option)
        }
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            send_approval_decision(app, pending, APPROVAL_ONCE)
        }
        KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Char('p') | KeyCode::Char('P') => {
            send_approval_decision(app, pending, APPROVAL_PROJECT)
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('d') | KeyCode::Char('D') => {
            send_approval_decision(app, pending, APPROVAL_DENY)
        }
        _ => {
            app.status = format_approval_status_line(&pending.request);
            app.pending_approval = Some(pending);
            true
        }
    }
}

fn send_approval_decision(
    app: &mut TuiApp,
    pending: PendingApproval,
    option: ApprovalOption,
) -> bool {
    let tool_name = pending.request.tool_name.clone();
    let _ = pending.decision_tx.send(option.decision);
    app.status = match option.choice {
        ApprovalChoice::Approve => format!("approved {tool_name}"),
        ApprovalChoice::ApproveProject => format!("saved repo approval for {tool_name}"),
        ApprovalChoice::Deny => format!("denied {tool_name}"),
    };
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalChoice {
    Approve,
    ApproveProject,
    Deny,
}

#[derive(Debug, Clone, Copy)]
struct ApprovalOption {
    choice: ApprovalChoice,
    label: &'static str,
    hint: &'static str,
    decision: ToolApprovalDecision,
}

const APPROVAL_ONCE: ApprovalOption = ApprovalOption {
    choice: ApprovalChoice::Approve,
    label: "Approve",
    hint: "run this once",
    decision: ToolApprovalDecision::AllowOnce,
};

const APPROVAL_PROJECT: ApprovalOption = ApprovalOption {
    choice: ApprovalChoice::ApproveProject,
    label: "Always approve this command in this repo",
    hint: "save a project rule",
    decision: ToolApprovalDecision::AllowRuleProject,
};

const APPROVAL_DENY: ApprovalOption = ApprovalOption {
    choice: ApprovalChoice::Deny,
    label: "Deny",
    hint: "skip this run",
    decision: ToolApprovalDecision::DenyOnce,
};

fn approval_options() -> &'static [ApprovalOption] {
    &[APPROVAL_ONCE, APPROVAL_PROJECT, APPROVAL_DENY]
}

/// Single-line status banner shown in the 1-line status bar. Compact by
/// design so the status bar remains useful for non-approval traffic.
pub(crate) fn format_approval_status_line(request: &ToolApprovalRequest) -> String {
    let permission = &request.permission;
    format!(
        "approval needed: {tool} risk={risk} target={target}",
        tool = request.tool_name,
        risk = permission.risk.as_str(),
        target = permission.target,
    )
}

#[cfg(test)]
pub(crate) fn format_approval_prompt(request: &ToolApprovalRequest) -> String {
    format_approval_menu_lines(request, 0)
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_approval_menu_lines(
    request: &ToolApprovalRequest,
    selected: usize,
) -> Vec<Line<'static>> {
    let permission = &request.permission;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "Approval needed",
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {} · {}", request.tool_name, permission.risk.as_str()),
            Style::default().fg(QUIET),
        ),
    ])];
    if let Some(command) = permission.metadata.get("command") {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(command.clone(), Style::default().fg(Color::White)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(permission.target.clone(), Style::default().fg(Color::White)),
        ]));
    }
    if let Some(cwd) = permission.metadata.get("cwd") {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("cwd {cwd}"), Style::default().fg(QUIET)),
        ]));
    }
    for (index, option) in approval_options().iter().enumerate() {
        let is_selected = index == selected.min(approval_options().len() - 1);
        let marker = if is_selected { "› " } else { "  " };
        let label_style = if is_selected {
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if is_selected { GOLD } else { QUIET }),
            ),
            Span::styled(option.label, label_style),
            Span::styled(format!(" · {}", option.hint), Style::default().fg(QUIET)),
        ]));
    }
    lines
}

fn render(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    let include_startup_card = area.height >= 16;
    let input_height = input_panel_height(app, area.width);
    let approval_height = approval_menu_height(app);
    let task_height = if should_show_task_panel(app) {
        let h = if approval_height > 0 {
            task_panel_height(app).min(5)
        } else {
            task_panel_height(app)
        };
        Some(h)
    } else {
        None
    };
    let required_height = task_height
        .unwrap_or(0)
        .saturating_add(approval_height)
        .saturating_add(input_height)
        .saturating_add(2);
    let optional_height = area.height.saturating_sub(required_height);
    let attachment_height = attachment_panel_height(app, optional_height);
    let requested_transcript_gap_height = transcript_prompt_gap_height(app);
    let reserved_height = required_height.saturating_add(attachment_height);
    let transcript_visual_height =
        transcript_visual_line_count(app, area.width, include_startup_card);
    let available_without_gap = area.height.saturating_sub(reserved_height);
    let transcript_prompt_gap_height = if requested_transcript_gap_height > 0
        && transcript_visual_height < available_without_gap
    {
        requested_transcript_gap_height
    } else {
        0
    };
    let available_transcript_height =
        available_without_gap.saturating_sub(transcript_prompt_gap_height);
    let transcript_height = transcript_visual_height.min(available_transcript_height);
    let mut constraints = Vec::new();
    if transcript_height > 0 {
        constraints.push(Constraint::Length(transcript_height));
    }
    if transcript_prompt_gap_height > 0 {
        constraints.push(Constraint::Length(transcript_prompt_gap_height));
    }
    if let Some(h) = task_height {
        constraints.push(Constraint::Length(h));
    }
    if attachment_height > 0 {
        constraints.push(Constraint::Length(attachment_height));
    }
    constraints.push(Constraint::Length(input_height));
    if approval_height > 0 {
        constraints.push(Constraint::Length(approval_height));
    }
    constraints.push(Constraint::Length(2));
    constraints.push(Constraint::Min(0));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut index = 0;
    if transcript_height > 0 {
        render_transcript(frame, chunks[index], app, include_startup_card);
        index += 1;
    }
    if transcript_prompt_gap_height > 0 {
        index += 1;
    }
    if task_height.is_some() {
        render_task_state(frame, chunks[index], app);
        index += 1;
    }
    if attachment_height > 0 {
        render_attachments(frame, chunks[index], app);
        index += 1;
    }
    render_input(frame, chunks[index], app);
    index += 1;
    if approval_height > 0 {
        render_approval(frame, chunks[index], app);
        index += 1;
    }
    render_status(frame, chunks[index], app);
    index += 1;
    // Flexible filler keeps the prompt/status block attached to the transcript
    // instead of pinning it to the terminal bottom.
    let _ = chunks[index];
}

fn render_inline(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    let input_height = input_panel_height(app, area.width);
    let approval_height = approval_menu_height(app);
    let task_height = should_show_task_panel(app).then_some(task_panel_height(app));
    let status_height = 2;
    let live_lines = pending_assistant_lines(app);
    let live_visual_height = visual_line_count(&live_lines, area.width);
    let live_gap = if live_visual_height > 0 { 1 } else { 0 };
    let required_height = task_height
        .unwrap_or(0)
        .saturating_add(input_height)
        .saturating_add(approval_height)
        .saturating_add(status_height)
        .saturating_add(live_gap);
    let attachment_height =
        attachment_panel_height(app, area.height.saturating_sub(required_height));
    let reserved_height = required_height.saturating_add(attachment_height);
    let live_height = live_visual_height.min(area.height.saturating_sub(reserved_height));

    let mut constraints = Vec::new();
    if live_height > 0 {
        constraints.push(Constraint::Length(live_height));
    }
    if live_gap > 0 && live_height > 0 {
        constraints.push(Constraint::Length(live_gap));
    }
    if let Some(h) = task_height {
        constraints.push(Constraint::Length(h));
    }
    if attachment_height > 0 {
        constraints.push(Constraint::Length(attachment_height));
    }
    constraints.push(Constraint::Length(input_height));
    if approval_height > 0 {
        constraints.push(Constraint::Length(approval_height));
    }
    constraints.push(Constraint::Length(status_height));
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut index = 0;
    if live_height > 0 {
        let scroll = transcript_scroll_offset(live_lines.len(), live_height, 0);
        let paragraph = Paragraph::new(live_lines)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, chunks[index]);
        index += 1;
    }
    if live_gap > 0 && live_height > 0 {
        index += 1;
    }
    if task_height.is_some() {
        render_task_state(frame, chunks[index], app);
        index += 1;
    }
    if attachment_height > 0 {
        render_attachments(frame, chunks[index], app);
        index += 1;
    }
    render_input(frame, chunks[index], app);
    index += 1;
    if approval_height > 0 {
        render_approval(frame, chunks[index], app);
        index += 1;
    }
    render_status(frame, chunks[index], app);
}

fn transcript_prompt_gap_height(app: &TuiApp) -> u16 {
    if app.transcript.is_empty() && app.pending_assistant.is_empty() {
        0
    } else {
        1
    }
}

fn should_show_task_panel(app: &TuiApp) -> bool {
    turn_in_progress(app)
        || app.last_turn_duration.is_some()
        || app
            .task_state
            .as_ref()
            .is_some_and(|snapshot| snapshot.status != squeezy_core::TaskStateStatus::Completed)
}

fn task_panel_height(_app: &TuiApp) -> u16 {
    1
}

fn render_task_state(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let line = if turn_in_progress(app) {
        working_line(app)
    } else if let Some(duration) = app.last_turn_duration {
        worked_divider_line(duration, area.width)
    } else if let Some(snapshot) = app.task_state.as_ref() {
        compact_task_state_line(snapshot)
    } else {
        working_line(app)
    };
    let paragraph = Paragraph::new(vec![line])
        .style(Style::default().fg(QUIET))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn turn_in_progress(app: &TuiApp) -> bool {
    app.turn_rx.is_some()
        || app.cancel.is_some()
        || (app.last_turn_duration.is_none()
            && app
                .task_state
                .as_ref()
                .is_some_and(|snapshot| snapshot.status == squeezy_core::TaskStateStatus::Running))
}

fn working_line(app: &TuiApp) -> Line<'static> {
    let interrupting = app.status == "interrupting";
    let activity_color = if interrupting { ERROR_RED } else { AMBER };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            "• ",
            Style::default()
                .fg(activity_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(if interrupting {
        vec![Span::styled(
            "Interrupting",
            Style::default().fg(ERROR_RED).add_modifier(Modifier::BOLD),
        )]
    } else {
        working_word_spans(app)
    });
    spans.push(Span::styled(
        format!(
            " ({} • esc to interrupt)",
            format_turn_duration(current_turn_duration(app))
        ),
        Style::default().fg(QUIET),
    ));
    if let Some(call) = app
        .active_tool_calls
        .values()
        .find(|call| !is_control_tool_name(&call.name))
    {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.extend(active_tool_spans(call));
    }
    Line::from(spans)
}

fn working_word_spans(app: &TuiApp) -> Vec<Span<'static>> {
    shimmer_word_spans("Working", prompt_elapsed_ms(app))
}

fn shimmer_word_spans(text: &'static str, elapsed_ms: u64) -> Vec<Span<'static>> {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return Vec::new();
    }
    let padding = 4usize;
    let band_half_width = 2.25f32;
    let period = chars.len() + padding * 2;
    let sweep_ms = 3_400u64;
    let position =
        ((elapsed_ms % sweep_ms) as f32 / sweep_ms as f32) * period.saturating_sub(1) as f32;
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let char_position = (index + padding) as f32;
            let distance = (char_position - position).abs();
            let intensity = if distance <= band_half_width {
                let x = std::f32::consts::PI * (distance / band_half_width);
                0.5 * (1.0 + x.cos())
            } else {
                0.0
            };
            let style = Style::default()
                .fg(blend_color(AMBER, WORKING_SHIMMER_HIGHLIGHT, intensity))
                .add_modifier(Modifier::BOLD);
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

fn current_turn_duration(app: &TuiApp) -> Duration {
    app.turn_started_at
        .map(|started_at| started_at.elapsed())
        .unwrap_or_default()
}

fn worked_divider_line(duration: Duration, width: u16) -> Line<'static> {
    let label = format!("─ Worked for {} ", format_turn_duration(duration));
    let label_width = label.chars().count();
    let fill_width = (width as usize).saturating_sub(label_width);
    let mut text = label;
    text.push_str(&"─".repeat(fill_width));
    Line::from(Span::styled(text, Style::default().fg(QUIET)))
}

fn compact_task_state_line(snapshot: &TaskStateSnapshot) -> Line<'static> {
    let (label, color) = task_status_label_color(snapshot.status);
    let detail = if task_title(snapshot) == "current turn" {
        None
    } else {
        Some(task_title(snapshot).to_string())
    };
    turn_state_line(label, detail, color)
}

fn task_status_label_color(status: squeezy_core::TaskStateStatus) -> (&'static str, Color) {
    match status {
        squeezy_core::TaskStateStatus::Running => ("Working", AMBER),
        squeezy_core::TaskStateStatus::Blocked => ("Blocked", GOLD),
        squeezy_core::TaskStateStatus::Completed => ("Done", SUCCESS_GREEN),
        squeezy_core::TaskStateStatus::Cancelled => ("Cancelled", ERROR_RED),
        squeezy_core::TaskStateStatus::Failed => ("Failed", ERROR_RED),
    }
}

fn turn_state_line(label: &'static str, detail: Option<String>, color: Color) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            "• ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(detail) = detail.filter(|value| !value.trim().is_empty()) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(detail, Style::default().fg(QUIET)));
    }
    Line::from(spans)
}

fn format_turn_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn task_title(snapshot: &TaskStateSnapshot) -> &str {
    if snapshot.task.is_empty() {
        "current turn"
    } else {
        snapshot.task.as_str()
    }
}

fn approval_menu_height(app: &TuiApp) -> u16 {
    if app.pending_approval.is_some() { 6 } else { 0 }
}

fn render_approval(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph = Paragraph::new(approval_lines(app))
        .style(Style::default().fg(QUIET))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn approval_lines(app: &TuiApp) -> Vec<Line<'static>> {
    app.pending_approval
        .as_ref()
        .map(|pending| format_approval_menu_lines(&pending.request, app.approval_selection_index))
        .unwrap_or_default()
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, include_startup_card: bool) {
    let lines = transcript_lines_for_render(app, Some(area.width), include_startup_card);
    let scroll =
        transcript_scroll_offset(lines.len(), area.height, app.transcript_scroll_from_bottom);
    let paragraph = Paragraph::new(lines)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn transcript_lines_for_render(
    app: &TuiApp,
    width: Option<u16>,
    include_startup_card: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if include_startup_card {
        let card_width = width.unwrap_or(64);
        lines.extend(startup_card_lines(app, card_width));
        lines.push(Line::from(""));
    }
    for (index, item) in app.transcript.iter().enumerate() {
        lines.extend(format_transcript_entry_with_width(
            item,
            app.selected_entry == Some(index),
            app.tool_output_verbosity,
            message_outcome(&app.transcript, index),
            width,
        ));
    }
    if let Some(pending_assistant) = pending_assistant_display_content(app) {
        lines.extend(assistant_text_lines(
            false,
            turn_coin_span(app),
            &pending_assistant,
            Style::default(),
        ));
        lines.push(Line::from(""));
    }
    lines
}

fn pending_assistant_lines(app: &TuiApp) -> Vec<Line<'static>> {
    pending_assistant_display_content(app)
        .map(|content| assistant_text_lines(false, turn_coin_span(app), &content, Style::default()))
        .unwrap_or_default()
}

fn startup_card_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let card_width = width.clamp(36, 64) as usize;
    let inner = card_width.saturating_sub(2);
    let border = "─".repeat(inner);
    vec![
        Line::from(Span::styled(
            format!("╭{border}╮"),
            Style::default().fg(GOLD),
        )),
        startup_card_row(
            inner,
            "",
            format!(">_ Squeezy v{}", app.version),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        startup_card_row(
            inner,
            "model",
            format!("{}:{}", app.provider_name, app.model),
            Style::default().fg(GOLD),
        ),
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
            Style::default().fg(GOLD),
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
            Span::styled("│ ", Style::default().fg(GOLD)),
            Span::styled(value, value_style),
            Span::raw(padding),
            Span::styled("│", Style::default().fg(GOLD)),
        ]);
    }
    Line::from(vec![
        Span::styled("│ ", Style::default().fg(GOLD)),
        Span::styled(format!("{label}:"), Style::default().fg(AMBER)),
        Span::raw(" ".repeat(label_width.saturating_sub(label.len() + 1))),
        Span::styled(value, value_style),
        Span::raw(padding),
        Span::styled("│", Style::default().fg(GOLD)),
    ])
}

fn attachment_panel_height(app: &TuiApp, optional_height: u16) -> u16 {
    if app.attachments.is_empty() {
        0
    } else {
        // Attachments are composer metadata, not transcript content. Keep at
        // least a small transcript viewport before spending rows on them.
        let max_attachment_rows = optional_height.saturating_sub(3);
        (app.attachments.len() as u16)
            .clamp(1, 2)
            .min(max_attachment_rows)
    }
}

fn render_attachments(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let max_rows = area.height as usize;
    let mut lines = app
        .attachments
        .iter()
        .take(max_rows)
        .map(|attachment| Line::from(format_attachment_line(attachment)))
        .collect::<Vec<_>>();
    if app.attachments.len() > max_rows && max_rows > 0 {
        let hidden = app.attachments.len() - max_rows;
        if let Some(last) = lines.last_mut() {
            last.spans.push(Span::styled(
                format!(" · +{hidden} more (/attachments)"),
                Style::default().fg(QUIET),
            ));
        }
    }
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

fn transcript_visual_line_count(app: &TuiApp, width: u16, include_startup_card: bool) -> u16 {
    visual_line_count(
        &transcript_lines_for_render(app, Some(width), include_startup_card),
        width,
    )
}

fn visual_line_count(lines: &[Line<'_>], width: u16) -> u16 {
    let content_width = width.max(1) as usize;
    lines
        .iter()
        .map(|line| {
            let chars = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>()
                .max(1);
            chars.div_ceil(content_width)
        })
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

#[cfg(test)]
fn format_transcript_item(item: &TranscriptItem) -> Line<'_> {
    let lines = format_message_entry(item, false, false, MessageOutcome::Normal);
    let fallback = lines.first().cloned();
    lines
        .into_iter()
        .find(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref() == item.content.as_str())
        })
        .or(fallback)
        .unwrap_or_else(|| Line::from(""))
}

#[cfg(test)]
fn format_transcript_entry(
    entry: &TranscriptEntry,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
    outcome: MessageOutcome,
) -> Vec<Line<'static>> {
    format_transcript_entry_with_width(entry, selected, tool_output_verbosity, outcome, None)
}

fn format_transcript_entry_with_width(
    entry: &TranscriptEntry,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
    outcome: MessageOutcome,
    width: Option<u16>,
) -> Vec<Line<'static>> {
    match &entry.kind {
        TranscriptEntryKind::Message(item) => {
            format_message_entry_with_width(item, entry.collapsed, selected, outcome, width)
        }
        TranscriptEntryKind::ToolResult(tool) => format_tool_result_entry(
            tool,
            entry.collapsed,
            selected,
            tool_output_verbosity,
            width,
        ),
        TranscriptEntryKind::Log(message) => format_log_entry(message, entry.collapsed, selected),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageOutcome {
    Normal,
    Failed,
}

fn message_outcome(entries: &[TranscriptEntry], index: usize) -> MessageOutcome {
    let Some(TranscriptEntry {
        kind: TranscriptEntryKind::Message(item),
        ..
    }) = entries.get(index)
    else {
        return MessageOutcome::Normal;
    };
    if item.role != Role::User && item.role != Role::Assistant {
        return MessageOutcome::Normal;
    }

    for entry in entries.iter().skip(index + 1) {
        match &entry.kind {
            TranscriptEntryKind::Log(message) if is_failure_log(message) => {
                return MessageOutcome::Failed;
            }
            TranscriptEntryKind::Message(_) => return MessageOutcome::Normal,
            _ => {}
        }
    }
    MessageOutcome::Normal
}

fn is_failure_log(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("failed") || lower.contains("error") || lower.contains("cancelled")
}

fn dedupe_assistant_repeated_tool_output(
    app: &TuiApp,
    mut message: TranscriptItem,
) -> Option<TranscriptItem> {
    if message.role != Role::Assistant {
        return Some(message);
    }

    let content = assistant_content_without_repeated_tool_output(app, &message.content);
    message.content = content;
    (!message.content.trim().is_empty()).then_some(message)
}

fn assistant_content_without_repeated_tool_output(app: &TuiApp, content: &str) -> String {
    let mut content = content.to_string();
    let outputs = recent_shell_tool_outputs(app);
    for output in &outputs {
        if let Some(stripped) = strip_repeated_fenced_tool_output(&content, output) {
            content = stripped;
        }
        if let Some(stripped) = strip_repeated_open_fenced_tool_output(&content, output) {
            content = stripped;
        }
    }
    if !outputs.is_empty() && assistant_tool_followup_is_filler(&content) {
        return String::new();
    }
    content
}

fn pending_assistant_display_content(app: &TuiApp) -> Option<String> {
    if app.pending_assistant.trim().is_empty() {
        return None;
    }
    let content = assistant_content_without_repeated_tool_output(app, &app.pending_assistant);
    (!content.trim().is_empty()).then_some(content)
}

fn recent_shell_tool_outputs(app: &TuiApp) -> Vec<String> {
    let mut outputs = Vec::new();
    for entry in app.transcript.iter().rev() {
        match &entry.kind {
            TranscriptEntryKind::Message(item) if item.role == Role::User => break,
            TranscriptEntryKind::ToolResult(tool)
                if matches!(tool.result.tool_name.as_str(), "shell" | "verify") =>
            {
                if let Some(output) = shell_tool_output_text(tool) {
                    outputs.push(output);
                }
            }
            _ => {}
        }
    }
    outputs
}

fn shell_tool_output_text(tool: &ToolTranscript) -> Option<String> {
    let stdout = string_arg(&tool.result.content, "stdout").unwrap_or_default();
    let stderr = string_arg(&tool.result.content, "stderr").unwrap_or_default();
    let output = match (!stdout.trim().is_empty(), !stderr.trim().is_empty()) {
        (true, true) => format!("{stdout}\n{stderr}"),
        (true, false) => stdout,
        (false, true) => stderr,
        (false, false) => string_arg(&tool.result.content, "output").unwrap_or_default(),
    };
    (!output.trim().is_empty()).then_some(output)
}

fn strip_repeated_fenced_tool_output(content: &str, output: &str) -> Option<String> {
    let duplicate = normalize_duplicate_tool_output(output);
    if duplicate.len() < 80 && duplicate.lines().count() < 4 {
        return None;
    }

    let mut kept = Vec::new();
    let mut fence = Vec::new();
    let mut in_fence = false;
    let mut changed = false;

    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            if in_fence {
                fence.push(line.to_string());
                let body = fence[1..fence.len().saturating_sub(1)].join("\n");
                if fenced_block_repeats_tool_output(&body, &duplicate) {
                    changed = true;
                } else {
                    kept.append(&mut fence);
                }
                in_fence = false;
            } else {
                in_fence = true;
                fence.push(line.to_string());
            }
        } else if in_fence {
            fence.push(line.to_string());
        } else {
            kept.push(line.to_string());
        }
    }

    if in_fence {
        kept.append(&mut fence);
    }

    changed.then(|| tidy_stripped_assistant_text(kept.join("\n")))
}

fn strip_repeated_open_fenced_tool_output(content: &str, output: &str) -> Option<String> {
    let duplicate = normalize_duplicate_tool_output(output);
    if duplicate.len() < 80 && duplicate.lines().count() < 4 {
        return None;
    }

    let mut kept = Vec::new();
    let mut fence = Vec::new();
    let mut in_fence = false;

    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            if in_fence {
                fence.push(line.to_string());
                kept.append(&mut fence);
                in_fence = false;
            } else {
                in_fence = true;
                fence.push(line.to_string());
            }
        } else if in_fence {
            fence.push(line.to_string());
        } else {
            kept.push(line.to_string());
        }
    }

    if !in_fence {
        return None;
    }

    let body = fence[1..].join("\n");
    open_fenced_block_repeats_tool_output(&body, &duplicate)
        .then(|| tidy_stripped_assistant_text(kept.join("\n")))
}

fn fenced_block_repeats_tool_output(body: &str, duplicate: &str) -> bool {
    let body = normalize_duplicate_tool_output(body);
    !body.is_empty()
        && (body == duplicate
            || body.contains(duplicate)
            || duplicate.contains(&body)
            || tool_output_similarity_is_duplicate(&body, duplicate))
}

fn open_fenced_block_repeats_tool_output(body: &str, duplicate: &str) -> bool {
    let body = normalize_duplicate_tool_output(body);
    if body.len() < 40 && body.lines().count() < 2 {
        return false;
    }
    duplicate.starts_with(&body)
        || duplicate.contains(&body)
        || tool_output_similarity_is_duplicate(&body, duplicate)
}

fn normalize_duplicate_tool_output(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

fn tool_output_similarity_is_duplicate(body: &str, duplicate: &str) -> bool {
    let shorter_len = body.len().min(duplicate.len());
    if shorter_len < 80 {
        return false;
    }

    let body_lines = output_line_fingerprints(body);
    let duplicate_lines = output_line_fingerprints(duplicate);
    if body_lines.len() >= 3 && duplicate_lines.len() >= 3 {
        let shared_lines = body_lines.intersection(&duplicate_lines).count();
        let line_containment =
            shared_lines as f32 / body_lines.len().min(duplicate_lines.len()) as f32;
        if shared_lines >= 3 && line_containment >= 0.55 {
            return true;
        }
    }

    let body_tokens = output_similarity_tokens(body);
    let duplicate_tokens = output_similarity_tokens(duplicate);
    if body_tokens.len() < 12 || duplicate_tokens.len() < 12 {
        return false;
    }

    let shared = body_tokens.intersection(&duplicate_tokens).count();
    let smaller = body_tokens.len().min(duplicate_tokens.len());
    let union = body_tokens.len() + duplicate_tokens.len() - shared;
    let containment = shared as f32 / smaller as f32;
    let jaccard = shared as f32 / union as f32;

    shared >= 12 && containment >= 0.72 && jaccard >= 0.45
}

fn output_line_fingerprints(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(normalize_output_line)
        .filter(|line| line.len() >= 16)
        .collect()
}

fn normalize_output_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn output_similarity_tokens(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '.' && ch != '-')
        .map(str::trim)
        .filter(|token| token.len() >= 2)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn tidy_stripped_assistant_text(text: String) -> String {
    let mut text = text.trim().to_string();
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    if text.ends_with(':') {
        text.pop();
        text.push('.');
    }
    text
}

fn assistant_tool_followup_is_filler(content: &str) -> bool {
    let lines = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return true;
    }
    lines.iter().all(|line| tool_followup_filler_line(line))
}

fn tool_followup_filler_line(line: &str) -> bool {
    let normalized = normalize_followup_line(line);
    if normalized.is_empty() {
        return true;
    }
    let generic_offer = normalized.starts_with("if you want")
        || normalized.starts_with("i can also ")
        || normalized.starts_with("i can next ")
        || (normalized.starts_with("i can ") && normalized.contains(" next"))
        || normalized.starts_with("let me know")
        || normalized.starts_with("would you like");
    let generic_output_intro = normalized.starts_with("here is ")
        || normalized.starts_with("here's ")
        || normalized.starts_with("this is ")
        || normalized.starts_with("the output")
        || normalized.starts_with("output:")
        || normalized.starts_with("result:")
        || normalized.starts_with("results:");
    let generic_completion = matches!(
        normalized.as_str(),
        "done" | "completed" | "command completed" | "command completed successfully"
    );
    generic_offer || generic_output_intro || generic_completion
}

fn normalize_followup_line(line: &str) -> String {
    line.trim()
        .trim_matches(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '`' | '*' | '_' | '"' | '\'' | ':' | '.' | ',' | ';' | '-' | '—' | '–'
                )
        })
        .replace('’', "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[cfg(test)]
fn format_message_entry(
    item: &TranscriptItem,
    collapsed: bool,
    selected: bool,
    outcome: MessageOutcome,
) -> Vec<Line<'static>> {
    format_message_entry_with_width(item, collapsed, selected, outcome, None)
}

fn format_message_entry_with_width(
    item: &TranscriptItem,
    collapsed: bool,
    selected: bool,
    outcome: MessageOutcome,
    width: Option<u16>,
) -> Vec<Line<'static>> {
    if item.role == Role::User {
        return format_user_prompt_entry(item, selected, width);
    }
    if item.role == Role::Assistant {
        return format_assistant_message_entry(item, collapsed, selected, outcome);
    }
    let (action, color) = role_action(&item.role);
    let failed = outcome == MessageOutcome::Failed;
    let label_color = if failed { ERROR_RED } else { color };
    let action_color = if failed { ERROR_RED } else { color };
    let content_style = message_content_style(&item.role);
    if collapsed {
        return vec![action_line_styled(
            selected,
            "• ",
            label_color,
            action,
            action_color,
            collapsed_content_summary(&item.content),
            content_style,
        )];
    }
    action_text_lines_styled(
        selected,
        "• ",
        label_color,
        action,
        action_color,
        &item.content,
        content_style,
    )
}

fn format_user_prompt_entry(
    item: &TranscriptItem,
    _selected: bool,
    width: Option<u16>,
) -> Vec<Line<'static>> {
    let mut content = item.content.split('\n').collect::<Vec<_>>();
    if content.is_empty() {
        content.push("");
    }
    let mut lines = Vec::with_capacity(content.len() + 3);
    lines.push(user_prompt_blank_line(width));
    lines.extend(content.into_iter().enumerate().map(|(index, line)| {
        let marker = if index == 0 { "> " } else { "  " };
        user_prompt_content_line(marker, line, width)
    }));
    lines.push(user_prompt_blank_line(width));
    lines.push(Line::from(""));
    lines
}

fn user_prompt_blank_line(width: Option<u16>) -> Line<'static> {
    let marker = "  ";
    let surface_width = user_prompt_surface_width(marker, width).unwrap_or(1);
    Line::from(vec![
        user_prompt_marker_span(marker),
        Span::styled(" ".repeat(surface_width), Style::default().bg(PROMPT_BG)),
    ])
}

fn user_prompt_content_line(marker: &'static str, line: &str, width: Option<u16>) -> Line<'static> {
    let text_width = line.chars().count();
    let padding = user_prompt_surface_width(marker, width)
        .map(|surface_width| " ".repeat(surface_width.saturating_sub(text_width)))
        .unwrap_or_default();
    Line::from(vec![
        user_prompt_marker_span(marker),
        Span::styled(
            line.to_string(),
            Style::default().fg(Color::White).bg(PROMPT_BG),
        ),
        Span::styled(padding, Style::default().bg(PROMPT_BG)),
    ])
}

fn user_prompt_surface_width(marker: &str, width: Option<u16>) -> Option<usize> {
    width.map(|width| (width as usize).saturating_sub(marker.chars().count()))
}

fn user_prompt_marker_span(marker: &'static str) -> Span<'static> {
    let style = if marker.trim().is_empty() {
        Style::default().bg(PROMPT_BG)
    } else {
        Style::default().fg(GOLD).bg(PROMPT_BG)
    };
    Span::styled(marker, style)
}

fn format_assistant_message_entry(
    item: &TranscriptItem,
    collapsed: bool,
    selected: bool,
    outcome: MessageOutcome,
) -> Vec<Line<'static>> {
    let color = if outcome == MessageOutcome::Failed {
        ERROR_RED
    } else {
        SUCCESS_GREEN
    };
    let mut lines = if collapsed {
        vec![assistant_line(
            selected,
            assistant_static_span(color),
            collapsed_content_summary(&item.content),
            Style::default(),
        )]
    } else {
        assistant_text_lines(
            selected,
            assistant_static_span(color),
            &item.content,
            Style::default(),
        )
    };
    lines.push(Line::from(""));
    lines
}

fn collapsed_content_summary(content: &str) -> String {
    let lines = content.lines().collect::<Vec<_>>();
    if lines.len() > 1 {
        let first = compact_text(lines.first().copied().unwrap_or_default(), 120);
        format!("{first} … +{} lines (Ctrl-E to expand)", lines.len() - 1)
    } else {
        compact_text(content, 160)
    }
}

fn format_tool_result_entry(
    tool: &ToolTranscript,
    collapsed: bool,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
    width: Option<u16>,
) -> Vec<Line<'static>> {
    let (marker, action) = tool_result_action(tool);
    let color = tool_result_display_color(tool);
    let summary_spans = tool_result_summary_spans(tool);
    if collapsed {
        let mut lines = vec![action_line_spans(
            selected,
            marker,
            color,
            action,
            color,
            summary_spans,
        )];
        lines.extend(collapsed_tool_preview_lines(tool, width));
        return lines;
    }
    let mut lines = vec![action_line_spans(
        selected,
        marker,
        color,
        action,
        color,
        summary_spans,
    )];
    lines.extend(expanded_tool_detail_lines(tool, tool_output_verbosity));
    lines
}

fn collapsed_tool_preview_lines(tool: &ToolTranscript, width: Option<u16>) -> Vec<Line<'static>> {
    if let Some(lines) = collapsed_shell_preview_lines(tool, width) {
        return lines;
    }
    if !matches!(tool.result.status, ToolStatus::Success)
        || !matches!(tool.result.tool_name.as_str(), "apply_patch" | "write_file")
    {
        return Vec::new();
    }
    let Some(file) = edit_changed_files(tool).into_iter().find(|file| {
        file.patch
            .as_ref()
            .is_some_and(|patch| !patch.trim().is_empty())
    }) else {
        return Vec::new();
    };
    let Some(patch) = file.patch.as_deref() else {
        return Vec::new();
    };
    let mut lines = vec![detail_line(false, QUIET, format!("diff {}", file.path))];
    lines.extend(render_diff_patch_preview_lines(patch, 10));
    lines
}

fn format_log_entry(message: &str, collapsed: bool, selected: bool) -> Vec<Line<'static>> {
    let color = log_color(message);
    if collapsed {
        let preview = compact_text(message, 140);
        return vec![detail_line(selected, color, preview)];
    }
    detail_text_lines(selected, color, message)
}

fn role_action(role: &Role) -> (&'static str, Color) {
    match role {
        Role::User => ("Asked", AMBER),
        Role::Assistant => ("Answered", SUCCESS_GREEN),
        Role::System => ("Noted", GOLD),
    }
}

fn message_content_style(role: &Role) -> Style {
    match role {
        Role::User => Style::default().fg(Color::White).bg(PROMPT_BG),
        Role::Assistant | Role::System => Style::default(),
    }
}

fn log_color(message: &str) -> Color {
    if is_failure_log(message) {
        ERROR_RED
    } else {
        GOLD
    }
}

fn action_line_styled(
    selected: bool,
    label: &'static str,
    label_color: Color,
    action: &'static str,
    action_color: Color,
    content: impl Into<String>,
    content_style: Style,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let content = content.into();
    let spacer = if content.is_empty() { "" } else { " " };
    Line::from(vec![
        Span::raw(marker),
        Span::styled(
            label,
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            action,
            Style::default()
                .fg(action_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(spacer),
        Span::styled(content, content_style),
    ])
}

fn action_line_spans(
    selected: bool,
    label: &'static str,
    label_color: Color,
    action: &'static str,
    action_color: Color,
    content: Vec<Span<'static>>,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let mut spans = vec![
        Span::raw(marker),
        Span::styled(
            label,
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            action,
            Style::default()
                .fg(action_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !content.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(content);
    }
    Line::from(spans)
}

fn detail_line(selected: bool, color: Color, content: impl Into<String>) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    Line::from(vec![
        Span::raw(marker),
        Span::styled(
            "└ ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(content.into(), Style::default().fg(QUIET)),
    ])
}

fn action_text_lines_styled(
    selected: bool,
    label: &'static str,
    label_color: Color,
    action: &'static str,
    action_color: Color,
    content: &str,
    content_style: Style,
) -> Vec<Line<'static>> {
    if content.is_empty() {
        return vec![action_line_styled(
            selected,
            label,
            label_color,
            action,
            action_color,
            "",
            content_style,
        )];
    }
    content
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                action_line_styled(
                    selected,
                    label,
                    label_color,
                    action,
                    action_color,
                    line.to_string(),
                    content_style,
                )
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line.to_string(), content_style),
                ])
            }
        })
        .collect()
}

fn assistant_line(
    selected: bool,
    status: Span<'static>,
    content: impl Into<String>,
    content_style: Style,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let content = content.into();
    let spacer = if content.is_empty() { "" } else { " " };
    Line::from(vec![
        Span::raw(marker),
        status,
        Span::raw(spacer),
        Span::styled(content, content_style),
    ])
}

fn assistant_text_lines(
    selected: bool,
    status: Span<'static>,
    content: &str,
    content_style: Style,
) -> Vec<Line<'static>> {
    if content.is_empty() {
        return vec![assistant_line(selected, status, "", content_style)];
    }
    render::markdown::render_markdown(content)
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            for span in &mut line.spans {
                span.style = content_style.patch(span.style);
            }
            if index == 0 {
                let marker = if selected { "> " } else { "  " };
                let mut spans = vec![Span::raw(marker), status.clone(), Span::raw(" ")];
                spans.extend(line.spans);
                Line::from(spans)
            } else {
                let mut spans = vec![Span::raw("    ")];
                spans.extend(line.spans);
                Line::from(spans)
            }
        })
        .collect()
}

fn detail_text_lines(selected: bool, color: Color, content: &str) -> Vec<Line<'static>> {
    if content.is_empty() {
        return vec![detail_line(selected, color, "")];
    }
    content
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                detail_line(selected, color, line.to_string())
            } else {
                Line::from(format!("    {line}"))
            }
        })
        .collect()
}

fn tool_result_summary(tool: &ToolTranscript) -> String {
    spans_plain_text(&tool_result_summary_spans(tool))
}

fn tool_result_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let result = &tool.result;
    if is_invalid_argument_result(result) {
        let mut spans = vec![Span::styled(
            result.tool_name.clone(),
            Style::default().fg(Color::White),
        )];
        if let Some(call) = tool.call.as_ref() {
            let label = tool_call_label(call);
            if label != result.tool_name {
                spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
                spans.push(Span::styled(label, Style::default().fg(QUIET)));
            }
        }
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            tool_result_error_detail(result),
            Style::default().fg(QUIET),
        ));
        if tool.repeat_count > 1 {
            spans.push(Span::styled(
                format!(" ({}x)", tool.repeat_count),
                Style::default().fg(QUIET),
            ));
        }
        return spans;
    }
    let mut spans = match result.tool_name.as_str() {
        "shell" | "verify" => shell_tool_summary_spans(tool),
        "decl_search" => decl_search_summary_spans(tool),
        "definition_search" | "reference_search" | "symbol_context" | "hierarchy"
        | "upstream_flow" | "downstream_flow" => semantic_tool_summary_spans(tool),
        "repo_map" => repo_map_summary_spans(tool),
        "grep" | "glob" | "read_file" | "read_slice" | "read_tool_output" => {
            read_search_summary_spans(tool)
        }
        "diff_context" => diff_context_summary_spans(tool),
        "plan_patch" => plan_patch_summary_spans(tool),
        "apply_patch" | "write_file" => edit_summary_spans(tool),
        "webfetch" | "websearch" => web_summary_spans(tool),
        _ => vec![Span::styled(
            result.tool_name.clone(),
            Style::default().fg(Color::White),
        )],
    };
    if tool_result_not_run(tool) {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            tool_result_error_detail(result),
            Style::default().fg(QUIET),
        ));
        if tool.repeat_count > 1 {
            spans.push(Span::styled(
                format!(" ({}x)", tool.repeat_count),
                Style::default().fg(QUIET),
            ));
        }
        return spans;
    }
    match result.status {
        ToolStatus::Error | ToolStatus::Stale => {
            spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
            spans.push(Span::styled(
                tool_result_error_detail(result),
                Style::default().fg(QUIET),
            ));
        }
        ToolStatus::Denied => {
            spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
            spans.push(Span::styled(
                tool_result_denied_detail(result),
                Style::default().fg(QUIET),
            ));
        }
        ToolStatus::Cancelled => {
            spans.push(Span::styled(" · cancelled", Style::default().fg(QUIET)));
        }
        ToolStatus::Success => {}
    }
    if tool.repeat_count > 1 {
        spans.push(Span::styled(
            format!(" ({}x)", tool.repeat_count),
            Style::default().fg(QUIET),
        ));
    }
    spans
}

fn spans_plain_text(spans: &[Span<'_>]) -> String {
    spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn shell_tool_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let command = tool
        .call
        .as_ref()
        .and_then(|call| string_arg(&call.arguments, "command"))
        .or_else(|| string_arg(&tool.result.content, "command"))
        .unwrap_or_else(|| tool.result.tool_name.clone());
    command_spans(&command)
}

fn shell_result_is_exploration(tool: &ToolTranscript) -> bool {
    if !matches!(tool.result.tool_name.as_str(), "shell" | "verify") {
        return false;
    }
    matches!(
        tool.result.content["policy"]["capability"].as_str(),
        Some("read" | "search")
    )
}

fn decl_search_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let query = tool
        .call
        .as_ref()
        .and_then(|call| string_arg(&call.arguments, "query"))
        .or_else(|| string_arg(&tool.result.content, "query"));
    let language = tool
        .call
        .as_ref()
        .and_then(|call| string_arg(&call.arguments, "language"))
        .or_else(|| string_arg(&tool.result.content, "language"));
    let kind = tool
        .call
        .as_ref()
        .and_then(|call| string_arg(&call.arguments, "kind"))
        .or_else(|| string_arg(&tool.result.content, "kind"));
    let mut label = String::new();
    if let Some(language) = language {
        label.push_str(&language);
        label.push(' ');
    }
    if let Some(kind) = kind {
        label.push_str(&kind_label(&kind));
        label.push(' ');
    }
    label.push_str("declarations");
    if let Some(query) = query {
        label.push_str(" for ");
        label.push_str(&query);
    }
    let mut spans = vec![Span::styled(label, Style::default().fg(Color::White))];
    if let Some(total) = number_field(&tool.result.content, "total_matches")
        .or_else(|| number_field(&tool.result.content, "returned_matches"))
    {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{total} matches"),
            Style::default().fg(GOLD),
        ));
    }
    if tool.result.content["truncated"].as_bool().unwrap_or(false) {
        spans.push(Span::styled(
            " · more available",
            Style::default().fg(QUIET),
        ));
    }
    spans
}

fn semantic_tool_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let label = tool_call_label_or_name(tool);
    let mut spans = vec![Span::styled(label, Style::default().fg(Color::White))];
    if let Some(matches) = number_field(&tool.result.content, "total_matches")
        .or_else(|| number_field(&tool.result.content, "returned_matches"))
        .or_else(|| {
            tool.result.content["packets"]
                .as_array()
                .map(|items| items.len() as u64)
        })
    {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{matches} matches"),
            Style::default().fg(GOLD),
        ));
    }
    spans
}

fn repo_map_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("repo map", Style::default().fg(Color::White))];
    if let Some(files) = tool.result.content["stats"]["files"].as_u64() {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{files} files"),
            Style::default().fg(GOLD),
        ));
    }
    if let Some(symbols) = tool.result.content["stats"]["symbols"].as_u64() {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{symbols} symbols"),
            Style::default().fg(GOLD),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn read_search_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    match tool.result.tool_name.as_str() {
        "glob" => glob_summary_spans(tool),
        "read_file" | "read_slice" => read_file_summary_spans(tool),
        "read_tool_output" => read_tool_output_summary_spans(tool),
        _ => grep_summary_spans(tool),
    }
}

fn grep_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let label = tool_call_label_or_name(tool);
    let mut spans = vec![Span::styled(label, Style::default().fg(Color::White))];
    if let Some(matches) = number_field(&tool.result.content, "matches_returned")
        .or_else(|| number_field(&tool.result.content, "count"))
        .or_else(|| {
            tool.result.content["matches"]
                .as_array()
                .map(|items| items.len() as u64)
        })
        .or_else(|| {
            tool.result.content["paths"]
                .as_array()
                .map(|items| items.len() as u64)
        })
    {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{matches} matches"),
            Style::default().fg(GOLD),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn glob_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let pattern = tool
        .call
        .as_ref()
        .and_then(|call| string_arg(&call.arguments, "pattern"))
        .or_else(|| string_arg(&tool.result.content["metadata"], "pattern"));
    let label = pattern
        .map(|pattern| format!("list files matching {pattern}"))
        .unwrap_or_else(|| "list files".to_string());
    let mut spans = vec![Span::styled(label, Style::default().fg(Color::White))];
    if let Some(paths) = tool.result.content["paths"]
        .as_array()
        .map(|items| items.len() as u64)
    {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{paths} paths"),
            Style::default().fg(GOLD),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn read_file_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let label = tool_call_label_or_name(tool);
    let mut spans = vec![Span::styled(label, Style::default().fg(Color::White))];
    if let Some(bytes) = number_field(&tool.result.content, "bytes_returned") {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(format_bytes(bytes), Style::default().fg(GOLD)));
    } else if let Some(ranges) = tool.result.content["ranges"]
        .as_array()
        .map(|items| items.len() as u64)
    {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{ranges} ranges"),
            Style::default().fg(GOLD),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn read_tool_output_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        "expand saved tool output",
        Style::default().fg(Color::White),
    )];
    if let Some(bytes) = number_field(&tool.result.content, "bytes_returned") {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(format_bytes(bytes), Style::default().fg(GOLD)));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn edit_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let files = edit_changed_files(tool);
    let label = if files.is_empty() {
        tool_call_label_or_name(tool)
    } else if files.len() == 1 {
        files[0].path.clone()
    } else {
        format!("{} files", files.len())
    };
    let mut spans = vec![Span::styled(label, Style::default().fg(Color::White))];
    let additions = files.iter().map(|file| file.additions).sum::<u64>();
    let deletions = files.iter().map(|file| file.deletions).sum::<u64>();
    if additions > 0 || deletions > 0 {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("+{additions} -{deletions}"),
            Style::default().fg(QUIET),
        ));
    } else if let Some(count) = number_field(&tool.result.content, "matches") {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{count} matches"),
            Style::default().fg(GOLD),
        ));
    }
    spans
}

#[derive(Debug)]
struct EditChangedFile {
    path: String,
    additions: u64,
    deletions: u64,
    patch: Option<String>,
    patch_truncated: bool,
}

fn edit_changed_files(tool: &ToolTranscript) -> Vec<EditChangedFile> {
    let mut files = tool.result.content["checkpoint"]["files"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(EditChangedFile {
                        path: item["path"].as_str()?.to_string(),
                        additions: item["additions"].as_u64().unwrap_or(0),
                        deletions: item["deletions"].as_u64().unwrap_or(0),
                        patch: item["patch"].as_str().map(ToString::to_string),
                        patch_truncated: item["patch_truncated"].as_bool().unwrap_or(false),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if files.is_empty() {
        if let Some(items) = tool.result.content["files"].as_array() {
            files.extend(items.iter().filter_map(|item| {
                Some(EditChangedFile {
                    path: item["path"].as_str()?.to_string(),
                    additions: 0,
                    deletions: 0,
                    patch: None,
                    patch_truncated: false,
                })
            }));
        } else if let Some(path) = string_arg(&tool.result.content, "path") {
            files.push(EditChangedFile {
                path,
                additions: 0,
                deletions: 0,
                patch: None,
                patch_truncated: false,
            });
        }
    }
    files
}

fn diff_context_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let mode = string_arg(&tool.result.content, "mode")
        .map(|mode| format!("diff context ({mode})"))
        .unwrap_or_else(|| "diff context".to_string());
    let mut spans = vec![Span::styled(mode, Style::default().fg(Color::White))];
    let files = tool.result.content["summary"]["files_changed"]
        .as_u64()
        .or_else(|| {
            tool.result.content["files"]
                .as_array()
                .map(|items| items.len() as u64)
        });
    if let Some(files) = files {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{files} files"),
            Style::default().fg(GOLD),
        ));
    }
    let additions = tool.result.content["summary"]["additions"].as_u64();
    let deletions = tool.result.content["summary"]["deletions"].as_u64();
    if additions.unwrap_or(0) > 0 || deletions.unwrap_or(0) > 0 {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("+{} -{}", additions.unwrap_or(0), deletions.unwrap_or(0)),
            Style::default().fg(QUIET),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn plan_patch_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let objective = tool
        .call
        .as_ref()
        .and_then(|call| string_arg(&call.arguments, "objective"))
        .or_else(|| string_arg(&tool.result.content, "objective"));
    let label = objective
        .map(|objective| format!("plan patch for {}", compact_text(&objective, 64)))
        .unwrap_or_else(|| "plan patch".to_string());
    let mut spans = vec![Span::styled(label, Style::default().fg(Color::White))];
    if let Some(symbols) = tool.result.content["symbols"]
        .as_array()
        .map(|items| items.len() as u64)
        .filter(|count| *count > 0)
    {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{symbols} symbols"),
            Style::default().fg(GOLD),
        ));
    }
    if let Some(paths) = tool.result.content["impact"]["neighborhood_paths"]
        .as_array()
        .map(|items| items.len() as u64)
        .filter(|count| *count > 0)
    {
        spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        spans.push(Span::styled(
            format!("{paths} paths"),
            Style::default().fg(GOLD),
        ));
    }
    if tool.result.content["graph_available"].as_bool() == Some(false) {
        spans.push(Span::styled(
            " · graph unavailable",
            Style::default().fg(QUIET),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn web_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    vec![Span::styled(
        tool_call_label_or_name(tool),
        Style::default().fg(Color::White),
    )]
}

fn tool_call_label_or_name(tool: &ToolTranscript) -> String {
    tool.call
        .as_ref()
        .map(tool_call_label)
        .unwrap_or_else(|| tool.result.tool_name.clone())
}

fn tool_call_label(call: &ToolCall) -> String {
    match call.name.as_str() {
        "shell" | "verify" => string_arg(&call.arguments, "command")
            .or_else(|| string_arg(&call.arguments, "description"))
            .unwrap_or_else(|| call.name.clone()),
        "decl_search" => {
            let language = string_arg(&call.arguments, "language");
            let kind = string_arg(&call.arguments, "kind").map(|value| kind_label(&value));
            let query = string_arg(&call.arguments, "query");
            let mut label = String::new();
            if let Some(language) = language {
                label.push_str(&language);
                label.push(' ');
            }
            if let Some(kind) = kind {
                label.push_str(&kind);
                label.push(' ');
            }
            label.push_str("declarations");
            if let Some(query) = query {
                label.push_str(" for ");
                label.push_str(&query);
            }
            label
        }
        "repo_map" => "repo map".to_string(),
        "definition_search" => string_arg(&call.arguments, "query")
            .map(|query| format!("definition search for {query}"))
            .unwrap_or_else(|| "definition search".to_string()),
        "reference_search" => string_arg(&call.arguments, "query")
            .or_else(|| string_arg(&call.arguments, "symbol_id"))
            .map(|query| format!("reference search for {query}"))
            .unwrap_or_else(|| "reference search".to_string()),
        "symbol_context" => string_arg(&call.arguments, "query")
            .map(|query| format!("symbol context for {query}"))
            .unwrap_or_else(|| "symbol context".to_string()),
        "grep" => string_arg(&call.arguments, "query")
            .or_else(|| string_arg(&call.arguments, "pattern"))
            .map(|query| format!("grep {query}"))
            .unwrap_or_else(|| "grep".to_string()),
        "glob" => string_arg(&call.arguments, "pattern")
            .map(|pattern| format!("glob {pattern}"))
            .unwrap_or_else(|| "glob".to_string()),
        "read_file" | "read_slice" => string_arg(&call.arguments, "path")
            .map(|path| format!("read {path}"))
            .unwrap_or_else(|| call.name.clone()),
        "read_tool_output" => "expand previous tool output".to_string(),
        "diff_context" => "diff context".to_string(),
        "plan_patch" => string_arg(&call.arguments, "objective")
            .map(|objective| format!("plan patch for {}", compact_text(&objective, 64)))
            .unwrap_or_else(|| "plan patch".to_string()),
        "apply_patch" => "apply patch".to_string(),
        "write_file" => string_arg(&call.arguments, "path")
            .map(|path| format!("write {path}"))
            .unwrap_or_else(|| "write file".to_string()),
        "webfetch" => string_arg(&call.arguments, "url")
            .map(|url| format!("fetch {url}"))
            .unwrap_or_else(|| "web fetch".to_string()),
        "websearch" => string_arg(&call.arguments, "query")
            .map(|query| format!("web search {query}"))
            .unwrap_or_else(|| "web search".to_string()),
        _ => call.name.clone(),
    }
}

fn active_tool_spans(call: &ToolCall) -> Vec<Span<'static>> {
    let action = active_tool_action(&call.name);
    let mut spans = vec![Span::styled(
        action,
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    )];
    if matches!(call.name.as_str(), "shell" | "verify")
        && let Some(command) = string_arg(&call.arguments, "command")
    {
        spans.extend(command_spans(&command));
    } else {
        spans.push(Span::styled(
            tool_call_label(call),
            Style::default().fg(Color::White),
        ));
    }
    spans
}

fn is_control_tool_name(name: &str) -> bool {
    matches!(name, "update_task_state" | "load_tool_schema")
}

fn active_tool_action(tool_name: &str) -> &'static str {
    match tool_name {
        "plan_patch" => "Planning ",
        "apply_patch" | "write_file" => "Editing ",
        name if is_exploration_tool(name) => "Exploring ",
        _ => "Running ",
    }
}

fn expanded_tool_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    match tool.result.tool_name.as_str() {
        "shell" | "verify" => expanded_shell_detail_lines(tool, verbosity),
        "decl_search" => expanded_decl_search_detail_lines(tool, verbosity),
        "repo_map" => expanded_repo_map_detail_lines(tool),
        "diff_context" => expanded_diff_context_detail_lines(tool),
        "plan_patch" => expanded_plan_patch_detail_lines(tool),
        "apply_patch" | "write_file" => expanded_edit_detail_lines(tool, verbosity),
        "grep" | "glob" | "read_file" | "read_slice" | "read_tool_output" => {
            expanded_read_search_detail_lines(tool, verbosity)
        }
        _ => expanded_generic_tool_detail_lines(tool, verbosity),
    }
}

fn expanded_shell_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let mut lines = shell_output_block_lines(tool, None);
    if let Some(command) = tool
        .call
        .as_ref()
        .and_then(|call| string_arg(&call.arguments, "command"))
        .or_else(|| string_arg(&tool.result.content, "command"))
        && lines.is_empty()
    {
        lines.push(detail_spans_line(command_spans(&command)));
    }
    if tool.result.status != ToolStatus::Success
        && let Some(workdir) = string_arg(&tool.result.content, "workdir")
    {
        lines.push(detail_line(false, QUIET, format!("cwd {workdir}")));
    }
    if tool.result.status != ToolStatus::Success
        && let Some(exit_code) = tool
            .result
            .content
            .get("exit_code")
            .and_then(|value| value.as_i64())
    {
        lines.push(detail_line(false, QUIET, format!("exit {exit_code}")));
    }
    if tool.result.status != ToolStatus::Success {
        lines.extend(output_block_lines(
            "stdout",
            string_arg(&tool.result.content, "stdout")
                .as_deref()
                .unwrap_or(""),
            verbosity,
        ));
        lines.extend(output_block_lines(
            "stderr",
            string_arg(&tool.result.content, "stderr")
                .as_deref()
                .unwrap_or(""),
            verbosity,
        ));
    }
    if lines.is_empty() {
        lines.extend(expanded_generic_tool_detail_lines(tool, verbosity));
    }
    lines
}

fn collapsed_shell_preview_lines(
    tool: &ToolTranscript,
    width: Option<u16>,
) -> Option<Vec<Line<'static>>> {
    if !matches!(tool.result.tool_name.as_str(), "shell" | "verify")
        || tool.result.status != ToolStatus::Success
    {
        return None;
    }
    let mut lines = shell_output_block_lines(tool, Some(SHELL_COLLAPSED_OUTPUT_PREVIEW_LINES));
    if lines.is_empty() {
        None
    } else {
        lines.insert(0, transcript_separator_line(width));
        lines.push(transcript_separator_line(width));
        Some(lines)
    }
}

fn shell_output_block_lines(
    tool: &ToolTranscript,
    preview_limit: Option<usize>,
) -> Vec<Line<'static>> {
    let stdout = string_arg(&tool.result.content, "stdout").unwrap_or_default();
    let stderr = string_arg(&tool.result.content, "stderr").unwrap_or_default();
    let output = match (!stdout.trim().is_empty(), !stderr.trim().is_empty()) {
        (true, true) => format!("{stdout}\n{stderr}"),
        (true, false) => stdout,
        (false, true) => stderr,
        (false, false) => return Vec::new(),
    };
    let command = tool
        .call
        .as_ref()
        .and_then(|call| string_arg(&call.arguments, "command"))
        .or_else(|| string_arg(&tool.result.content, "command"))
        .unwrap_or_else(|| tool.result.tool_name.clone());
    let workdir = string_arg(&tool.result.content, "workdir").unwrap_or_else(|| ".".to_string());
    let limit = preview_limit.unwrap_or(usize::MAX);
    let mut lines = vec![shell_output_title_line(&command, &workdir)];
    lines.extend(head_tail_lines(&output, limit).into_iter().map(|line| {
        if line.truncated_marker {
            detail_line(false, QUIET, line.text)
        } else {
            shell_output_line(&line.text)
        }
    }));
    lines
}

fn shell_output_title_line(command: &str, workdir: &str) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            "└ ",
            Style::default().fg(QUIET).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(command_spans(command));
    spans.push(Span::styled(" in ", Style::default().fg(QUIET)));
    spans.push(Span::styled(
        workdir.to_string(),
        Style::default().fg(Color::White),
    ));
    spans.push(Span::styled(":", Style::default().fg(QUIET)));
    Line::from(spans)
}

fn shell_output_line(content: &str) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(styled_output_spans(content));
    Line::from(spans)
}

fn transcript_separator_line(width: Option<u16>) -> Line<'static> {
    let width = width.unwrap_or(96).max(8) as usize;
    Line::from(Span::styled("─".repeat(width), Style::default().fg(QUIET)))
}

fn expanded_decl_search_detail_lines(
    tool: &ToolTranscript,
    _verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(total) = number_field(&tool.result.content, "total_matches") {
        lines.push(detail_line(false, QUIET, format!("total matches {total}")));
    }
    if let Some(returned) = number_field(&tool.result.content, "returned_matches") {
        lines.push(detail_line(
            false,
            QUIET,
            format!("shown matches {returned}"),
        ));
    }
    if let Some(languages) = compact_json_object(&tool.result.content["counts_by_language"]) {
        lines.push(detail_line(false, QUIET, format!("languages {languages}")));
    }
    if let Some(kinds) = compact_json_object(&tool.result.content["counts_by_kind"]) {
        lines.push(detail_line(false, QUIET, format!("kinds {kinds}")));
    }
    lines
}

fn expanded_repo_map_detail_lines(tool: &ToolTranscript) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(files) = tool.result.content["stats"]["files"].as_u64() {
        lines.push(detail_line(false, QUIET, format!("files {files}")));
    }
    if let Some(symbols) = tool.result.content["stats"]["symbols"].as_u64() {
        lines.push(detail_line(false, QUIET, format!("symbols {symbols}")));
    }
    if let Some(languages) = compact_json_object(&tool.result.content["languages"]) {
        lines.push(detail_line(false, QUIET, format!("languages {languages}")));
    }
    lines
}

fn expanded_diff_context_detail_lines(tool: &ToolTranscript) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(mode) = string_arg(&tool.result.content, "mode") {
        lines.push(detail_line(false, QUIET, format!("mode {mode}")));
    }
    let summary = &tool.result.content["summary"];
    if let Some(files) = summary["files_changed"].as_u64() {
        let additions = summary["additions"].as_u64().unwrap_or(0);
        let deletions = summary["deletions"].as_u64().unwrap_or(0);
        lines.push(detail_line(
            false,
            QUIET,
            format!("changed {files} files, +{additions} -{deletions}"),
        ));
    }
    let diff_files = diff_context_files(tool);
    if diff_files.is_empty() {
        lines.extend(path_detail_lines(&tool.result.content["files"], "path", 6));
    } else {
        for file in diff_files {
            let mut summary = format!("file {}", file.path);
            if file.additions > 0 || file.deletions > 0 {
                summary.push_str(&format!(" +{} -{}", file.additions, file.deletions));
            }
            if file.patch_truncated {
                summary.push_str(" · diff truncated");
            }
            lines.push(detail_line(false, QUIET, summary));
            if file
                .patch
                .as_ref()
                .is_some_and(|patch| !patch.trim().is_empty())
            {
                lines.extend(
                    render::diff::render_diff_file(&file)
                        .into_iter()
                        .map(detail_rendered_line),
                );
            }
        }
    }
    lines
}

fn diff_context_files(tool: &ToolTranscript) -> Vec<squeezy_vcs::DiffFile> {
    tool.result.content["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|file| serde_json::from_value(file.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn expanded_plan_patch_detail_lines(tool: &ToolTranscript) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(objective) = string_arg(&tool.result.content, "objective") {
        lines.push(detail_line(false, QUIET, format!("objective {objective}")));
    }
    if let Some(plan_id) = string_arg(&tool.result.content, "plan_id") {
        lines.push(detail_line(false, QUIET, format!("plan {plan_id}")));
    }
    if let Some(symbols) = tool.result.content["symbols"].as_array() {
        lines.push(detail_line(
            false,
            QUIET,
            format!("symbols {}", symbols.len()),
        ));
    }
    if let Some(paths) = tool.result.content["impact"]["neighborhood_paths"].as_array() {
        lines.push(detail_line(false, QUIET, format!("paths {}", paths.len())));
        lines.extend(paths.iter().take(5).filter_map(|path| {
            path.as_str()
                .map(|path| detail_line(false, QUIET, format!("path {path}")))
        }));
    }
    if let Some(next) = tool.result.content["next_action"]["reason"].as_str() {
        lines.push(detail_line(false, QUIET, format!("next {next}")));
    }
    lines
}

fn expanded_edit_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let files = edit_changed_files(tool);
    for file in files {
        let mut summary = format!("file {}", file.path);
        if file.additions > 0 || file.deletions > 0 {
            summary.push_str(&format!(" +{} -{}", file.additions, file.deletions));
        }
        if file.patch_truncated {
            summary.push_str(" · diff truncated");
        }
        lines.push(detail_line(false, QUIET, summary));
        if let Some(patch) = file.patch.as_deref().filter(|patch| !patch.is_empty()) {
            lines.push(detail_line(false, QUIET, "diff"));
            lines.extend(render_diff_patch_full_lines(patch));
        }
    }
    if let Some(matches) = number_field(&tool.result.content, "matches") {
        lines.push(detail_line(false, QUIET, format!("matches {matches}")));
    }
    if let Some(contexts) = tool.result.content["match_contexts"].as_array() {
        lines.extend(contexts.iter().take(5).filter_map(|context| {
            let index = context["match_index"].as_u64()?;
            let line = context["line"].as_u64()?;
            let preview = context["preview"].as_str()?;
            Some(detail_line(
                false,
                QUIET,
                format!("match {index} line {line}: {preview}"),
            ))
        }));
    }
    if lines.is_empty() {
        expanded_generic_tool_detail_lines(tool, verbosity)
    } else {
        lines
    }
}

fn expanded_read_search_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let lines = match tool.result.tool_name.as_str() {
        "glob" => expanded_glob_detail_lines(tool),
        "grep" => expanded_grep_detail_lines(tool),
        "read_file" | "read_slice" => expanded_read_file_detail_lines(tool, verbosity),
        "read_tool_output" => expanded_read_tool_output_detail_lines(tool, verbosity),
        _ => expanded_generic_tool_detail_lines(tool, verbosity),
    };
    if lines.is_empty() {
        expanded_generic_tool_detail_lines(tool, verbosity)
    } else {
        lines
    }
}

fn expanded_glob_detail_lines(tool: &ToolTranscript) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(pattern) = string_arg(&tool.result.content["metadata"], "pattern") {
        lines.push(detail_line(false, QUIET, format!("pattern {pattern}")));
    }
    if let Some(path) = string_arg(&tool.result.content["metadata"], "path") {
        lines.push(detail_line(false, QUIET, format!("root {path}")));
    }
    if let Some(paths) = tool.result.content["paths"].as_array() {
        lines.push(detail_line(false, QUIET, format!("paths {}", paths.len())));
        lines.extend(paths.iter().take(8).filter_map(|path| {
            path.as_str()
                .map(|path| detail_line(false, QUIET, format!("path {path}")))
        }));
    }
    lines
}

fn expanded_grep_detail_lines(tool: &ToolTranscript) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(pattern) = string_arg(&tool.result.content["metadata"], "pattern") {
        lines.push(detail_line(false, QUIET, format!("pattern {pattern}")));
    }
    if let Some(path) = string_arg(&tool.result.content["metadata"], "path") {
        lines.push(detail_line(false, QUIET, format!("root {path}")));
    }
    if let Some(count) = number_field(&tool.result.content, "count") {
        lines.push(detail_line(false, QUIET, format!("matches {count}")));
    }
    lines.extend(path_detail_lines(&tool.result.content["paths"], "", 8));
    if let Some(matches) = tool.result.content["matches"].as_array() {
        for item in matches.iter().take(6) {
            let path = item["path"].as_str().unwrap_or("?");
            let line = item["line"].as_u64().unwrap_or(0);
            let text = item["text"].as_str().unwrap_or_default();
            lines.push(detail_line(
                false,
                QUIET,
                format!("{path}:{line} {}", compact_text(text, 100)),
            ));
        }
    }
    lines
}

fn expanded_read_file_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(path) = string_arg(&tool.result.content, "path") {
        lines.push(detail_line(false, QUIET, format!("path {path}")));
    }
    if let Some(bytes) = number_field(&tool.result.content, "bytes_returned") {
        let total = number_field(&tool.result.content, "total_bytes").unwrap_or(bytes);
        lines.push(detail_line(
            false,
            QUIET,
            format!("bytes {} of {}", format_bytes(bytes), format_bytes(total)),
        ));
    }
    if let Some(ranges) = tool.result.content["ranges"].as_array() {
        lines.push(detail_line(
            false,
            QUIET,
            format!("ranges {}", ranges.len()),
        ));
    }
    if let Some(content) = string_arg(&tool.result.content, "content") {
        lines.extend(output_block_lines("content", &content, verbosity));
    }
    lines
}

fn expanded_read_tool_output_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(handle) = string_arg(&tool.result.content, "handle") {
        lines.push(detail_line(false, QUIET, format!("handle {handle}")));
    }
    if let Some(bytes) = number_field(&tool.result.content, "bytes_returned") {
        let total = number_field(&tool.result.content, "total_bytes").unwrap_or(bytes);
        lines.push(detail_line(
            false,
            QUIET,
            format!("bytes {} of {}", format_bytes(bytes), format_bytes(total)),
        ));
    }
    if let Some(content) = string_arg(&tool.result.content, "content") {
        lines.extend(output_block_lines("content", &content, verbosity));
    }
    lines
}

fn expanded_generic_tool_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let preview = preview_tool_result(&tool.result, verbosity);
    output_block_lines("details", &preview, verbosity)
}

fn render_diff_patch_preview_lines(patch: &str, limit: usize) -> Vec<Line<'static>> {
    render::diff::render_patch_preview_lines(patch, limit)
        .into_iter()
        .map(detail_rendered_line)
        .collect()
}

fn render_diff_patch_full_lines(patch: &str) -> Vec<Line<'static>> {
    render::diff::render_patch_full_lines(patch)
        .into_iter()
        .map(detail_rendered_line)
        .collect()
}

fn output_block_lines(
    label: &'static str,
    content: &str,
    _verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    if content.trim().is_empty() {
        return Vec::new();
    }
    let limit = usize::MAX;
    let lines = head_tail_lines(content, limit);
    let mut rendered = vec![detail_line(false, QUIET, label)];
    rendered.extend(lines.into_iter().map(|line| {
        if line.truncated_marker {
            detail_line(false, QUIET, line.text)
        } else {
            detail_spans_line(styled_output_spans(&line.text))
        }
    }));
    rendered
}

#[derive(Debug, Clone)]
struct PreviewLine {
    text: String,
    truncated_marker: bool,
}

fn head_tail_lines(content: &str, limit: usize) -> Vec<PreviewLine> {
    let mut lines = content.lines().map(str::to_string).collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(content.to_string());
    }
    if lines.len() <= limit {
        return lines
            .into_iter()
            .map(|text| PreviewLine {
                text,
                truncated_marker: false,
            })
            .collect();
    }
    let head = limit / 2;
    let tail = limit.saturating_sub(head).saturating_sub(1);
    let omitted = lines.len().saturating_sub(head + tail);
    let mut preview = lines
        .iter()
        .take(head)
        .cloned()
        .map(|text| PreviewLine {
            text,
            truncated_marker: false,
        })
        .collect::<Vec<_>>();
    preview.push(PreviewLine {
        text: format!("… +{omitted} lines (Ctrl-E to expand)"),
        truncated_marker: true,
    });
    preview.extend(
        lines
            .into_iter()
            .rev()
            .take(tail)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|text| PreviewLine {
                text,
                truncated_marker: false,
            }),
    );
    preview
}

fn detail_spans_line(content: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            "└ ",
            Style::default().fg(QUIET).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(content);
    Line::from(spans)
}

fn detail_rendered_line(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            "└ ",
            Style::default().fg(QUIET).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(line.spans);
    Line::from(spans)
}

fn command_spans(command: &str) -> Vec<Span<'static>> {
    let tokens = command
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return vec![Span::styled(
            command.to_string(),
            Style::default().fg(Color::White),
        )];
    }
    let mut command_seen = false;
    let mut spans = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if !command_seen && !looks_like_env_assignment(token) {
            command_seen = true;
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else if token.starts_with('-') {
            Style::default().fg(AMBER)
        } else if token.starts_with('"') || token.starts_with('\'') {
            Style::default().fg(SUCCESS_GREEN)
        } else if token.contains('/') || token.contains('.') {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(QUIET)
        };
        spans.push(Span::styled(token.clone(), style));
    }
    spans
}

fn looks_like_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn styled_output_spans(line: &str) -> Vec<Span<'static>> {
    if line.contains("\x1b[") {
        ansi_spans(line)
    } else {
        keyword_spans(line)
    }
}

fn ansi_spans(line: &str) -> Vec<Span<'static>> {
    let mut spans = render::ansi::ansi_to_line(line).spans;
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

fn keyword_spans(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut token = String::new();
    for ch in line.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            push_keyword_token(&mut spans, &mut token);
            spans.push(Span::styled(ch.to_string(), Style::default().fg(QUIET)));
        }
    }
    push_keyword_token(&mut spans, &mut token);
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

fn push_keyword_token(spans: &mut Vec<Span<'static>>, token: &mut String) {
    if token.is_empty() {
        return;
    }
    let lower = token.to_ascii_lowercase();
    let style = if matches!(
        lower.as_str(),
        "error" | "failed" | "failure" | "panic" | "fatal"
    ) {
        Style::default().fg(ERROR_RED).add_modifier(Modifier::BOLD)
    } else if matches!(lower.as_str(), "warning" | "warn") {
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD)
    } else if matches!(lower.as_str(), "ok" | "passed" | "success" | "done") {
        Style::default()
            .fg(SUCCESS_GREEN)
            .add_modifier(Modifier::BOLD)
    } else if matches!(
        lower.as_str(),
        "fn" | "class"
            | "interface"
            | "public"
            | "private"
            | "protected"
            | "return"
            | "async"
            | "await"
            | "let"
            | "const"
            | "struct"
            | "enum"
            | "impl"
    ) {
        Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    spans.push(Span::styled(std::mem::take(token), style));
}

fn string_arg(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn number_field(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|value| value.as_u64())
}

fn compact_json_object(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    if object.is_empty() {
        return None;
    }
    Some(
        object
            .iter()
            .map(|(key, value)| format!("{key} {}", value.as_u64().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn path_detail_lines(
    value: &serde_json::Value,
    object_key: &str,
    limit: usize,
) -> Vec<Line<'static>> {
    value
        .as_array()
        .into_iter()
        .flat_map(|items| items.iter().take(limit))
        .filter_map(|item| {
            if object_key.is_empty() {
                item.as_str()
            } else {
                item[object_key].as_str()
            }
        })
        .map(|path| detail_line(false, QUIET, format!("path {path}")))
        .collect()
}

fn append_truncation_hint(spans: &mut Vec<Span<'static>>, tool: &ToolTranscript) {
    if tool.result.cost_hint.truncated
        || tool.result.content["truncated"].as_bool().unwrap_or(false)
    {
        spans.push(Span::styled(
            " · more available",
            Style::default().fg(QUIET),
        ));
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn kind_label(kind: &str) -> String {
    match kind.trim().to_ascii_lowercase().as_str() {
        "callable" | "callables" | "function_like" | "function-like" | "functions" => {
            "callable".to_string()
        }
        other => other.replace('_', " "),
    }
}

fn is_exploration_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "repo_map"
            | "diff_context"
            | "decl_search"
            | "definition_search"
            | "reference_search"
            | "symbol_context"
            | "hierarchy"
            | "upstream_flow"
            | "downstream_flow"
            | "grep"
            | "glob"
            | "read_file"
            | "read_slice"
            | "read_tool_output"
    )
}

fn is_invalid_argument_result(result: &ToolResult) -> bool {
    result.status == ToolStatus::Error
        && result
            .content
            .get("error")
            .and_then(|value| value.as_str())
            .is_some_and(|error| error.contains("invalid tool arguments"))
}

fn cargo_manifest_missing_result(result: &ToolResult) -> bool {
    matches!(result.tool_name.as_str(), "shell" | "verify")
        && ["error", "stderr", "stdout"].iter().any(|key| {
            result
                .content
                .get(*key)
                .and_then(|value| value.as_str())
                .is_some_and(|text| text.contains("could not find `Cargo.toml`"))
        })
}

fn tool_result_error_detail(result: &ToolResult) -> String {
    if cargo_manifest_missing_result(result) {
        return "no Cargo.toml found".to_string();
    }
    if let Some(reason) = result
        .content
        .get("reason")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && result
            .content
            .get("not_run")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    {
        return compact_text(reason, 140);
    }
    if let Some(error) = result
        .content
        .get("error")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if error == "invalid tool arguments from model"
            && let Some(parse_error) = result
                .content
                .get("parse_error")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        {
            return compact_text(parse_error, 140);
        }
        return compact_text(error, 140);
    }
    if let Some(code) = result
        .content
        .get("exit_code")
        .and_then(|value| value.as_i64())
    {
        return format!("exit {code}");
    }
    for key in ["stderr", "stdout"] {
        if let Some(line) = result
            .content
            .get(key)
            .and_then(|value| value.as_str())
            .and_then(first_nonempty_line)
        {
            return compact_text(line, 140);
        }
    }
    if result.cost_hint.truncated {
        "output shortened".to_string()
    } else {
        "no output".to_string()
    }
}

fn tool_result_status_text(result: &ToolResult) -> String {
    if cargo_manifest_missing_result(result) {
        return format!("{} not run: no Cargo.toml found", result.tool_name);
    }
    if is_retryable_tool_result(result) {
        return format!(
            "{} retrying: {}",
            result.tool_name,
            tool_result_error_detail(result)
        );
    }
    let status = match result.status {
        ToolStatus::Success => "completed",
        ToolStatus::Error | ToolStatus::Stale => "failed",
        ToolStatus::Denied => "denied",
        ToolStatus::Cancelled => "cancelled",
    };
    let mut text = format!("{} {status}", result.tool_name);
    if result.cost_hint.truncated {
        text.push_str(" · output shortened");
    } else if result.cost_hint.output_bytes > 0 {
        text.push_str(&format!(
            " · {}",
            format_bytes(result.cost_hint.output_bytes)
        ));
    }
    if result.cost_hint.redactions > 0 {
        text.push_str(&format!(" · redacted {}", result.cost_hint.redactions));
    }
    text
}

fn tool_result_denied_detail(result: &ToolResult) -> String {
    result
        .content
        .get("reason")
        .or_else(|| result.content.get("error"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| compact_text(value, 140))
        .unwrap_or_else(|| "denied".to_string())
}

fn first_nonempty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn preview_tool_result(result: &ToolResult, verbosity: ToolOutputVerbosity) -> String {
    let limit = match verbosity {
        ToolOutputVerbosity::Compact => TOOL_PREVIEW_COMPACT_BYTES,
        ToolOutputVerbosity::Normal => TOOL_PREVIEW_NORMAL_BYTES,
        ToolOutputVerbosity::Verbose => TOOL_PREVIEW_VERBOSE_BYTES,
    };
    let text = tool_result_output_text(result).unwrap_or_else(|| {
        serde_json::to_string_pretty(&result.content).unwrap_or_else(|_| result.content.to_string())
    });
    truncate_bytes(&text, limit)
}

fn tool_result_output_text(result: &ToolResult) -> Option<String> {
    let stdout = string_arg(&result.content, "stdout").unwrap_or_default();
    let stderr = string_arg(&result.content, "stderr").unwrap_or_default();
    let output = match (!stdout.trim().is_empty(), !stderr.trim().is_empty()) {
        (true, true) => format!("{stdout}\n{stderr}"),
        (true, false) => stdout,
        (false, true) => stderr,
        (false, false) => string_arg(&result.content, "output").unwrap_or_default(),
    };
    (!output.trim().is_empty()).then_some(output)
}

fn status_color(status: ToolStatus) -> Color {
    match status {
        ToolStatus::Success => SUCCESS_GREEN,
        ToolStatus::Error | ToolStatus::Stale => ERROR_RED,
        ToolStatus::Denied | ToolStatus::Cancelled => GOLD,
    }
}

fn tool_result_display_color(tool: &ToolTranscript) -> Color {
    if tool_result_not_run(tool) || is_retryable_tool_result(&tool.result) {
        GOLD
    } else {
        status_color(tool.result.status)
    }
}

fn tool_result_action(tool: &ToolTranscript) -> (&'static str, &'static str) {
    if tool_result_not_run(tool) {
        return ("⚠ ", "Not run");
    }
    if is_retryable_tool_result(&tool.result) {
        return ("⚠ ", "Retried");
    }
    match tool.result.status {
        ToolStatus::Success if tool.result.tool_name == "plan_patch" => ("✔ ", "Planned"),
        ToolStatus::Success
            if matches!(tool.result.tool_name.as_str(), "apply_patch" | "write_file") =>
        {
            ("✔ ", "Edited")
        }
        ToolStatus::Success if shell_result_is_exploration(tool) => ("✔ ", "Explored"),
        ToolStatus::Success if is_exploration_tool(&tool.result.tool_name) => ("✔ ", "Explored"),
        ToolStatus::Success => ("✔ ", "Ran"),
        ToolStatus::Error | ToolStatus::Stale if is_invalid_argument_result(&tool.result) => {
            ("⚠ ", "Retried")
        }
        ToolStatus::Error | ToolStatus::Stale => ("✖ ", "Failed"),
        ToolStatus::Denied => ("⚠ ", "Denied"),
        ToolStatus::Cancelled => ("⚠ ", "Cancelled"),
    }
}

fn tool_result_not_run(tool: &ToolTranscript) -> bool {
    tool.result
        .content
        .get("not_run")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        || cargo_manifest_missing_result(&tool.result)
}

fn is_retryable_tool_result(result: &ToolResult) -> bool {
    if is_invalid_argument_result(result) {
        return true;
    }
    matches!(result.tool_name.as_str(), "apply_patch" | "write_file")
        && matches!(result.status, ToolStatus::Error | ToolStatus::Stale)
        && result
            .content
            .get("error")
            .and_then(|value| value.as_str())
            .is_some_and(|error| {
                error.contains("search text matched more than once")
                    || error.contains("search text not found")
                    || error.contains("expected_sha256")
            })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnVisualState {
    Idle,
    Running,
    Succeeded,
    Failed,
}

impl TurnVisualState {
    fn color(self, tick: u64) -> Color {
        match self {
            Self::Idle => AMBER,
            Self::Running => {
                if tick % 8 < 4 {
                    GOLD
                } else {
                    AMBER
                }
            }
            Self::Succeeded => SUCCESS_GREEN,
            Self::Failed => ERROR_RED,
        }
    }
}

fn turn_coin_span(app: &TuiApp) -> Span<'static> {
    Span::styled(
        prompt_coin_frame(app),
        Style::default()
            .fg(app.turn_visual.color(app.animation_tick))
            .add_modifier(Modifier::BOLD),
    )
}

fn assistant_static_span(color: Color) -> Span<'static> {
    Span::styled("●", Style::default().fg(color).add_modifier(Modifier::BOLD))
}

fn prompt_coin_span(app: &TuiApp) -> Span<'static> {
    let color = if (prompt_elapsed_ms(app) / 800).is_multiple_of(2) {
        GOLD
    } else {
        AMBER
    };
    Span::styled(prompt_coin_frame(app), Style::default().fg(color))
}

fn prompt_coin_frame(app: &TuiApp) -> &'static str {
    const FRAMES: [&str; 8] = ["●", "◕", "◐", "◔", "○", "◔", "◑", "◕"];
    let elapsed_ms = prompt_elapsed_ms(app);
    let direction_reversed = (elapsed_ms / 60_000) % 2 == 1;
    let frame_index = ((elapsed_ms / 320) as usize) % FRAMES.len();
    if direction_reversed {
        FRAMES[FRAMES.len() - 1 - frame_index]
    } else {
        FRAMES[frame_index]
    }
}

fn prompt_elapsed_ms(app: &TuiApp) -> u64 {
    let tick_ms = app.animation_tick_rate.as_millis().max(1) as u64;
    app.animation_tick.saturating_mul(tick_ms)
}

fn prompt_cursor_span() -> Span<'static> {
    Span::styled("┃", Style::default().fg(GOLD).bg(PROMPT_BG))
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

fn input_panel_height(app: &TuiApp, width: u16) -> u16 {
    prompt_visual_line_count(&app.input, width)
        .saturating_add(2)
        .saturating_add(slash_suggestions(&app.input).len())
        .clamp(PROMPT_MIN_HEIGHT as usize, PROMPT_MAX_HEIGHT as usize) as u16
}

fn prompt_visual_line_count(input: &str, width: u16) -> usize {
    let content_width = width.saturating_sub(4).max(1) as usize;
    if input.is_empty() {
        return 1;
    }
    input
        .split('\n')
        .map(|line| {
            let chars = line.chars().count().max(1);
            chars.div_ceil(content_width)
        })
        .sum()
}

fn prompt_input_content_lines(app: &TuiApp) -> Vec<Line<'static>> {
    if app.input.is_empty() {
        return vec![Line::from(vec![
            Span::styled(" ", Style::default().bg(PROMPT_BG)),
            prompt_coin_span(app),
            Span::styled("  ", Style::default().bg(PROMPT_BG)),
            prompt_cursor_span(),
        ])];
    }
    let cursor = input_cursor(app);
    let parts = app.input.split('\n').collect::<Vec<_>>();
    let mut line_start = 0usize;
    parts
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 {
                vec![
                    Span::styled(" ", Style::default().bg(PROMPT_BG)),
                    prompt_coin_span(app),
                    Span::styled("  ", Style::default().bg(PROMPT_BG)),
                ]
            } else {
                vec![Span::styled(
                    "    ",
                    Style::default().fg(QUIET).bg(PROMPT_BG),
                )]
            };
            let mut spans = prefix;
            let line_end = line_start + line.len();
            if cursor >= line_start && cursor <= line_end {
                let split_at = cursor.saturating_sub(line_start).min(line.len());
                let (before, after) = line.split_at(split_at);
                if !before.is_empty() {
                    spans.push(Span::styled(
                        before.to_string(),
                        Style::default().fg(Color::White).bg(PROMPT_BG),
                    ));
                }
                spans.push(prompt_cursor_span());
                if !after.is_empty() {
                    spans.push(Span::styled(
                        after.to_string(),
                        Style::default().fg(Color::White).bg(PROMPT_BG),
                    ));
                }
            } else {
                spans.push(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::White).bg(PROMPT_BG),
                ));
            }
            line_start = line_end.saturating_add(1);
            Line::from(spans)
        })
        .collect()
}

fn prompt_blank_line() -> Line<'static> {
    Line::from(Span::styled(" ", Style::default().bg(PROMPT_BG)))
}

fn prompt_input_lines(app: &TuiApp, height: u16) -> Vec<Line<'static>> {
    let content = prompt_input_content_lines(app);
    let spare = (height as usize).saturating_sub(content.len());
    let top_padding = spare / 2;
    let bottom_padding = spare.saturating_sub(top_padding);
    let mut lines = Vec::with_capacity(top_padding + content.len() + bottom_padding);
    lines.extend((0..top_padding).map(|_| prompt_blank_line()));
    lines.extend(content);
    lines.extend((0..bottom_padding).map(|_| prompt_blank_line()));
    lines
}

fn slash_suggestion_lines(app: &TuiApp) -> Vec<Line<'static>> {
    let suggestions = slash_suggestions(&app.input);
    let visible = visible_slash_suggestions(&suggestions, app.slash_menu_index);
    let command_width = visible
        .iter()
        .map(|command| command.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(12);
    visible
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let absolute_index = slash_menu_window_start(suggestions.len(), app.slash_menu_index)
                .saturating_add(index);
            let selected = absolute_index
                == app
                    .slash_menu_index
                    .min(suggestions.len().saturating_sub(1));
            let marker = if selected { "› " } else { "  " };
            let command_padding =
                " ".repeat(command_width.saturating_sub(command.name.chars().count()) + 2);
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected { GOLD } else { QUIET }),
                ),
                Span::styled(
                    command.name,
                    Style::default().fg(if selected { GOLD } else { AMBER }),
                ),
                Span::styled(command_padding, Style::default().fg(QUIET)),
                Span::styled(command.description, Style::default().fg(QUIET)),
            ])
        })
        .collect()
}

fn slash_menu_window_start(total: usize, selected: usize) -> usize {
    if total <= SLASH_MENU_MAX_ITEMS {
        return 0;
    }
    selected
        .saturating_add(1)
        .saturating_sub(SLASH_MENU_MAX_ITEMS)
        .min(total - SLASH_MENU_MAX_ITEMS)
}

fn visible_slash_suggestions(suggestions: &[SlashCommand], selected: usize) -> &[SlashCommand] {
    let start = slash_menu_window_start(suggestions.len(), selected);
    let end = start
        .saturating_add(SLASH_MENU_MAX_ITEMS)
        .min(suggestions.len());
    &suggestions[start..end]
}

fn render_input(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let suggestion_lines = slash_suggestion_lines(app);
    let prompt_height = area.height.saturating_sub(suggestion_lines.len() as u16);
    let mut lines = prompt_input_lines(app, prompt_height);
    lines.extend(suggestion_lines);
    let scroll = lines.len().saturating_sub(area.height as usize) as u16;
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(Color::White).bg(PROMPT_BG))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn format_status_overview_line(app: &TuiApp, width: u16) -> Line<'static> {
    let right = mode_status_text(app);
    let right_width = right.chars().count();
    let available_left = (width as usize).saturating_sub(right_width + 1);
    let left = fit_chars(&status_left_text(app), available_left);
    let left_width = left.chars().count();
    let padding = " ".repeat((width as usize).saturating_sub(left_width + right_width));
    Line::from(vec![
        Span::styled(left, Style::default().fg(Color::Gray)),
        Span::raw(padding),
        Span::styled(right, Style::default().fg(mode_status_color(app.mode))),
    ])
}

fn status_left_text(app: &TuiApp) -> String {
    let branch = if app.repo.available {
        app.repo.branch.as_deref().unwrap_or("detached")
    } else {
        "no repo"
    };
    format!("dir {} · git {}", app.directory, branch)
}

fn mode_status_text(app: &TuiApp) -> String {
    format!("{} mode (Shift+Tab to cycle)", title_case_mode(app.mode))
}

fn title_case_mode(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Plan => "Plan",
        SessionMode::Build => "Build",
    }
}

fn mode_status_color(mode: SessionMode) -> Color {
    match mode {
        SessionMode::Plan => MODE_PURPLE,
        SessionMode::Build => MODE_BUILD_GREEN,
    }
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph =
        Paragraph::new(format_status_lines(app, area.width)).style(Style::default().fg(QUIET));
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

fn format_status_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![
        format_status_overview_line(app, width),
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
    format!("{}  {}", status_left_text(app), mode_status_text(app))
}

fn format_status_details(app: &TuiApp) -> String {
    format!(
        "{}  repo {}  sandbox {}  telemetry {}  cost {}  tok {}/{}{}  ctx {}  pins {}  compact {}  tools {}  budget {}  cfg {}  read {}B  receipts {}  redactions {}  cached {}  cache_write {}",
        app.permissions.compact(),
        app.repo.detail(),
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

fn format_status_hints(app: &TuiApp) -> String {
    if app.pending_approval.is_some() {
        return "Up/Down choose · Enter select · Y approve · A always approve repo · N deny · Esc cancel"
            .to_string();
    } else if app.cancel.is_some() {
        return "Ctrl-C/Esc interrupt · Ctrl+J newline · Ctrl-P task · Ctrl-E expand/collapse · Ctrl-Y copy · /help"
            .to_string();
    } else if app.exit_armed {
        return "Esc again to exit · Enter send · Ctrl+J newline · Ctrl-P task · Ctrl-E expand/collapse · /help"
            .to_string();
    }
    if app.alternate_scroll_enabled {
        "Enter send · !cmd shell · Wheel/PgUp/PgDn scroll · Up/Down menu · Alt+Up/Down history · Ctrl+J newline · Ctrl-E expand/collapse · /help"
            .to_string()
    } else {
        "Enter send · !cmd shell · Up/Down menu/history · Ctrl+J newline · Ctrl-E expand/collapse · /help"
            .to_string()
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
        format!("repo={}", self.detail())
    }

    fn detail(&self) -> String {
        if !self.available {
            return "none".to_string();
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
    input_cursor: usize,
    input_history: Vec<String>,
    input_history_index: Option<usize>,
    input_history_draft: String,
    slash_menu_index: usize,
    alternate_scroll_enabled: bool,
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
    active_tool: Option<String>,
    status: String,
    turn_visual: TurnVisualState,
    turn_started_at: Option<Instant>,
    last_turn_duration: Option<Duration>,
    animation_tick: u64,
    animation_tick_rate: Duration,
    exit_armed: bool,
    active_tool_calls: BTreeMap<String, ToolCall>,
    cost: squeezy_core::CostSnapshot,
    metrics: squeezy_core::TurnMetrics,
    turn_rx: Option<mpsc::Receiver<AgentEvent>>,
    job_rx: Option<broadcast::Receiver<JobEvent>>,
    jobs: BTreeMap<JobId, JobSnapshot>,
    notifications: VecDeque<JobNotification>,
    cancel: Option<CancellationToken>,
    pending_approval: Option<PendingApproval>,
    approval_selection_index: usize,
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
        let transcript = Vec::new();
        let status = "ready".to_string();
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
            input_cursor: 0,
            input_history: Vec::new(),
            input_history_index: None,
            input_history_draft: String::new(),
            slash_menu_index: 0,
            alternate_scroll_enabled: TerminalMode::from(config.tui.alternate_screen)
                == TerminalMode::AlternateScreen,
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
            active_tool: None,
            status,
            turn_visual: TurnVisualState::Idle,
            turn_started_at: None,
            last_turn_duration: None,
            animation_tick: 0,
            animation_tick_rate: config.tick_rate,
            exit_armed: false,
            active_tool_calls: BTreeMap::new(),
            cost: squeezy_core::CostSnapshot::default(),
            metrics: squeezy_core::TurnMetrics::default(),
            turn_rx: None,
            job_rx: None,
            jobs: BTreeMap::new(),
            notifications: VecDeque::new(),
            cancel: None,
            pending_approval: None,
            approval_selection_index: 0,
            pending_feedback: None,
            pending_report: None,
            clipboard,
        }
    }

    fn note_turn_started(&mut self) {
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(Instant::now());
        }
        self.last_turn_duration = None;
    }

    fn note_turn_finished(&mut self) {
        if let Some(started_at) = self.turn_started_at.take() {
            self.last_turn_duration = Some(started_at.elapsed());
        }
    }

    fn push_transcript_item(&mut self, item: TranscriptItem) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::message(id, item, self.transcript_default));
    }

    #[cfg(test)]
    fn push_tool_result(&mut self, result: ToolResult) {
        self.push_tool_result_with_call(result, None);
    }

    fn push_tool_result_with_call(&mut self, result: ToolResult, call: Option<ToolCall>) {
        if tool_result_hidden_by_default(&result) {
            return;
        }
        let id = self.next_id();
        let entry = TranscriptEntry::tool_result(id, result, call, self.transcript_default);
        if let Some(last) = self.transcript.last_mut()
            && coalesce_tool_transcript_entry(last, &entry)
        {
            return;
        }
        self.push_entry(entry);
    }

    fn push_log(&mut self, message: String) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::log(id, message, self.transcript_default));
    }

    fn push_entry(&mut self, entry: TranscriptEntry) {
        self.transcript.push(entry);
    }

    fn remember_active_tool_call(&mut self, call: ToolCall) {
        if is_control_tool_name(&call.name) {
            return;
        }
        self.active_tool = Some(call.name.clone());
        self.active_tool_calls.insert(call.call_id.clone(), call);
    }

    fn refresh_active_tool_name(&mut self) {
        self.active_tool = self
            .active_tool_calls
            .values()
            .find(|call| !is_control_tool_name(&call.name))
            .map(|call| call.name.clone());
    }

    fn clear_active_tools(&mut self) {
        self.active_tool = None;
        self.active_tool_calls.clear();
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
            && item.role != Role::Assistant
            && item.content.chars().count() > LONG_ASSISTANT_CHARS;
        Self {
            id,
            kind: TranscriptEntryKind::Message(item),
            collapsed,
        }
    }

    fn tool_result(
        id: u64,
        result: ToolResult,
        call: Option<ToolCall>,
        transcript_default: TranscriptDefault,
    ) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::ToolResult(Box::new(ToolTranscript {
                call,
                result,
                repeat_count: 1,
            })),
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
            TranscriptCategory::Tools => matches!(self.kind, TranscriptEntryKind::ToolResult(_)),
            TranscriptCategory::Logs => match &self.kind {
                TranscriptEntryKind::Log(_) => true,
                TranscriptEntryKind::Message(item) => item.role == Role::System,
                _ => false,
            },
            TranscriptCategory::Diffs => match &self.kind {
                TranscriptEntryKind::ToolResult(tool) => tool.result.tool_name.contains("diff"),
                _ => false,
            },
            TranscriptCategory::Receipts => match &self.kind {
                TranscriptEntryKind::ToolResult(tool) => {
                    !tool.result.receipt.output_sha256.is_empty()
                }
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
            TranscriptEntryKind::ToolResult(tool) => {
                vec![format!("tool result: {}", tool_result_summary(tool))]
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

    fn is_toggleable(&self) -> bool {
        true
    }

    fn pin_payload(&self) -> (String, String, String) {
        match &self.kind {
            TranscriptEntryKind::Message(item) => (
                format!("{} message", role_label(&item.role)),
                item.content.clone(),
                format!("transcript:{}", self.id),
            ),
            TranscriptEntryKind::ToolResult(tool) => (
                format!("tool result {}", tool.result.tool_name),
                tool_result_summary(tool),
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
    ToolResult(Box<ToolTranscript>),
    Log(String),
}

#[derive(Debug, Clone)]
struct ToolTranscript {
    call: Option<ToolCall>,
    result: ToolResult,
    repeat_count: u32,
}

fn coalesce_tool_transcript_entry(existing: &mut TranscriptEntry, next: &TranscriptEntry) -> bool {
    let TranscriptEntryKind::ToolResult(existing_tool) = &mut existing.kind else {
        return false;
    };
    let TranscriptEntryKind::ToolResult(next_tool) = &next.kind else {
        return false;
    };
    if tool_retry_key(existing_tool.as_ref()).is_some()
        && tool_retry_key(existing_tool.as_ref()) == tool_retry_key(next_tool.as_ref())
    {
        existing_tool.repeat_count += next_tool.repeat_count;
        existing_tool.result = next_tool.result.clone();
        existing_tool.call = next_tool.call.clone();
        true
    } else {
        false
    }
}

fn tool_result_hidden_by_default(result: &ToolResult) -> bool {
    result.tool_name == "plan_patch" && result.status == ToolStatus::Success
}

fn tool_retry_key(tool: &ToolTranscript) -> Option<String> {
    if !is_retryable_tool_result(&tool.result) {
        return None;
    }
    Some(format!(
        "{}:{}",
        tool.result.tool_name,
        tool_result_error_detail(&tool.result)
    ))
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

fn exit_hint(session_id: Option<&str>) -> Option<String> {
    session_id.map(|session_id| {
        format!("Squeezy session saved. Resume with: squeezy sessions resume {session_id}")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalMode {
    Inline,
    AlternateScreen,
}

impl From<TuiAlternateScreen> for TerminalMode {
    fn from(value: TuiAlternateScreen) -> Self {
        match value {
            TuiAlternateScreen::Auto => Self::Inline,
            TuiAlternateScreen::Never => Self::Inline,
            TuiAlternateScreen::Always => Self::AlternateScreen,
        }
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    mode: TerminalMode,
    exit_hint: Option<String>,
    startup_flushed: bool,
    transcript_flushed_len: usize,
}

impl TerminalGuard {
    fn enter(alternate_screen: TuiAlternateScreen) -> Result<Self> {
        let mode = TerminalMode::from(alternate_screen);
        enable_raw_mode().map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        let mut stdout = io::stdout();
        match mode {
            TerminalMode::Inline => {
                execute!(
                    stdout,
                    Print(CLEAR_SCROLLBACK_AND_VISIBLE),
                    Print(DISABLE_MOUSE_MODES),
                    DisableAlternateScroll,
                    EnableBracketedPaste
                )
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
            }
            TerminalMode::AlternateScreen => {
                execute!(
                    stdout,
                    EnterAlternateScreen,
                    Print(DISABLE_MOUSE_MODES),
                    EnableAlternateScroll,
                    Clear(ClearType::All),
                    MoveTo(0, 0),
                    EnableBracketedPaste
                )
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
            }
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match mode {
            TerminalMode::Inline => Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Inline(INLINE_VIEWPORT_HEIGHT),
                },
            ),
            TerminalMode::AlternateScreen => Terminal::new(backend),
        }
        .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        Ok(Self {
            terminal,
            mode,
            exit_hint: None,
            startup_flushed: false,
            transcript_flushed_len: 0,
        })
    }

    fn set_exit_hint(&mut self, exit_hint: Option<String>) {
        self.exit_hint = exit_hint;
    }

    fn draw_app(&mut self, app: &TuiApp) -> Result<()> {
        match self.mode {
            TerminalMode::Inline => {
                self.flush_history(app)?;
                self.terminal.draw(|frame| render_inline(frame, app))
            }
            TerminalMode::AlternateScreen => self.terminal.draw(|frame| render(frame, app)),
        }
        .map(|_| ())
        .map_err(|err| SqueezyError::Terminal(err.to_string()))
    }

    fn flush_history(&mut self, app: &TuiApp) -> Result<()> {
        if self.mode != TerminalMode::Inline {
            return Ok(());
        }
        let width = self
            .terminal
            .size()
            .map_err(|err| SqueezyError::Terminal(err.to_string()))?
            .width;
        let lines = inline_history_lines_for_flush(
            app,
            width,
            !self.startup_flushed,
            self.transcript_flushed_len,
        );
        self.startup_flushed = true;
        self.transcript_flushed_len = app.transcript.len();
        self.insert_before(lines, width)
    }

    fn insert_before(&mut self, lines: Vec<Line<'static>>, width: u16) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let height = visual_line_count(&lines, width);
        self.terminal
            .insert_before(height, |buffer| render_lines_to_buffer(buffer, lines))
            .map_err(|err| SqueezyError::Terminal(err.to_string()))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        match self.mode {
            TerminalMode::Inline => {
                let _ = execute!(
                    self.terminal.backend_mut(),
                    DisableBracketedPaste,
                    DisableAlternateScroll,
                    Print(DISABLE_MOUSE_MODES),
                    Print(CLEAR_SCROLLBACK_AND_VISIBLE)
                );
            }
            TerminalMode::AlternateScreen => {
                let _ = execute!(
                    self.terminal.backend_mut(),
                    DisableBracketedPaste,
                    DisableAlternateScroll,
                    Print(DISABLE_MOUSE_MODES),
                    Clear(ClearType::All),
                    MoveTo(0, 0),
                    LeaveAlternateScreen
                );
            }
        }
        let _ = self.terminal.show_cursor();
        if let Some(hint) = &self.exit_hint {
            let _ = writeln!(self.terminal.backend_mut(), "{hint}");
        }
    }
}

fn render_lines_to_buffer(buffer: &mut Buffer, lines: Vec<Line<'static>>) {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(buffer.area, buffer);
}

fn inline_history_lines_for_flush(
    app: &TuiApp,
    width: u16,
    include_startup_card: bool,
    transcript_from: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if include_startup_card {
        lines.extend(startup_card_lines(app, width));
        lines.push(Line::from(""));
    }
    for (index, item) in app.transcript.iter().enumerate().skip(transcript_from) {
        lines.extend(format_transcript_entry_with_width(
            item,
            false,
            app.tool_output_verbosity,
            message_outcome(&app.transcript, index),
            Some(width),
        ));
    }
    lines
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
