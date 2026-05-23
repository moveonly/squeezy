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
use squeezy_agent::{Agent, AgentEvent};
use squeezy_core::{AppConfig, Result, Role, SqueezyError, TranscriptItem};
use squeezy_llm::LlmProvider;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub async fn run(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let agent = Agent::new(config.clone(), provider);
    let mut app = TuiApp::new(agent.provider_name(), config.model.clone());

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        drain_agent_events(&mut app).await;
        if poll_input(&mut app, &agent, config.tick_rate).await? {
            break;
        }
    }

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
                AgentEvent::Completed { message, cost, .. } => {
                    app.transcript.push(message);
                    app.pending_assistant.clear();
                    app.cost = cost;
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
            app.input.push(ch);
            Ok(false)
        }
        _ => Ok(false),
    }
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
    let tokens = format!(
        "provider={} model={} status={} in={} out={} cached={} | Enter send | Ctrl-C cancel/quit | Esc quit",
        app.provider_name,
        app.model,
        app.status,
        app.cost
            .input_tokens
            .map_or("-".to_string(), |value| value.to_string()),
        app.cost
            .output_tokens
            .map_or("-".to_string(), |value| value.to_string()),
        app.cost
            .cached_input_tokens
            .map_or("-".to_string(), |value| value.to_string()),
    );
    frame.render_widget(Paragraph::new(tokens), area);
}

struct TuiApp {
    provider_name: &'static str,
    model: String,
    input: String,
    transcript: Vec<TranscriptItem>,
    pending_assistant: String,
    status: String,
    cost: squeezy_core::CostSnapshot,
    turn_rx: Option<mpsc::Receiver<AgentEvent>>,
    cancel: Option<CancellationToken>,
}

impl TuiApp {
    fn new(provider_name: &'static str, model: String) -> Self {
        Self {
            provider_name,
            model,
            input: String::new(),
            transcript: Vec::new(),
            pending_assistant: String::new(),
            status: "ready".to_string(),
            cost: squeezy_core::CostSnapshot::default(),
            turn_rx: None,
            cancel: None,
        }
    }
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
