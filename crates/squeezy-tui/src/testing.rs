//! In-process driver for an external consumer (`squeezy-eval`) to
//! exercise the full TUI runtime — TuiApp, Agent, ratatui — against a
//! headless `TestBackend`. Gated behind the `testing` feature so
//! release builds of the TUI library never carry this surface.
//!
//! The harness intentionally hides `TuiApp` itself. Consumers see a
//! stable, narrow API: build, pump events, send keys, snapshot a
//! frame, read transcript / status state. The internal struct keeps
//! its 150-plus `pub(crate)` fields private to the crate.

use std::sync::Arc;

pub use crossterm::event::KeyEvent;
use crossterm::event::{KeyEventKind, KeyEventState};
use ratatui::{Terminal, backend::TestBackend};
use squeezy_agent::Agent;
use squeezy_core::{AppConfig, Result, SessionMode, SqueezyError};
use squeezy_llm::LlmProvider;

use crate::{
    Clipboard, TranscriptEntryKind, TuiApp, apply_theme_overrides, drain_agent_events,
    drain_job_events, drain_pending_diff, handle_key, keymap::parse_keyspec, render,
    start_user_turn,
};

/// Opaque driver wrapping a TuiApp + Agent + headless terminal.
pub struct TuiHarness {
    app: TuiApp,
    /// Wrapped in `Option` so `Drop` can release the `Agent` first;
    /// dropping `TuiApp` first would close the receiver while the
    /// agent's background tasks may still try to push final
    /// `TurnCompleted` / shutdown events.
    agent: Option<Agent>,
    terminal: Terminal<TestBackend>,
    width: u16,
    height: u16,
}

impl TuiHarness {
    /// Build a harness around `config + provider`. Mirrors the
    /// preamble of `run_inner` (`apply_theme_overrides` then construct
    /// TuiApp + Agent) so palette and state line up with production.
    pub fn new(
        config: AppConfig,
        mode: SessionMode,
        provider: Arc<dyn LlmProvider>,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        apply_theme_overrides(config.tui.theme);
        let agent = Agent::new(config.clone(), provider);
        let app = TuiApp::new_with_clipboard(
            "eval-harness",
            &config,
            mode,
            None,
            Box::new(NoopClipboard),
        );
        let backend = TestBackend::new(width, height);
        let terminal = Terminal::new(backend)
            .map_err(|e| SqueezyError::Terminal(format!("test backend init: {e}")))?;
        Ok(Self {
            app,
            agent: Some(agent),
            terminal,
            width,
            height,
        })
    }

    /// Mutable access to the agent for callers that need to issue
    /// commands directly (`queue_user_message`, `dispatch_command_raw`,
    /// `start_turn`, etc.).
    pub fn agent_mut(&mut self) -> &mut Agent {
        self.agent
            .as_mut()
            .expect("agent dropped before harness; this is a bug in TuiHarness")
    }

    /// Start a user turn through the same code path the TUI uses for
    /// keyboard `Enter`. Equivalent to typing `text` and pressing
    /// return — the agent stream is driven by the harness's pump.
    pub fn start_user_turn(&mut self, text: impl Into<String>) {
        let agent = self
            .agent
            .as_mut()
            .expect("agent dropped before harness; this is a bug in TuiHarness");
        start_user_turn(&mut self.app, agent, text.into());
    }

