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

// Heavy, term-matrix-only surface: the real-ANSI capture harness
// (`drive_scenario`) pulls the `termsim` capture types and the
// crate-private append-only paint helpers. Gated so the default
// `testing` build never compiles it.
#[cfg(feature = "term-matrix")]
use std::io::Write as _;
#[cfg(feature = "term-matrix")]
use std::sync::Mutex;

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
    /// `/permissions`, `/effort`, `/tool-verbosity`, `/theme`,
    /// `/statusline`, `/keymap`, `/help`, etc.)
    /// from an eval driver — those commands
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
            crate::clear_mcp_elicitation_seeded_input(&mut self.app);
        }
        self.app.status = format_mcp_elicitation_status_line(&request);
        self.app.mcp_elicitation_selection_index = 0;
        crate::seed_mcp_elicitation_form_input(&mut self.app, &request);
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

/// Real-ANSI capture harness (§8 term-matrix). Gated behind
/// `term-matrix` because it pulls the `termsim` capture types and the
/// crate-private append-only paint helpers from `lib.rs`. The default
/// `testing` build (and `cargo test -p squeezy-tui`) never compile any
/// of this.
#[cfg(feature = "term-matrix")]
mod capture {
    // `drive_scenario` / `capture_paint` are exercised only by the
    // term-matrix `#[test]` (and, once it lands, the matrix runner), so
    // the lib-crate build with `term-matrix` but without `cfg(test)`
    // sees them as dead. This mirrors the tree-wide allowance the
    // sibling `termsim` scaffold already carries until the runner wires
    // them in.
    #![allow(dead_code)]

    use super::*;
    use crate::size_source::FixedSize;
    use crate::termsim::{CaptureLog, FrameMark, Step};
    use crossterm::execute;
    use crossterm::terminal::{Clear, ClearType, DisableLineWrap, EnableLineWrap};
    use ratatui::backend::CrosstermBackend;

    /// Inline viewport height the production guard pins itself to; the
    /// capture terminal mirrors it so ratatui's inline construction
    /// behaves identically. Kept in sync with `lib.rs`'s constant.
    const INLINE_VIEWPORT_HEIGHT: u16 = crate::INLINE_VIEWPORT_HEIGHT;

    /// Mutable paint bookkeeping the append-only renderer threads across
    /// frames. Mirrors the subset of `TerminalGuard` state that
    /// `paint_main` reads/writes: which history has flushed, the last
    /// flushed turn-divider generation, and whether a footer is on
    /// screen (so the next frame erases it). A standalone struct keeps
    /// this off the public `TuiHarness` surface.
    #[derive(Default)]
    struct PaintState {
        startup_flushed: bool,
        transcript_flushed_len: usize,
        turn_divider_flushed_generation: Option<u64>,
        footer_painted: bool,
    }

