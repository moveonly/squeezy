use std::{io, sync::Arc, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
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
use squeezy_agent::{Agent, AgentEvent, ToolApprovalDecision, ToolApprovalRequest};
use squeezy_core::{AppConfig, Result, Role, SessionMode, SqueezyError, TranscriptItem};
use squeezy_llm::LlmProvider;
use squeezy_tools::ToolCall;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub async fn run(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let agent = Agent::new(config.clone(), provider);
    let mut app = TuiApp::new(
        agent.provider_name(),
        config.model.clone(),
        config.config_source_labels().join(","),
        agent.session_mode(),
    );

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        drain_agent_events(&mut app).await;
        if poll_input(&mut app, &agent, config.tick_rate).await? {
            break;
        }
    }

    agent.flush_telemetry().await;

    Ok(())
}

async fn drain_agent_events(app: &mut TuiApp) {
    if let Some(rx) = &mut app.turn_rx {
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::UserMessage { message, .. } => {
                    app.transcript.push(message);
                    app.pending_assistant.clear();
                }
                AgentEvent::Started { .. } => {
                    app.status = "streaming response".to_string();
                }
                AgentEvent::AssistantDelta { delta, .. } => {
                    app.pending_assistant.push_str(&delta);
                }
                AgentEvent::ToolCallQueued { call, .. } => {
                    app.status = format!("tool queued: {}", call.name);
                }
                AgentEvent::ToolCallStarted { call, .. } => {
                    app.status = format!("running tool: {}", call.name);
                }
                AgentEvent::ToolCallCompleted { result, .. } => {
                    app.status = format!(
                        "tool {}: {:?} bytes={} truncated={}",
                        result.tool_name,
                        result.status,
                        result.cost_hint.output_bytes,
                        result.cost_hint.truncated
                    );
                    if result.cost_hint.redactions > 0 {
                        app.status
                            .push_str(&format!(" redactions={}", result.cost_hint.redactions));
                    }
                }
                AgentEvent::ApprovalRequested {
                    request,
                    decision_tx,
                    ..
                } => {
                    app.status = format_approval_prompt(&request);
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
                    app.transcript.push(message);
                    app.pending_assistant.clear();
                    app.cost = cost;
                    app.metrics = metrics;
                    app.status = "ready".to_string();
                    app.turn_rx = None;
                    app.cancel = None;
                    break;
                }
                AgentEvent::Cancelled { .. } => {
                    app.status = "cancelled".to_string();
                    app.pending_assistant.clear();
                    app.turn_rx = None;
                    app.cancel = None;
                    break;
                }
                AgentEvent::Failed { error, .. } => {
                    app.status = format!("provider error: {error}");
                    app.pending_assistant.clear();
                    app.turn_rx = None;
                    app.cancel = None;
                    break;
                }
            }
        }
    }
}

async fn poll_input(app: &mut TuiApp, agent: &Agent, tick_rate: Duration) -> Result<bool> {
    if !event::poll(tick_rate).map_err(|err| SqueezyError::Terminal(err.to_string()))? {
        return Ok(false);
    }

    let Event::Key(key) = event::read().map_err(|err| SqueezyError::Terminal(err.to_string()))?
    else {
        return Ok(false);
    };

    handle_key(app, agent, key).await
}

async fn handle_key(app: &mut TuiApp, agent: &Agent, key: KeyEvent) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if let Some(cancel) = &app.cancel {
            cancel.cancel();
            app.status = "cancelling".to_string();
            return Ok(false);
        }
        return Ok(true);
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

    if handle_approval_key(app, key) {
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc => Ok(true),
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
            app.turn_rx = Some(agent.start_turn(input, cancel.clone()));
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

async fn handle_slash_command(app: &mut TuiApp, agent: &Agent, input: &str) -> bool {
    let mut parts = input.split_whitespace();
    let Some(command) = parts.next() else {
        return false;
    };
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
    let result = agent
        .execute_local_tool(ToolCall {
            call_id: format!("tui-{name}"),
            name: name.to_string(),
            arguments,
        })
        .await;
    app.status = format!(
        "{}: {:?} {}",
        name,
        result.status,
        summarize_local_tool_result(&result.content)
    );
    true
}

fn summarize_local_tool_result(content: &serde_json::Value) -> String {
    if let Some(array) = content
        .get("checkpoints")
        .and_then(|value| value.as_array())
    {
        return format!("{} checkpoints", array.len());
    }
    if let Some(checkpoint) = content.get("checkpoint") {
        let id = checkpoint
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("?");
        let files = checkpoint
            .get("files")
            .and_then(|value| value.as_array())
            .map_or(0, |items| items.len());
        let skipped = checkpoint
            .get("skipped_files")
            .and_then(|value| value.as_array())
            .map_or(0, |items| items.len());
        return format!("checkpoint={id} files={files} skipped={skipped}");
    }
    if let Some(rollback) = content.get("rollback") {
        let restored = rollback
            .get("restored_files")
            .and_then(|value| value.as_array())
            .map_or(0, |items| items.len());
        let deleted = rollback
            .get("deleted_files")
            .and_then(|value| value.as_array())
            .map_or(0, |items| items.len());
        let conflicts = rollback
            .get("conflicts")
            .and_then(|value| value.as_array())
            .map_or(0, |items| items.len());
        return format!("restored={restored} deleted={deleted} conflicts={conflicts}");
    }
    String::new()
}

fn mode_command(input: &str) -> Option<SessionMode> {
    match input {
        "/plan" => Some(SessionMode::Plan),
        "/build" => Some(SessionMode::Build),
        _ => None,
    }
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
            app.status = format_approval_prompt(&pending.request);
            app.pending_approval = Some(pending);
            true
        }
    }
}

