use std::{
    io::{self, Write},
    sync::Arc,
    time::Duration,
};

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
use squeezy_core::{
    AppConfig, PermissionPolicy, Result, Role, SessionMode, SqueezyError, StatusVerbosity,
    TelemetryConfig, TranscriptItem,
};
use squeezy_llm::LlmProvider;
use squeezy_tools::ToolCall;
use squeezy_vcs::{DiffMode, DiffOptions, GitVcs, VcsKind};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub async fn run(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Result<()> {
    run_with_onboarding(config, provider, None).await
}

pub async fn run_with_onboarding(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    onboarding_summary: Option<String>,
) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let agent = Agent::new(config.clone(), provider);
    let mut app = TuiApp::new(
        agent.provider_name(),
        &config,
        agent.session_mode(),
        onboarding_summary,
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
                    app.transcript.push(message);
                    app.pending_assistant.clear();
                    app.cost = cost;
                    app.metrics = metrics;
                    app.status = "ready".to_string();
                    // Preserve the user's scroll position; if they paged up
                    // mid-turn we shouldn't snap them down on completion.
                    app.turn_rx = None;
                    app.cancel = None;
                    break;
                }
                AgentEvent::Cancelled { .. } => {
                    app.status = "cancelled; edit prompt or retry".to_string();
                    app.pending_assistant.clear();
                    app.turn_rx = None;
                    app.cancel = None;
                    break;
                }
                AgentEvent::Failed { error, .. } => {
                    app.status = format_error_status(&error);
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

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
        copy_to_clipboard(app, ClipboardTarget::LastAssistant);
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

    if handle_approval_key(app, key) {
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc => Ok(true),
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
    if command == "/copy" {
        match parts.next() {
            None => copy_to_clipboard(app, ClipboardTarget::LastAssistant),
            Some("transcript") => copy_to_clipboard(app, ClipboardTarget::Transcript),
            Some(_) => app.status = "usage: /copy [transcript]".to_string(),
        }
        return true;
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
                .find(|item| item.role == Role::Assistant && !item.content.trim().is_empty())
                .map(|item| item.content.clone())
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
        lines.push(format!("{}: {}", role_label(&item.role), item.content));
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
/// registry; the doc in `docs/CONFIGURATION.md` references this contract.
pub(crate) const APPROVAL_PROMPT_KEYS: &[&str] = &[
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
    if let Some(pending) = app.pending_approval.as_ref() {
        // When an approval is pending, reserve a dedicated panel large
        // enough to show every metadata line of `format_approval_prompt`.
        let prompt = format_approval_prompt(&pending.request);
        let line_count = prompt.matches('\n').count() as u16 + 1;
        let approval_height = line_count.saturating_add(2).clamp(6, 18);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(approval_height),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(area);
        render_transcript(frame, chunks[0], app);
        render_approval(frame, chunks[1], &prompt);
        render_input(frame, chunks[2], app);
        render_status(frame, chunks[3], app);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(area);
        render_transcript(frame, chunks[0], app);
        render_input(frame, chunks[1], app);
        render_status(frame, chunks[2], app);
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

    let scroll =
        transcript_scroll_offset(lines.len(), area.height, app.transcript_scroll_from_bottom);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().title("Squeezy").borders(Borders::ALL))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn transcript_scroll_offset(line_count: usize, area_height: u16, from_bottom: u16) -> u16 {
    let visible_lines = area_height.saturating_sub(2) as usize;
    let max_scroll = line_count.saturating_sub(visible_lines);
    max_scroll.saturating_sub(from_bottom as usize) as u16
}

fn format_transcript_item(item: &TranscriptItem) -> Line<'_> {
    let (label, color) = match &item.role {
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
    let paragraph =
        Paragraph::new(format_status_tokens(app)).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(paragraph, area);
}

fn format_status_tokens(app: &TuiApp) -> String {
    let scroll_marker = if app.transcript_scroll_from_bottom > 0 {
        "  scroll=history"
    } else {
        ""
    };
    let context = format!(
        "{}:{}  mode={}  {}  {}  sandbox={}  telemetry={}  status={}{}",
        app.provider_name,
        app.model,
        app.mode.as_str(),
        app.repo.compact(),
        app.permissions.compact(),
        app.permissions.sandbox,
        app.telemetry.as_str(),
        app.status,
        scroll_marker,
    );
    let spend = format!(
        "cost={} tok={}/{} tools={} budget={}",
        format_cost(&app.cost),
        format_optional_u64(app.cost.input_tokens),
        format_optional_u64(app.cost.output_tokens),
        app.metrics.tool_calls,
        if app.metrics.budget_denials == 0 {
            "ok".to_string()
        } else {
            format!("denied:{}", app.metrics.budget_denials)
        },
    );
    let hints = if app.pending_approval.is_some() {
        "Y allow once | A user | P project | N deny | U/D deny rule | Ctrl-C cancel"
    } else {
        "Enter send | Shift-Tab mode | PgUp/PgDn/Home/End scroll | Ctrl-Y copy | /copy | Ctrl-C cancel | Esc quit"
    };
    match app.status_verbosity {
        StatusVerbosity::Compact => format!("{context}  {spend}\n{hints}"),
        StatusVerbosity::Verbose => format!(
            "{context}  {spend}\ncfg={} read={}B receipts={} redactions={} cached={} cache_write={} | {hints}",
            app.config_sources,
            app.metrics.bytes_read,
            app.metrics.receipt_stub_hits + app.metrics.negative_receipt_hits,
            app.metrics.redactions,
            format_optional_u64(app.cost.cached_input_tokens),
            format_optional_u64(app.cost.cache_write_input_tokens),
        ),
    }
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
}

fn short_commit(head: &str) -> String {
    head.chars().take(7).collect()
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
    model: String,
    mode: SessionMode,
    config_sources: String,
    status_verbosity: StatusVerbosity,
    repo: RepoStatus,
    permissions: PermissionStatus,
    telemetry: TelemetryStatus,
    input: String,
    transcript: Vec<TranscriptItem>,
    transcript_scroll_from_bottom: u16,
    pending_assistant: String,
    status: String,
    cost: squeezy_core::CostSnapshot,
    metrics: squeezy_core::TurnMetrics,
    turn_rx: Option<mpsc::Receiver<AgentEvent>>,
    cancel: Option<CancellationToken>,
    pending_approval: Option<PendingApproval>,
    clipboard: Box<dyn Clipboard>,
}

impl TuiApp {
    fn new(
        provider_name: &'static str,
        config: &AppConfig,
        mode: SessionMode,
        onboarding_summary: Option<String>,
    ) -> Self {
        Self::new_with_clipboard(
            provider_name,
            config,
            mode,
            onboarding_summary,
            Box::new(Osc52Clipboard),
        )
    }

    fn new_with_clipboard(
        provider_name: &'static str,
        config: &AppConfig,
        mode: SessionMode,
        onboarding_summary: Option<String>,
        clipboard: Box<dyn Clipboard>,
    ) -> Self {
        let mut transcript = Vec::new();
        let status = if let Some(summary) = onboarding_summary {
            // The onboarding summary is generated by Squeezy itself, not the
            // model, so it belongs to the System role. Using Assistant would
            // both mislabel provenance and mix the same color with later
            // assistant turns, making the seam ambiguous in the transcript.
            transcript.push(TranscriptItem {
                role: Role::System,
                content: summary,
            });
            "repo profile ready".to_string()
        } else {
            "ready".to_string()
        };
        Self {
            provider_name,
            model: config.model.clone(),
            mode,
            config_sources: config.config_source_labels().join(","),
            status_verbosity: config.tui.status_verbosity,
            repo: RepoStatus::detect(config),
            permissions: PermissionStatus::from_policy(&config.permissions),
            telemetry: TelemetryStatus::from_config(&config.telemetry),
            input: String::new(),
            transcript,
            transcript_scroll_from_bottom: 0,
            pending_assistant: String::new(),
            status,
            cost: squeezy_core::CostSnapshot::default(),
            metrics: squeezy_core::TurnMetrics::default(),
            turn_rx: None,
            cancel: None,
            pending_approval: None,
            clipboard,
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