    impl TuiHarness {
        /// Drive a scripted [`Step`] sequence against the real
        /// append-only paint path, capturing every emitted byte into a
        /// [`CaptureLog`] and recording a [`FrameMark`] per `Step::Frame`.
        ///
        /// The harness builds a ratatui `Terminal` over a
        /// `CrosstermBackend` whose writer is a `TerminalWriter::Capture`
        /// sink, so the captured bytes are the exact ANSI the TUI would
        /// emit to a real terminal — not a `TestBackend` cell diff. Steps
        /// route through the existing `TuiApp` + `Agent` plumbing:
        ///
        /// * [`Step::Key`] → `handle_key` (pump idle before/after, like
        ///   [`send_key`](TuiHarness::send_key)).
        /// * [`Step::Paste`] → injected the way the paste handler receives it.
        /// * [`Step::Resize`] → swaps the [`FixedSize`] source to `(w, h)`,
        ///   sets `app.pending_resize`, and `terminal.resize`s the backing
        ///   ratatui terminal, mirroring `Event::Resize`.
        /// * [`Step::Tick`] / [`Step::SettleTurn`] → one / full
        ///   `pump_until_idle` pass.
        /// * [`Step::AssistantDelta`] → pushes assistant text the way a
        ///   committed model turn lands, driving the streaming/commit surface.
        /// * [`Step::ToolOutput`] → injects a tool-output transcript entry.
        /// * [`Step::OpenOverlay`] / [`Step::CloseOverlay`] → toggles the
        ///   fullscreen transcript overlay.
        /// * [`Step::Frame`] → forces one append-only paint of the current
        ///   state and records a `FrameMark{byte_offset, w, h}` at the
        ///   current sink length.
        ///
        /// Returns the accumulated [`CaptureLog`]: `bytes` is the verbatim
        /// stream read out of the `Capture` sink, `frames` is one mark per
        /// painted frame in order. Frame *i*'s bytes are
        /// `bytes[frames[i-1].byte_offset .. frames[i].byte_offset]`.
        ///
        /// `pub(crate)`: the parameter and return types are the
        /// crate-private `termsim` capture shapes, and the only callers
        /// are the in-crate matrix runner and tests — not the external
        /// `squeezy-eval` driver, which uses the `pub` snapshot API above.
        pub(crate) async fn drive_scenario(&mut self, steps: &[Step]) -> Result<CaptureLog> {
            // In-memory sink shared with the crossterm backend; the
            // capture writer tees every emitted byte here.
            let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
            let writer = crate::terminal_writer::TerminalWriter::capture(sink.clone());
            let backend = CrosstermBackend::new(writer);
            // Default (`Fullscreen`) viewport: the append-only path writes
            // raw bytes straight to the backend and never calls
            // `draw`/`insert_before`, so the viewport choice does not
            // affect the captured stream. Crucially, `Terminal::new`
            // avoids the inline-viewport cursor-position query that
            // `Viewport::Inline` performs at construction — that query
            // fails on a headless `Capture` writer with no real TTY.
            let mut terminal = Terminal::new(backend)
                .map_err(|e| SqueezyError::Terminal(format!("capture terminal init: {e}")))?;

            // Scripted terminal size: `Resize` steps mutate this, and the
            // paint reads it instead of crossterm's global `size()`.
            let mut size = FixedSize(self.width, self.height);
            let mut paint = PaintState::default();
            let mut log = CaptureLog::default();

            for step in steps {
                match step {
                    Step::Key(key) => {
                        let _ = self.send_key(*key).await?;
                    }
                    Step::Paste(text) => {
                        // Route paste the same way the production handler
                        // does: feed it at the composer, then pump.
                        self.pump_until_idle().await?;
                        let agent = self
                            .agent
                            .as_mut()
                            .expect("agent dropped before harness; bug in TuiHarness");
                        crate::handle_paste(&mut self.app, agent, text.clone()).await?;
                        self.pump_until_idle().await?;
                    }
                    Step::Resize(w, h) => {
                        size = FixedSize(*w, *h);
                        self.width = *w;
                        self.height = *h;
                        self.app.pending_resize = true;
                        // Mirror `Event::Resize`: the backing ratatui
                        // terminal learns the new dimensions so any
                        // inline bookkeeping (and a future `draw`) reflows.
                        let _ = terminal.resize(ratatui::layout::Rect::new(0, 0, *w, *h));
                    }
                    Step::Tick => {
                        self.pump_until_idle().await?;
                    }
                    Step::SettleTurn => {
                        // Pump to completion so the turn settles and
                        // history flushes — the settle boundary that gates
                        // the history commit.
                        self.pump_until_idle().await?;
                    }
                    Step::AssistantDelta(text) => {
                        // Land assistant text as a committed turn the way
                        // the model would, then pump so the streaming
                        // surface settles into a flushable transcript entry.
                        self.app
                            .push_transcript_item(squeezy_core::TranscriptItem::assistant(
                                text.clone(),
                            ));
                        self.pump_until_idle().await?;
                    }
                    Step::ToolOutput(text) => {
                        // A completed tool call lands as a transcript log
                        // line; the append-only path flushes it to history.
                        self.app.push_log(text.clone());
                        self.pump_until_idle().await?;
                    }
                    Step::OpenOverlay => {
                        self.app.transcript_overlay =
                            Some(crate::TranscriptOverlayState::default());
                    }
                    Step::CloseOverlay => {
                        self.app.transcript_overlay = None;
                    }
                    Step::Mouse => {
                        // Mouse routing is not wired yet (the scenario
                        // model documents it as a future surface); a no-op
                        // keeps the byte stream identical to a session that
                        // received no mouse input.
                    }
                    Step::Frame => {
                        self.pump_until_idle().await?;
                        capture_paint(&mut terminal, &mut self.app, size, &mut paint)?;
                        // Record the mark AFTER the paint flushed: the
                        // sink length is this frame's end offset.
                        let byte_offset = sink
                            .lock()
                            .map(|buf| buf.len())
                            .unwrap_or_else(|poison| poison.into_inner().len());
                        log.frames.push(FrameMark {
                            byte_offset,
                            w: size.0,
                            h: size.1,
                        });
                    }
                }
            }

            // Tee the full byte stream out of the sink into the log.
            log.bytes = sink
                .lock()
                .map(|buf| buf.clone())
                .unwrap_or_else(|poison| poison.into_inner().clone());
            Ok(log)
        }
    }