    /// Drive the same drain order `run_inner` uses (`drain_job_events`
    /// → `drain_agent_events` → optional `auto_drain_queue` →
    /// `drain_pending_diff`) until no turn is active and the prompt
    /// queue is empty. Bounded so a stuck channel surfaces as an error
    /// rather than hanging the calling scenario.
    pub async fn pump_until_idle(&mut self) -> Result<()> {
        // Bounded by wall clock, not iterations: a real LLM turn can
        // sit between deltas for seconds. `try_recv` returns
        // immediately when the queue is empty, so a pure-spin loop
        // would burn its iteration budget before the first reasoning
        // chunk even lands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            drain_job_events(&mut self.app);
            drain_agent_events(&mut self.app).await;
            let queued = if self.app.auto_drain_queue {
                self.app.auto_drain_queue = false;
                if self.app.turn_rx.is_none()
                    && let Some(next) = self.app.prompt_queue.pop_front()
                {
                    let agent = self
                        .agent
                        .as_mut()
                        .expect("agent dropped before harness; this is a bug in TuiHarness");
                    start_user_turn(&mut self.app, agent, next);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            drain_pending_diff(&mut self.app);
            if !queued && self.app.turn_rx.is_none() && self.app.prompt_queue.is_empty() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(SqueezyError::Agent(
                    "pump_until_idle: did not reach idle within 180s".into(),
                ));
            }
            // Sleep briefly so the LLM streaming task can make
            // progress; 10 ms balances latency against CPU spin.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Inject a single key event at `handle_key`. Drains both before
    /// and after so the harness sees the same "between frames" state
    /// the production event loop does.
    pub async fn send_key(&mut self, key: KeyEvent) -> Result<bool> {
        self.pump_until_idle().await?;
        let agent = self
            .agent
            .as_mut()
            .expect("agent dropped before harness; this is a bug in TuiHarness");
        let want_exit = handle_key(&mut self.app, agent, key).await?;
        self.pump_until_idle().await?;
        Ok(want_exit)
    }

    /// Inject a sequence of keys, pumping between each. Returns the
    /// last `handle_key` return value (true ⇒ caller asked to exit).
    pub async fn send_keys(&mut self, keys: &[KeyEvent]) -> Result<bool> {
        let mut last = false;
        for key in keys {
            last = self.send_key(*key).await?;
        }
        Ok(last)
    }

    /// Render one frame to the backing `TestBackend` and return its
    /// cell+text projection. Always re-renders — does not honour
    /// `app.needs_redraw`, because a key-driven snapshot needs the
    /// post-toggle screen even when the toggle handler forgot to set
    /// the flag.
    pub fn render_frame(&mut self) -> Result<FrameSnapshot> {
        let app = &self.app;
        self.terminal
            .draw(|frame| render(frame, app))
            .map_err(|e| SqueezyError::Terminal(format!("draw: {e}")))?;
        let buffer = self.terminal.backend().buffer();
        let mut plain = String::with_capacity(self.width as usize * self.height as usize);
        let mut cells = Vec::with_capacity(self.width as usize * self.height as usize);
        for y in 0..self.height {
            for x in 0..self.width {
                let cell = &buffer[(x, y)];
                let symbol = cell.symbol().to_string();
                plain.push_str(&symbol);
                cells.push(FrameCell { x, y, symbol });
            }
            plain.push('\n');
        }
        Ok(FrameSnapshot {
            width: self.width,
            height: self.height,
            plain_text: plain,
            cells,
        })
    }

    /// Current status-bar text. Toggle handlers write things like
    /// `"expanded 1 of 3"` here; assertions key off it.
    pub fn status_text(&self) -> &str {
        &self.app.status
    }

    /// Project the live transcript to a public summary list. Indexes
    /// match `app.transcript` order so a scenario can ask for
    /// "last reasoning entry" by `find().rposition()` over the result.
    pub fn transcript_entries(&self) -> Vec<TranscriptEntrySummary> {
        self.app
            .transcript
            .iter()
            .map(|entry| TranscriptEntrySummary {
                kind: transcript_kind_name(&entry.kind),
                collapsed: entry.collapsed,
                preview: transcript_preview(&entry.kind),
            })
            .collect()
    }

    /// True while a turn's `AgentEvent` channel is still attached —
    /// i.e. the model is mid-stream. Eval drivers can use this to gate
    /// "key only after turn completes" semantics.
    pub fn is_turn_active(&self) -> bool {
        self.app.turn_rx.is_some()
    }

    /// Width the harness was built with, for callers that mirror its
    /// dimensions into their own frame records.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Height the harness was built with.
    pub fn height(&self) -> u16 {
        self.height
    }
}

impl Drop for TuiHarness {
    fn drop(&mut self) {
        // Release the Agent's sender side first so any final flush on
        // the TuiApp end sees a cleanly-closed channel rather than an
        // active producer mid-write.
        drop(self.agent.take());
    }
}

/// Whole-screen snapshot returned by `render_frame`. Plain text
/// preserves trailing spaces per row so column diffs stay visible.
#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    pub width: u16,
    pub height: u16,
    pub plain_text: String,
    pub cells: Vec<FrameCell>,
}

/// Single rendered cell. Style is intentionally omitted from this v1
/// surface — consumers can pull the full styled buffer in a follow-up
/// when they actually need fg/bg/modifier deltas.
#[derive(Debug, Clone)]
pub struct FrameCell {
    pub x: u16,
    pub y: u16,
    pub symbol: String,
}

/// Public projection of `TranscriptEntry`. `kind` is a string tag
/// instead of the internal enum so consumers don't depend on its
/// private representation.
#[derive(Debug, Clone)]
pub struct TranscriptEntrySummary {
    /// One of `message | tool_result | log | plan_card | diff |
    /// reasoning | slash_echo`.
    pub kind: &'static str,
    pub collapsed: bool,
    /// First ~80 characters of the entry's primary text, with an
    /// ellipsis suffix when truncated. Empty for entry kinds whose
    /// text is structured (tool_result, plan_card, diff).
    pub preview: String,
}

fn transcript_kind_name(kind: &TranscriptEntryKind) -> &'static str {
    match kind {
        TranscriptEntryKind::Message(_) => "message",
        TranscriptEntryKind::ToolResult(_) => "tool_result",
        TranscriptEntryKind::Log(_) => "log",
        TranscriptEntryKind::PlanCard(_) => "plan_card",
        TranscriptEntryKind::Diff(_) => "diff",
        TranscriptEntryKind::Reasoning(_) => "reasoning",
        TranscriptEntryKind::SlashEcho(_) => "slash_echo",
    }
}

fn transcript_preview(kind: &TranscriptEntryKind) -> String {
    let text = match kind {
        TranscriptEntryKind::Message(item) => item.content.as_str(),
        TranscriptEntryKind::Reasoning(snap) => snap.display_text.as_str(),
        TranscriptEntryKind::SlashEcho(echo) => echo.cmd.as_str(),
        _ => "",
    };
    let mut preview: String = text.chars().take(80).collect();
    if text.chars().count() > 80 {
        preview.push('…');
    }
    preview
}

/// Translate a keymap-style spec (`"Ctrl+O"`, `"Alt+Up"`, `"PageDown"`,
/// `"F11"`, `"Enter"`) into a crossterm `KeyEvent`. Reuses
/// `keymap::parse_keyspec` so the dialect cannot drift from the
/// `[tui.keymap]` config schema.
pub fn parse_key(spec: &str) -> Option<KeyEvent> {
    let binding = parse_keyspec(spec)?;
    Some(KeyEvent {
        code: binding.code,
        modifiers: binding.modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

struct NoopClipboard;

impl Clipboard for NoopClipboard {
    fn copy_text(&mut self, _text: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}