pub(crate) fn format_approval_prompt(request: &ToolApprovalRequest) -> String {
    let permission = &request.permission;
    format!(
        "approve {summary} | risk={risk} target={target} | y once | a user allow | p project allow | u user deny | d project deny | n deny once",
        summary = permission.summary,
        risk = permission.risk.as_str(),
        target = permission.target,
    )
}

fn render(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    render_transcript(frame, chunks[0], app);
    render_input(frame, chunks[1], app);
    render_status(frame, chunks[2], app);
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut lines = Vec::new();
    for item in &app.transcript {
        lines.push(format_transcript_item(item));
    }
    if !app.pending_assistant.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "assistant ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(&app.pending_assistant),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(
            "Squeezy is ready. Type a prompt and press Enter.",
        ));
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().title("Squeezy").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn format_transcript_item(item: &TranscriptItem) -> Line<'_> {
    let (label, color) = match item.role {
        Role::User => ("user ", Color::Cyan),
        Role::Assistant => ("assistant ", Color::Green),
        Role::System => ("system ", Color::Yellow),
    };
    Line::from(vec![
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(item.content.as_str()),
    ])
}

fn render_input(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph = Paragraph::new(app.input.as_str())
        .block(Block::default().title("Prompt").borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    frame.render_widget(Paragraph::new(format_status_tokens(app)), area);
}

fn format_status_tokens(app: &TuiApp) -> String {
    format!(
        "provider={} model={} mode={} cfg={} status={} tools={} read={}B receipt_hits={} budget_denials={} redactions={} in={} out={} cached={} cache_write={} cost={} | Enter send | Shift-Tab mode | /plan /build | /undo /checkpoint /checkpoints /revert-turn | y/a/p approve | n/u/d deny | Ctrl-C cancel/quit | Esc quit",
        app.provider_name,
        app.model,
        app.mode.as_str(),
        app.config_sources,
        app.status,
        app.metrics.tool_calls,
        app.metrics.bytes_read,
        app.metrics.receipt_stub_hits + app.metrics.negative_receipt_hits,
        app.metrics.budget_denials,
        app.metrics.redactions,
        app.cost
            .input_tokens
            .map_or("-".to_string(), |value| value.to_string()),
        app.cost
            .output_tokens
            .map_or("-".to_string(), |value| value.to_string()),
        app.cost
            .cached_input_tokens
            .map_or("-".to_string(), |value| value.to_string()),
        app.cost
            .cache_write_input_tokens
            .map_or("-".to_string(), |value| value.to_string()),
        app.cost
            .estimated_usd_micros
            .map_or("-".to_string(), |value| format!(
                "${:.6}",
                value as f64 / 1_000_000.0
            )),
    )
}

struct TuiApp {
    provider_name: &'static str,
    model: String,
    mode: SessionMode,
    config_sources: String,
    input: String,
    transcript: Vec<TranscriptItem>,
    pending_assistant: String,
    status: String,
    cost: squeezy_core::CostSnapshot,
    metrics: squeezy_core::TurnMetrics,
    turn_rx: Option<mpsc::Receiver<AgentEvent>>,
    cancel: Option<CancellationToken>,
    pending_approval: Option<PendingApproval>,
}

impl TuiApp {
    fn new(
        provider_name: &'static str,
        model: String,
        config_sources: String,
        mode: SessionMode,
    ) -> Self {
        Self {
            provider_name,
            model,
            mode,
            config_sources,
            input: String::new(),
            transcript: Vec::new(),
            pending_assistant: String::new(),
            status: "ready".to_string(),
            cost: squeezy_core::CostSnapshot::default(),
            metrics: squeezy_core::TurnMetrics::default(),
            turn_rx: None,
            cancel: None,
            pending_approval: None,
        }
    }
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
        execute!(stdout, EnterAlternateScreen)
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
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
