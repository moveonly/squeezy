//! In-process driver for an external consumer (`squeezy-eval`) to
//! exercise the full TUI runtime — TuiApp, Agent, ratatui — against a
//! headless `TestBackend`. Gated behind the `testing` feature so
//! release builds of the TUI library never carry this surface.
//!
//! The harness intentionally hides `TuiApp` itself. Consumers see a
//! stable, narrow API: build, pump events, send keys, snapshot a
//! frame, read transcript / status state. The internal struct keeps
//! its 150-plus `pub(crate)` fields private to the crate.

use std::path::PathBuf;
use std::sync::Arc;

pub use crossterm::event::KeyEvent;
use crossterm::event::{KeyEventKind, KeyEventState};
use ratatui::style::{Color, Modifier};
use ratatui::{Terminal, backend::TestBackend};
use squeezy_agent::{Agent, ToolApprovalDecision};
use squeezy_core::{AppConfig, Result, Role, SessionMode, SqueezyError};
use squeezy_llm::LlmProvider;

use squeezy_tools::{McpElicitationKind, McpElicitationRequest, McpElicitationResponse};
use tokio::sync::oneshot;

use crate::{
    Clipboard, PendingMcpElicitation, TranscriptEntryKind, TuiApp, apply_theme_overrides,
    drain_agent_events, drain_job_events, drain_pending_diff, format_mcp_elicitation_status_line,
    handle_key, handle_slash_command, keymap::parse_keyspec, render, start_user_turn,
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
    ///
    /// `settings_path_override` pins the user-scope settings file that
    /// in-session slash commands (`/theme`, `/statusline`, …) persist
    /// into. The eval driver passes a per-run scratch path so scenarios
    /// can't clobber the operator's real `~/.squeezy/settings.toml`;
    /// `None` keeps the production fallback
    /// (`squeezy_core::default_settings_path`).
    pub fn new(
        config: AppConfig,
        mode: SessionMode,
        provider: Arc<dyn LlmProvider>,
        width: u16,
        height: u16,
        settings_path_override: Option<PathBuf>,
    ) -> Result<Self> {
        apply_theme_overrides(&config);
        // Mirror production (`crates/squeezy-tui/src/lib.rs:525`): the
        // banner / status-line provider label comes from the live
        // provider, not a harness literal. `Agent::provider_name()` is
        // defined as `self.provider.name()`, so reading it directly off
        // the trait keeps eval and production in lock-step.
        let provider_name = provider.name();
        let agent = Agent::new(config.clone(), provider);
        let mut app =
            TuiApp::new_with_clipboard(provider_name, &config, mode, None, Box::new(NoopClipboard));
        app.set_settings_path_override(settings_path_override);
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

    /// In-crate access to the wrapped [`TuiApp`] for internal unit
    /// tests that need to inspect or drive private TUI state directly
    /// (e.g. asserting `settings_path_override` plumbing). Not part of
    /// the public eval-driver API — external callers go through
    /// [`send_keys`](Self::send_keys) and [`render_frame`](Self::render_frame).
    #[cfg(test)]
    pub(crate) fn app_mut(&mut self) -> &mut TuiApp {
        &mut self.app
    }

    /// In-crate access to both [`TuiApp`] and [`Agent`] in one borrow,
    /// for unit tests that need to drive `handle_slash_command` (which
    /// takes both mutably) against the harness's internal state without
    /// going through the async key-pump.
    #[cfg(test)]
    pub(crate) fn app_and_agent_mut(&mut self) -> (&mut TuiApp, &mut Agent) {
        let agent = self
            .agent
            .as_mut()
            .expect("agent dropped before harness; this is a bug in TuiHarness");
        (&mut self.app, agent)
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
    ///
    /// "Idle" includes any open operator-input modal: a parked
    /// `pending_approval`, `pending_mcp_elicitation`, or
    /// `pending_request_user_input` is the agent waiting on the
    /// scenario driver, not a deadlock. Returning early lets the driver
    /// drain its queued `Approve`/`Deny`/`RespondElicitation`/
    /// `RespondUserInput` action and feed the response back through
    /// the matching `respond_*` method before pumping again.
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
            let waiting_on_operator = self.app.pending_approval.is_some()
                || self.app.pending_mcp_elicitation.is_some()
                || self.app.pending_request_user_input.is_some();
            if waiting_on_operator {
                return Ok(());
            }
            // Keep pumping while a spawn_blocking diff task is still
            // outstanding — otherwise an immediate idle return drops
            // the scenario back before `drain_pending_diff` had a
            // chance to land the result. squeezy-nyg8.2.
            let waiting_on_pending_diff = self.app.pending_diff.is_some();
            if !queued
                && self.app.turn_rx.is_none()
                && self.app.prompt_queue.is_empty()
                && !waiting_on_pending_diff
            {
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

    /// True iff the agent is parked on a `pending_approval` slot —
    /// the harness needs the scenario driver to issue an
    /// `Approve`/`Deny` (routed through `respond_approval` /
    /// `respond_deny`) before the next pump can make progress.
    pub fn has_pending_approval(&self) -> bool {
        self.app.pending_approval.is_some()
    }

    /// Name of the tool the agent is waiting approval for, if any.
    /// Exposed so the eval driver can match a queued
    /// `Action::Approve { match: { tool } }` against the live
    /// request without peeking at private TuiApp state.
    pub fn pending_approval_tool(&self) -> Option<&str> {
        self.app
            .pending_approval
            .as_ref()
            .map(|p| p.request.tool_name.as_str())
    }

    /// Approve the currently-parked approval request with the same
    /// `ToolApprovalDecision::Approved` the non-drive_tui driver
    /// emits from `Driver::decide_approval`. Returns true when a slot
    /// was actually consumed; false if nothing was pending so the
    /// caller can record an `unfired` outcome instead of silently
    /// pretending it worked.
    pub fn respond_approval(&mut self) -> bool {
        let Some(pending) = self.app.pending_approval.take() else {
            return false;
        };
        let tool = pending.request.tool_name.clone();
        let _ = pending.decision_tx.send(ToolApprovalDecision::Approved);
        self.app.status = format!("approved {tool}");
        true
    }

    /// Deny the currently-parked approval request with
    /// `ToolApprovalDecision::Denied`. Mirrors `respond_approval`'s
    /// return contract: true when a slot was consumed.
    pub fn respond_deny(&mut self) -> bool {
        let Some(pending) = self.app.pending_approval.take() else {
            return false;
        };
        let tool = pending.request.tool_name.clone();
        let _ = pending.decision_tx.send(ToolApprovalDecision::Denied);
        self.app.status = format!("denied {tool}");
        true
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

    /// Dispatch a slash command (e.g. `"/config"`, `"/permissions"`,
    /// `"/effort high"`) through the TUI's `handle_slash_command`
    /// path — the same code path a user typing the command into the
    /// composer would take. This is the only way to exercise
    /// `DispatchOutcome::TuiOnly` commands (`/config`, `/model`,
    /// `/permissions`, `/effort`, `/verbosity`, `/tool-verbosity`,
    /// `/theme`, `/statusline`, `/keymap`, `/collapse`, `/expand`,
    /// `/copy`, `/help`, etc.) from an eval driver — those commands
    /// short-circuit `Agent::dispatch_command_raw` and never reach the
    /// TUI.
    ///
    /// Returns `true` when the input parsed as a slash command (a
    /// known head OR a registered prompt template); `false` when the
    /// input is empty or non-slash. Pumps the harness before and
    /// after so any agent-side side effect of the command (mode
    /// change, transcript push, status update) is reflected before
    /// the caller asserts.
    pub async fn dispatch_slash_command(&mut self, input: &str) -> Result<bool> {
        self.pump_until_idle().await?;
        let agent = self
            .agent
            .as_mut()
            .expect("agent dropped before harness; this is a bug in TuiHarness");
        let routed = handle_slash_command(&mut self.app, agent, input).await;
        self.pump_until_idle().await?;
        Ok(routed)
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

    /// Inject a synthesized `McpElicitationRequest` straight into the
    /// TUI's `pending_mcp_elicitation` slot. Bypasses the
    /// MCP transport and `install_mcp_elicitation_handler` round-trip
    /// so a scenario can exercise the modal layer
    /// (`format_mcp_elicitation_menu_lines`, key-driven Accept/Decline/
    /// Cancel routing) without standing up a fake MCP server. The
    /// returned receiver resolves when the user (driven by `send_key`)
    /// answers the modal; dropping it is harmless.
    ///
    /// Mirrors the production handler in
    /// `events.rs:286` (`AgentEvent::McpElicitationRequested`) so the
    /// resulting `TuiApp` state — `status` line, selection index reset,
    /// previous-request cancellation — matches a real elicitation.
    pub fn push_pending_mcp_elicitation(
        &mut self,
        request: McpElicitationRequest,
    ) -> oneshot::Receiver<McpElicitationResponse> {
        let (response_tx, response_rx) = oneshot::channel();
        if let Some(previous) = self.app.pending_mcp_elicitation.take() {
            let _ = previous.response_tx.send(McpElicitationResponse::cancel());
        }
        self.app.status = format_mcp_elicitation_status_line(&request);
        self.app.mcp_elicitation_selection_index = 0;
        self.app.pending_mcp_elicitation = Some(PendingMcpElicitation {
            request,
            response_tx,
        });
        response_rx
    }

    /// Construct an `McpElicitationRequest` from the loose fields a
    /// scenario author supplies. Centralizes the request_id /
    /// elicitation_id defaulting so callers don't have to spell out
    /// fields the modal doesn't read.
    pub fn make_mcp_elicitation_request(
        server: impl Into<String>,
        kind: McpElicitationKind,
        message: impl Into<String>,
        schema: Option<serde_json::Value>,
        url: Option<String>,
    ) -> McpElicitationRequest {
        McpElicitationRequest {
            server: server.into(),
            request_id: "eval-inject".into(),
            kind,
            message: message.into(),
            schema,
            url,
            elicitation_id: None,
        }
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
                cells.push(FrameCell {
                    x,
                    y,
                    symbol,
                    fg: color_name(cell.fg),
                    bg: color_name(cell.bg),
                    modifiers: modifier_names(cell.modifier),
                });
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

    /// Full content of the most recent `Role::Assistant` Message entry,
    /// or empty when the harness has no assistant turn yet. Unlike
    /// `transcript_entries(...).preview`, this returns the entry text
    /// untruncated so eval scenarios can run `final_text_contains` and
    /// frame-record reconstruction on the harness-driven path.
    pub fn last_assistant_text(&self) -> String {
        self.app
            .transcript
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                TranscriptEntryKind::Message(item) if item.role == Role::Assistant => {
                    Some(item.content.clone())
                }
                _ => None,
            })
            .unwrap_or_default()
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

    /// Slug for the currently-focused section in the config_screen
    /// modal, or `None` when no config_screen is open. Maps onto
    /// `squeezy_core::config_schema::SectionId::slug` so scenarios
    /// can disambiguate `/config` vs `/model` (`"models"`) vs
    /// `/permissions` (`"permissions"`) when `current_modal()` only
    /// reports `"config"` for all three. squeezy-qr9e (audit H2).
    pub fn config_section(&self) -> Option<&'static str> {
        self.app
            .config_screen
            .as_ref()
            .map(|state| state.current_section().id.slug())
    }

    /// Stable identifier for the modal/overlay currently occupying the
    /// foreground, or `None` when the composer holds focus. The
    /// returned id is the audit-stable name an eval scenario asserts
    /// against (`"approval"`, `"mcp_elicitation"`, `"config"`,
    /// `"model"`, etc.). Priority follows the production
    /// `app.has_modal_focus` chain — top-z modals (`approval`,
    /// `mcp_elicitation`, `user_input`) shadow the lower-z pickers.
    pub fn current_modal(&self) -> Option<&'static str> {
        if self.app.pending_approval.is_some() {
            return Some("approval");
        }
        if self.app.pending_mcp_elicitation.is_some() {
            return Some("mcp_elicitation");
        }
        if self.app.pending_request_user_input.is_some() {
            return Some("user_input");
        }
        if self.app.transcript_overlay.is_some() {
            return Some("transcript_overlay");
        }
        if self.app.config_screen.is_some() {
            return Some("config");
        }
        if self.app.status_line_setup.is_some() {
            return Some("statusline");
        }
        if let Some(overlay) = self.app.overlay.as_ref() {
            return Some(match overlay {
                crate::overlay::Overlay::Model(_) => "model",
            });
        }
        if self.app.prompt_queue_overlay.is_some() {
            return Some("prompt_queue");
        }
        if self.app.pending_plan_choice.is_some() {
            return Some("plan_choice");
        }
        if self.app.pending_feedback.is_some() {
            return Some("feedback");
        }
        if self.app.pending_report.is_some() {
            return Some("report");
        }
        None
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

/// Single rendered cell. `fg` / `bg` are stringified ratatui colors
/// (`"red"`, `"dark_gray"`, `"rgb(215,147,52)"`, `"indexed(7)"`) and
/// are `None` when the cell carries `Color::Reset` (i.e. inherits the
/// terminal default). `modifiers` lists active text modifiers (`"bold"`,
/// `"dim"`, ...). Consumers needing the raw ratatui `Color` enum can
/// parse `rgb(R,G,B)` via [`parse_rgb`].
#[derive(Debug, Clone)]
pub struct FrameCell {
    pub x: u16,
    pub y: u16,
    pub symbol: String,
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub modifiers: Vec<String>,
}

/// Stringify a ratatui `Color` for [`FrameCell`]. `Color::Reset` returns
/// `None` so consumers can distinguish "use terminal default" from any
/// explicit color choice. Mirrors the encoding used by
/// `squeezy_eval::tui_capture::TuiCell` so both surfaces share scenario
/// assertions.
fn color_name(c: Color) -> Option<String> {
    if c == Color::Reset {
        return None;
    }
    Some(match c {
        Color::Reset => "reset".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "dark_gray".into(),
        Color::LightRed => "light_red".into(),
        Color::LightGreen => "light_green".into(),
        Color::LightYellow => "light_yellow".into(),
        Color::LightBlue => "light_blue".into(),
        Color::LightMagenta => "light_magenta".into(),
        Color::LightCyan => "light_cyan".into(),
        Color::White => "white".into(),
        Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
        Color::Indexed(i) => format!("indexed({i})"),
    })
}

fn modifier_names(modifier: Modifier) -> Vec<String> {
    let mut out = Vec::new();
    for (flag, name) in [
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINED, "underlined"),
        (Modifier::SLOW_BLINK, "slow_blink"),
        (Modifier::RAPID_BLINK, "rapid_blink"),
        (Modifier::REVERSED, "reversed"),
        (Modifier::HIDDEN, "hidden"),
        (Modifier::CROSSED_OUT, "crossed_out"),
    ] {
        if modifier.contains(flag) {
            out.push(name.to_string());
        }
    }
    out
}

/// Resolve a stringified color (from [`FrameCell::fg`] / `bg`) to its
/// approximate sRGB triple. Named ratatui colors map to their common
/// 8-bit palette equivalents; `rgb(R,G,B)` parses directly; `indexed`
/// colors and unknown names return `None` (caller can decide whether
/// to skip or warn). Used by eval-side luminance assertions so the
/// rubric in `EVAL_COVERAGE_PLAN_WAVE2.md` (`0.299R + 0.587G + 0.114B
/// > 160` is a finding) can run against any [`FrameCell`].
pub fn cell_rgb(name: &str) -> Option<(u8, u8, u8)> {
    if let Some(rgb) = parse_rgb(name) {
        return Some(rgb);
    }
    Some(match name {
        "black" => (0, 0, 0),
        "red" => (170, 0, 0),
        "green" => (0, 170, 0),
        "yellow" => (170, 85, 0),
        "blue" => (0, 0, 170),
        "magenta" => (170, 0, 170),
        "cyan" => (0, 170, 170),
        "gray" => (170, 170, 170),
        "dark_gray" => (85, 85, 85),
        "light_red" => (255, 85, 85),
        "light_green" => (85, 255, 85),
        "light_yellow" => (255, 255, 85),
        "light_blue" => (85, 85, 255),
        "light_magenta" => (255, 85, 255),
        "light_cyan" => (85, 255, 255),
        "white" => (255, 255, 255),
        _ => return None,
    })
}

/// Parse the `rgb(R,G,B)` form emitted by [`color_name`]. Returns
/// `None` for any other shape (named color, `indexed(...)`, malformed
/// input) so callers can chain through [`cell_rgb`].
pub fn parse_rgb(name: &str) -> Option<(u8, u8, u8)> {
    let inner = name.strip_prefix("rgb(")?.strip_suffix(')')?;
    let mut parts = inner.split(',');
    let r: u8 = parts.next()?.trim().parse().ok()?;
    let g: u8 = parts.next()?.trim().parse().ok()?;
    let b: u8 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

/// Rec. 601 luminance of an sRGB triple, rounded to the nearest u8.
/// `0.299R + 0.587G + 0.114B`. Wave-2 palette guardrails treat any
/// rendered cell whose luminance exceeds ~160 as a finding.
pub fn rgb_luminance(rgb: (u8, u8, u8)) -> u8 {
    let (r, g, b) = (f64::from(rgb.0), f64::from(rgb.1), f64::from(rgb.2));
    (0.299 * r + 0.587 * g + 0.114 * b)
        .round()
        .clamp(0.0, 255.0) as u8
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

#[cfg(test)]
#[path = "testing_tests.rs"]
mod tests;