    /// One append-only frame, byte-for-byte the same compound op
    /// `TerminalGuard::paint_main` emits, but driven by an injected
    /// [`FixedSize`] instead of crossterm's global `terminal_size()` so a
    /// scripted `Resize` actually changes what the paint reads. Reuses
    /// the crate-private renderer helpers so the captured ANSI matches
    /// production exactly.
    fn capture_paint(
        terminal: &mut Terminal<CrosstermBackend<crate::terminal_writer::TerminalWriter>>,
        app: &mut TuiApp,
        size: FixedSize,
        paint: &mut PaintState,
    ) -> Result<()> {
        use crate::size_source::SizeSource as _;
        let (w, h) = size
            .size()
            .map_err(|e| SqueezyError::Terminal(e.to_string()))?;
        if w == 0 || h == 0 {
            return Ok(());
        }
        // Match `paint_main`: render two columns narrower so a glyph the
        // terminal draws wider than unicode-width can't overflow.
        let content_w = w.saturating_sub(2).max(1);

        // History flush bookkeeping (the `prepare_history` body, inlined
        // so the paint state lives in `PaintState` rather than the guard).
        let flush_to = crate::settling_flush_boundary(app);
        let (history_lines, flushed_divider_generation) =
            crate::inline_history_lines_for_flush_with_turn_divider(
                app,
                content_w,
                !paint.startup_flushed,
                paint.transcript_flushed_len,
                flush_to,
                paint.turn_divider_flushed_generation,
            );
        if let Some(generation) = flushed_divider_generation {
            paint.turn_divider_flushed_generation = Some(generation);
        }
        paint.startup_flushed = true;
        paint.transcript_flushed_len = flush_to;

        let divider_gen = paint.turn_divider_flushed_generation;
        let footer = crate::render_footer_to_buffer(
            app,
            content_w,
            INLINE_VIEWPORT_HEIGHT.min(h),
            divider_gen,
        );
        let footer_h = crate::capped_footer_height(crate::footer_content_height(&footer), h);
        let history = (!history_lines.is_empty())
            .then(|| crate::render_lines_to_owned_buffer(&history_lines, content_w));

        let had_footer = paint.footer_painted;
        let backend = terminal.backend_mut();
        let body = (|| -> std::io::Result<()> {
            // Capture path always wraps in the DEC 2026 synchronized
            // update, matching a synchronized-capable production terminal.
            backend.write_all(crate::BEGIN_SYNCHRONIZED_UPDATE.as_bytes())?;
            if had_footer {
                execute!(backend, Clear(ClearType::FromCursorDown))?;
            }
            if let Some(hist) = history.as_ref() {
                for y in 0..hist.area.height {
                    backend.write_all(b"\r")?;
                    crate::emit_buffer_row_styled(backend, hist, y, content_w)?;
                    backend.write_all(b"\r\n")?;
                }
            }
            execute!(backend, DisableLineWrap)?;
            for y in 0..footer_h {
                backend.write_all(b"\r")?;
                crate::emit_buffer_row_styled(backend, &footer, y, content_w)?;
                execute!(backend, Clear(ClearType::UntilNewLine))?;
                if y + 1 < footer_h {
                    backend.write_all(b"\r\n")?;
                }
            }
            execute!(backend, EnableLineWrap)?;
            if footer_h > 1 {
                execute!(backend, crossterm::cursor::MoveToPreviousLine(footer_h - 1))?;
            } else {
                execute!(backend, crossterm::cursor::MoveToColumn(0))?;
            }
            Ok(())
        })();
        let _ = execute!(backend, EnableLineWrap);
        let _ = backend.write_all(crate::END_SYNCHRONIZED_UPDATE.as_bytes());
        let flushed = backend.flush();

        paint.footer_painted = true;
        app.footer_origin = h.saturating_sub(footer_h);
        body.and(flushed)
            .map_err(|e| SqueezyError::Terminal(e.to_string()))
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
