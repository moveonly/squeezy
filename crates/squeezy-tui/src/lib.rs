use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
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
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
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
    widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap},
};
#[cfg(test)]
use squeezy_agent::RequestUserInputChoice;
use squeezy_agent::{
    Agent, AgentEvent, JobEvent, JobId, JobNotification, JobSnapshot, MAX_JOB_NOTIFICATIONS,
    PendingConfigSwap, RequestUserInputRequest, RequestUserInputResponse, ToolApprovalDecision,
    ToolApprovalRequest,
};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextCompactionRecord, ContextCompactionState, ContextEstimate,
    PermissionCapability, PermissionPolicy, ResponseVerbosity, Result, Role, SessionMode,
    SqueezyError, StatusVerbosity, TaskStateSnapshot, TelemetryConfig, ToolOutputVerbosity,
    TranscriptDefault, TranscriptItem, TuiAlternateScreen, TuiTheme,
};
use squeezy_llm::LlmProvider;
use squeezy_store::{BugReportBundle, BugReportOptions, SessionQuery};
use squeezy_telemetry::PreparedFeedback;
use squeezy_tools::{
    McpElicitationKind, McpElicitationRequest, McpElicitationResponse, McpServerStatus,
    McpStatusSnapshot, ToolCall, ToolResult, ToolStatus,
};
use squeezy_vcs::{DiffMode, DiffOptions, GitVcs, VcsKind};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod approval;
mod commands;
mod config_screen;
mod events;
mod history;
mod input;
mod keymap;
mod mention;
mod notification;
mod overlay;
mod proposed_plan;
mod render;
mod resume_picker;
mod settings_watcher;
mod status;
mod status_line_setup;
mod streaming;
mod streaming_patch;
mod toast;
pub use render::markdown::render_markdown;
pub use streaming_patch::{JsonPatchPreviewParser, PatchPreviewEvent};

#[cfg(test)]
pub(crate) use events::apply_mcp_status_update;
pub(crate) use events::{drain_agent_events, drain_job_events};
#[cfg(test)]
pub(crate) use input::set_input;
pub(crate) use input::{HistoryDirection, SLASH_COMMANDS, SelectionDirection, SlashCommand};
use input::{
    clear_input, complete_selected_slash_command, delete_at_cursor, delete_before_cursor,
    delete_next_word, delete_previous_word, delete_to_line_end, delete_to_line_start,
    handle_mention_popup_key, handle_overlay_key, handle_request_user_input_key, input_cursor,
    insert_input_char, insert_input_text, move_input_cursor_left, move_input_cursor_line_end,
    move_input_cursor_line_start, move_input_cursor_right, move_input_cursor_word_left,
    move_input_cursor_word_right, move_slash_menu_selection, push_input_history,
    recall_prompt_history, reject_unknown_slash_command, slash_suggestions,
};

use notification::{DesktopNotifier, NotificationQueue, Severity as NotifySeverity};
use render::palette::{
    AMBER, ERROR_RED, GOLD, MODE_BUILD_GREEN, MODE_PURPLE, PROMPT_BG, QUIET, SUCCESS_GREEN,
    WORKING_SHIMMER_HIGHLIGHT, blend_color,
};
#[cfg(test)]
use render::palette::{DIFF_ADD_FG, DIFF_DEL_FG};
use toast::ToastQueue;

const INLINE_PASTE_MAX_BYTES: usize = 512;
const LONG_ASSISTANT_CHARS: usize = 1_200;
const TOOL_PREVIEW_COMPACT_BYTES: usize = 300;
const TOOL_PREVIEW_NORMAL_BYTES: usize = 1_200;
const TOOL_PREVIEW_VERBOSE_BYTES: usize = 4_000;
/// Default tool-card cap for model-initiated tool calls. Matches codex's
/// `TOOL_CALL_MAX_LINES`. Aggressive on purpose — the structured detail
/// is one keystroke (Ctrl-E / Ctrl-T) away, and a 5-line preview keeps
/// the transcript readable even when the model fires off long commands.
const TOOL_CALL_MAX_LINES: usize = 5;
/// Larger cap for `!`-shell calls the user typed directly (those carry
/// `direct_user_shell: true` in their arguments, set by
/// `local_shell_command_call` in the agent). Mirrors codex's
/// `USER_SHELL_TOOL_CALL_MAX_LINES`.
const USER_SHELL_TOOL_CALL_MAX_LINES: usize = 50;
const PROMPT_MIN_HEIGHT: u16 = 3;
const PROMPT_MAX_HEIGHT: u16 = 8;
const INLINE_VIEWPORT_HEIGHT: u16 = 18;
const SLASH_MENU_MAX_ITEMS: usize = 5;
const DISABLE_MOUSE_MODES: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";
const CLEAR_SCROLLBACK_AND_VISIBLE: &str = "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H";
const RESET_KEYBOARD_ENHANCEMENT_FLAGS: &str = "\x1b[<u";
const TITLE_SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TITLE_SPINNER_INTERVAL_MS: u64 = 100;
const TITLE_NOTIFICATION_GLYPH: &str = "●";

/// Tracks what we want the terminal window/tab title to convey. Most
/// terminal emulators surface their own "activity" indicator that
/// flickers any time stdout sees output, which leaves users staring at
/// a constantly buzzing tab. Taking the title over ourselves lets us
/// give honest signal: spinner while a turn is in flight, a
/// notification glyph once it finishes, and a clear once the user has
/// interacted again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TerminalTitleState {
    Cleared,
    Working,
    Notification,
}

fn terminal_title_for(state: TerminalTitleState, label: &str, elapsed_ms: u64) -> Option<String> {
    match state {
        TerminalTitleState::Cleared => None,
        TerminalTitleState::Working => {
            let idx =
                ((elapsed_ms / TITLE_SPINNER_INTERVAL_MS) as usize) % TITLE_SPINNER_FRAMES.len();
            Some(format!("{} squeezy · {label}", TITLE_SPINNER_FRAMES[idx]))
        }
        TerminalTitleState::Notification => {
            Some(format!("{TITLE_NOTIFICATION_GLYPH} squeezy · {label}"))
        }
    }
}

fn keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        // Modern Windows Terminal and ConPTY honour the ANSI form via
        // `is_ansi_code_supported() == true`. Legacy consoles ignore alternate
        // scroll mode entirely, so a winapi no-op is the right fallback.
        Ok(())
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
        Ok(())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableModifyOtherKeys;

impl Command for DisableModifyOtherKeys {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[>4;0m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupProfile {
    pub onboarding_summary: Option<String>,
    pub languages: String,
    /// When true, the startup resume picker is bypassed and a fresh
    /// agent is created immediately (or the explicit `--resume` id, if
    /// any, is honoured). The CLI flips this on via `--no-resume-picker`
    /// for non-interactive flows (CI, scripts).
    pub skip_resume_picker: bool,
    /// Optional banner that the CLI populates from
    /// `update::banner_for_startup()` when GitHub reports a newer
    /// release than the running binary. Empty / `None` keeps the
    /// transcript quiet. The TUI flushes this through `push_log` at
    /// startup so it lands above the first agent turn.
    pub update_banner: Option<String>,
    /// Pre-resolved session id to resume directly without showing the
    /// picker. Populated by the CLI when `--continue` or
    /// `--session <id>` selected an explicit target; behaves like a
    /// `squeezy sessions resume <id>` invocation but keeps the rest of
    /// the startup banner pipeline intact.
    pub resume_session_id: Option<String>,
}

/// Maximum draw rate enforced by the event loop. 60 FPS keeps animations
/// smooth on every common refresh rate while protecting against unbounded
/// redraw spam when a flurry of agent/job events lands inside one tick.
const MAX_FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Clamps consecutive draws so they cannot fire faster than
/// `MAX_FRAME_INTERVAL`. Many events between draws coalesce into a single
/// redraw.
#[derive(Debug, Default)]
struct FrameRateLimiter {
    last_emitted_at: Option<Instant>,
}

impl FrameRateLimiter {
    /// Returns `true` when a draw is allowed at `now`. Callers must invoke
    /// `mark_emitted` immediately after the draw completes.
    fn allow(&self, now: Instant) -> bool {
        match self.last_emitted_at {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= MAX_FRAME_INTERVAL,
        }
    }

    fn mark_emitted(&mut self, at: Instant) {
        self.last_emitted_at = Some(at);
    }

    /// How long the caller must wait before the next draw is allowed.
    /// `None` when a draw can fire immediately.
    fn time_until_next(&self, now: Instant) -> Option<Duration> {
        let last = self.last_emitted_at?;
        let elapsed = now.saturating_duration_since(last);
        if elapsed >= MAX_FRAME_INTERVAL {
            None
        } else {
            Some(MAX_FRAME_INTERVAL - elapsed)
        }
    }
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
            skip_resume_picker: false,
            update_banner: None,
            resume_session_id: None,
        },
    )
    .await
}

pub async fn run_with_startup_profile(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    startup: StartupProfile,
) -> Result<()> {
    // Resume target carried inside the profile (`--continue` /
    // `--session`) wins over the picker; surface it to `run_inner` as
    // the canonical resume id so the rest of the boot path is identical
    // to `squeezy_tui::resume`.
    let resume = startup.resume_session_id.clone();
    run_inner(config, provider, resume, startup).await
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
    // Apply the persisted theme preference before the first render so the
    // initial paint already reflects the user's choice — without this the
    // first frame uses the auto-detected tone and pops to the override on
    // the next redraw.
    apply_theme_overrides(config.tui.theme);
    let mut terminal = TerminalGuard::enter(config.tui.alternate_screen)?;
    let resume_session_id =
        match maybe_pick_resume_session(&mut terminal, &config, resume_session_id, &startup)? {
            ResumeStartup::Use(id) => Some(id),
            ResumeStartup::Fresh => None,
            ResumeStartup::Quit => return Ok(()),
        };
    let (mut agent, initial_transcript) = if let Some(session_id) = resume_session_id {
        Agent::resume(config.clone(), provider, &session_id)?
    } else {
        (Agent::new(config.clone(), provider), Vec::new())
    };
    // One-shot migration of pre-v3 flat-layout plan files into a
    // legacy subdir; safe to run unconditionally (no-op when nothing
    // needs moving). Per-session pruning runs below once we know the
    // session id.
    let migrated = proposed_plan::migrate_legacy_plans(&config.workspace_root);
    let session_id_for_plans = agent.session_id();
    let plans_session_owned = session_id_for_plans
        .clone()
        .unwrap_or_else(|| proposed_plan::FALLBACK_SESSION_ID.to_string());
    // PR-H (issue 13): plan ids referenced in the last 30 days of git
    // history survive retention pruning even when older than the cap,
    // so design-doc references in commits don't get yanked under the
    // user. Best-effort: no git repo → empty protected set → mtime
    // behaviour.
    let protected_plan_ids = proposed_plan::git_referenced_plan_ids(&config.workspace_root, 30);
    let pruned = proposed_plan::prune_plan_dir(
        &config.workspace_root,
        &plans_session_owned,
        &protected_plan_ids,
    );
    // `StartupProfile` is moved into `TuiApp::new`, so capture the banner
    // (the only field needed below) before that hand-off.
    let update_banner = startup.update_banner.clone();
    let mut app = TuiApp::new(
        agent.provider_name(),
        &config,
        agent.session_mode(),
        startup,
    );
    app.session_id = session_id_for_plans;
    if migrated > 0 {
        app.push_log(format!(
            "migrated {migrated} legacy plan file(s) to {}/{}",
            proposed_plan::PLAN_DIR,
            proposed_plan::LEGACY_PLAN_DIR
        ));
    }
    if pruned > 0 {
        app.push_log(format!(
            "pruned {pruned} stale plan file(s) from {}/{} (kept {} newest)",
            proposed_plan::PLAN_DIR,
            plans_session_owned,
            proposed_plan::PLAN_RETENTION_LIMIT
        ));
    }
    if let Some(banner) = update_banner.filter(|s| !s.trim().is_empty()) {
        app.push_log(banner);
    }
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

    let mut settings_watcher = settings_watcher::SettingsWatcher::new();
    // Poll mtimes roughly once per second; tick_rate defaults to 50ms.
    let settings_poll_every = (1000 / config.tick_rate.as_millis().max(1) as u64).max(1);
    let mut frame_limiter = FrameRateLimiter::default();

    loop {
        // Drain producers first so the next draw reflects everything that
        // has landed since the previous iteration. A flurry of events
        // therefore coalesces into a single frame.
        drain_job_events(&mut app);
        drain_agent_events(&mut app).await;

        app.animation_tick = app.animation_tick.wrapping_add(1);
        if app.app_notifications.tick() {
            app.needs_redraw = true;
        }
        if app.toasts.tick() {
            app.needs_redraw = true;
        }
        if app.animation_tick.is_multiple_of(settings_poll_every)
            && app.config_screen.is_none()
            && settings_watcher.poll()
        {
            apply_external_settings_reload(&mut app, &mut agent);
            app.needs_redraw = true;
        }
        // Only repaint when state actually changed (`needs_redraw`), a
        // resize is pending, or something visible is currently animating.
        // Skipping the draw on idle iterations stops the continuous
        // stdout traffic that triggers terminal emulators' per-tab
        // activity indicators. The frame limiter caps the redraw rate at
        // 60 FPS so bursts of events do not produce a draw storm.
        let wants_draw = app.needs_redraw || app.pending_resize || app.has_active_animation();
        let now = Instant::now();
        if wants_draw && frame_limiter.allow(now) {
            terminal.draw_app(&mut app)?;
            app.needs_redraw = false;
            frame_limiter.mark_emitted(now);
        }

        // Bound the input poll so a deferred draw wakes promptly when the
        // frame budget releases; otherwise honour the configured tick rate.
        let poll_budget = if wants_draw {
            frame_limiter
                .time_until_next(Instant::now())
                .unwrap_or(Duration::ZERO)
                .min(config.tick_rate)
        } else {
            config.tick_rate
        };

        if poll_input(&mut app, &mut agent, poll_budget).await? {
            break;
        }
    }

    agent
        .finish_session(squeezy_store::SessionStatus::Completed)
        .await;
    agent.flush_telemetry().await;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeStartup {
    Use(String),
    Fresh,
    Quit,
}

/// Decide whether to show the startup resume picker, and resolve the
/// session-id (if any) the TUI should resume into.
fn maybe_pick_resume_session(
    terminal: &mut TerminalGuard,
    config: &AppConfig,
    resume_session_id: Option<String>,
    startup: &StartupProfile,
) -> Result<ResumeStartup> {
    if let Some(id) = resume_session_id {
        // Explicit `--resume <id>` bypasses the picker entirely.
        return Ok(ResumeStartup::Use(id));
    }
    if startup.skip_resume_picker {
        return Ok(ResumeStartup::Fresh);
    }
    let candidates = resume_picker::load_candidates(config);
    if candidates.is_empty() {
        return Ok(ResumeStartup::Fresh);
    }
    let choice = resume_picker::run_picker(
        &mut terminal.terminal,
        candidates,
        config.workspace_root.clone(),
    )
    .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
    match choice {
        resume_picker::ResumeChoice::StartFresh => Ok(ResumeStartup::Fresh),
        resume_picker::ResumeChoice::Resume(id) => Ok(ResumeStartup::Use(id)),
        resume_picker::ResumeChoice::CrossProject {
            session_id,
            target_cwd,
        } => {
            // Silently switching cwd would surprise users juggling sibling
            // repos; instead exit with the exact recommended invocation in
            // the exit hint so they can re-run from the target directory.
            terminal.set_exit_hint(Some(cross_project_resume_hint(&session_id, &target_cwd)));
            Ok(ResumeStartup::Quit)
        }
        resume_picker::ResumeChoice::Quit => Ok(ResumeStartup::Quit),
    }
}

/// Format the exit hint printed when the user picks a cross-project
/// resume target. Exposed at module scope so tests can lock the wording.
fn cross_project_resume_hint(session_id: &str, target_cwd: &str) -> String {
    format!("Cross-project resume — run from {target_cwd}:\n  squeezy sessions resume {session_id}")
}

/// Restore the most recently cancelled prompt into the composer. Only
/// fires when no turn is running and the composer is empty, so the user
/// cannot lose in-progress text. Returns `true` when it acted.
fn restore_cancelled_prompt(app: &mut TuiApp) -> bool {
    if app.turn_rx.is_some() {
        return false;
    }
    if !app.input.is_empty() {
        return false;
    }
    let Some(text) = app.cancelled_prompt.take() else {
        return false;
    };
    app.input = text;
    app.input_cursor = app.input.len();
    app.status = "restored last prompt — edit and Enter to retry".to_string();
    true
}

async fn handle_plan_choice_key(app: &mut TuiApp, agent: &mut Agent, key: KeyEvent) -> bool {
    let Some(mut pending) = app.pending_plan_choice.take() else {
        return false;
    };
    let len = PLAN_CHOICES.len();
    let activate_index = match key.code {
        KeyCode::Up => {
            pending.selection_index = pending.selection_index.saturating_sub(1);
            app.pending_plan_choice = Some(pending);
            return true;
        }
        KeyCode::Down => {
            pending.selection_index = (pending.selection_index + 1).min(len - 1);
            app.pending_plan_choice = Some(pending);
            return true;
        }
        KeyCode::Esc => {
            // Dismiss without taking action; equivalent to "Refine" — the
            // plan file stays, the prompt goes away, the user can type
            // anything next.
            app.status = "plan prompt dismissed; keep refining or switch with Shift+Tab".into();
            return true;
        }
        KeyCode::BackTab => {
            // Shift+Tab is the canonical mode toggle; let it fall through
            // so a user who pressed it while the prompt was open still
            // switches modes instead of being stuck.
            app.pending_plan_choice = Some(pending);
            return false;
        }
        KeyCode::Enter => Some(pending.selection_index.min(len - 1)),
        KeyCode::Char(c) => {
            let lower = c.to_ascii_lowercase();
            PLAN_CHOICES
                .iter()
                .position(|option| option.shortcut == lower)
        }
        _ => None,
    };
    let Some(idx) = activate_index else {
        app.pending_plan_choice = Some(pending);
        return true;
    };
    apply_plan_choice(app, agent, &pending, idx).await;
    true
}

async fn apply_plan_choice(
    app: &mut TuiApp,
    agent: &mut Agent,
    pending: &PendingPlanChoice,
    idx: usize,
) {
    let option = &PLAN_CHOICES[idx.min(PLAN_CHOICES.len() - 1)];
    match option.action {
        PlanChoiceAction::Execute => {
            switch_mode(app, agent, Some(SessionMode::Build), "plan_choice_execute");
            if app.mode != SessionMode::Build {
                // Mode switch was refused (active turn, pending approval,
                // …) — leave the queued handoff in place and let the user
                // retry once the blocker clears.
                return;
            }
            start_user_turn(
                app,
                agent,
                "Begin executing the plan above, step by step.".to_string(),
            );
        }
        PlanChoiceAction::ExecuteClean => {
            // Compact the prior conversation so the agent doesn't replay
            // the planning chatter on execution. The plan body still rides
            // in via the handoff prefix queued by the mode switch, so the
            // model retains the full plan even with an emptied transcript.
            match agent.compact_context_manual().await {
                Ok(report) => {
                    app.context_compaction.last = Some(report.record.clone());
                    app.context_compaction.generation = report.record.generation;
                    app.context_compaction.summary = Some(report.summary.clone());
                    app.context_compaction.history.push(report.record.clone());
                    app.context_estimate = report.record.after.clone();
                    app.context_compaction_nudge_shown = false;
                    app.push_log(format!(
                        "compacted prior context before executing plan {}",
                        pending.plan_id
                    ));
                }
                Err(err) => {
                    // "not enough context to compact" is fine — common on a
                    // fresh session and not a blocker for execution.
                    app.push_log(format!(
                        "execute-clean: skipped compaction ({err}); running plan"
                    ));
                }
            }
            switch_mode(
                app,
                agent,
                Some(SessionMode::Build),
                "plan_choice_execute_clean",
            );
            if app.mode != SessionMode::Build {
                return;
            }
            start_user_turn(
                app,
                agent,
                "Begin executing the plan above, step by step.".to_string(),
            );
        }
        PlanChoiceAction::Refine => {
            app.status = "stay in Plan; describe the refinement".into();
        }
        PlanChoiceAction::Discard => match std::fs::remove_file(&pending.plan_path) {
            Ok(()) => {
                app.push_log(format!(
                    "plan {} discarded ({} deleted)",
                    pending.plan_id,
                    compact_path(&pending.plan_path)
                ));
                if app.current_plan_id.as_deref() == Some(pending.plan_id.as_str()) {
                    app.current_plan_id = None;
                }
                if app.pending_plan_handoff.as_deref() == Some(pending.plan_path.as_path()) {
                    app.pending_plan_handoff = None;
                    app.plan_handoff_turns_seen = 0;
                }
            }
            Err(err) => {
                app.push_log(format!(
                    "could not delete plan file {}: {err}",
                    compact_path(&pending.plan_path)
                ));
            }
        },
        PlanChoiceAction::View => {
            app.push_log(format!(
                "plan {} file: {}",
                pending.plan_id,
                compact_path(&pending.plan_path)
            ));
            // Keep the prompt open so the user can pick another action
            // after looking at the file.
            let mut next = pending.clone();
            next.selection_index = 0;
            app.pending_plan_choice = Some(next);
        }
    }
}

/// External `settings.toml` edit observed by `SettingsWatcher` — rebuild the
/// effective `AppConfig` and apply it to the agent. Provider-identical edits
/// (every knob except provider variant) snap immediately via
/// `Agent::replace_config`; a provider switch is armed as a `NextPrompt`
/// swap so an in-flight turn keeps talking to the client it started with.
///
/// Read errors are surfaced as a notification but do not interrupt the
/// session — the most common cause is mid-write (the editor truncating the
/// file before re-writing) and the next poll will see the finished file.
fn apply_external_settings_reload(app: &mut TuiApp, agent: &mut Agent) {
    use crate::notification::Severity;

    let new_cfg = match AppConfig::from_env_and_settings() {
        Ok(cfg) => cfg,
        Err(err) => {
            app.app_notifications
                .push(format!("settings reload failed: {err}"), Severity::Warn);
            return;
        }
    };
    // Mirror an external theme edit into the runtime palette override
    // immediately — the agent's config_snapshot already carries the new
    // value, but the palette layer reads the override directly.
    apply_theme_overrides(new_cfg.tui.theme);
    let old_provider = agent.provider_name();
    let new_provider = squeezy_llm::provider_name(&new_cfg.provider);
    if old_provider == new_provider {
        agent.replace_config(new_cfg);
        app.app_notifications
            .push("settings reloaded from disk".to_string(), Severity::Info);
        return;
    }
    let provider_cfg = new_cfg.provider.clone();
    let handle = std::thread::spawn(move || squeezy_llm::provider_from_config(&provider_cfg));
    match handle.join() {
        Ok(Ok(provider)) => {
            agent.arm_config_swap(PendingConfigSwap {
                config: new_cfg,
                provider: Some(provider),
                display_note: Some(format!(
                    "provider {old_provider} → {new_provider} (applies on next prompt)"
                )),
            });
            app.app_notifications.push(
                format!(
                    "settings reloaded — provider {old_provider} → {new_provider} arms on next prompt"
                ),
                Severity::Info,
            );
        }
        Ok(Err(err)) => {
            agent.replace_config(new_cfg);
            app.app_notifications.push(
                format!(
                    "settings reloaded, but the new {new_provider} client failed to build: {err}"
                ),
                Severity::Error,
            );
        }
        Err(_) => {
            agent.replace_config(new_cfg);
            app.app_notifications.push(
                "settings reloaded, but the provider client thread panicked".to_string(),
                Severity::Error,
            );
        }
    }
}

/// Pick the `slot`-th (1-based) most recent session for this workspace,
/// excluding the active session, and resume into it. Returns `false` when
/// the press should fall through to other key handlers (modals/active
/// turn block the switch); status messages are written for every other
/// outcome including "no session at this slot" so the user sees feedback.
async fn handle_session_quick_switch(app: &mut TuiApp, agent: &mut Agent, slot: usize) -> bool {
    if app.turn_rx.is_some()
        || app.pending_approval.is_some()
        || app.pending_mcp_elicitation.is_some()
        || app.pending_plan_choice.is_some()
        || app.config_screen.is_some()
        || app.status_line_setup.is_some()
        || app.overlay.is_some()
        || app.transcript_overlay.is_some()
    {
        return false;
    }
    let sessions = match agent.list_sessions(&SessionQuery::default()) {
        Ok(list) => list,
        Err(error) => {
            app.status = format!("session quick-switch failed: {error}");
            return true;
        }
    };
    let active = agent.session_id();
    let target = sessions
        .into_iter()
        .filter(|meta| active.as_deref() != Some(meta.session_id.as_str()))
        .nth(slot - 1);
    let Some(target) = target else {
        app.status = format!("no recent session at slot {slot}");
        return true;
    };
    let session_id = target.session_id.clone();
    switch_to_session(app, agent, &session_id).await;
    true
}

/// Replace the current session with `session_id` and rebuild the in-memory
/// transcript from the persisted log. Used by both `/resume` and the
/// Alt+1-9 quick-switch handler so the two paths stay in lockstep.
async fn switch_to_session(app: &mut TuiApp, agent: &mut Agent, session_id: &str) {
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
}

async fn poll_input(app: &mut TuiApp, agent: &mut Agent, tick_rate: Duration) -> Result<bool> {
    if !event::poll(tick_rate).map_err(|err| SqueezyError::Terminal(err.to_string()))? {
        return Ok(false);
    }

    // Any event we read here either drives a state mutation directly or
    // arms `pending_resize` for the next draw. In every non-timeout
    // branch we flip `needs_redraw` so the main loop knows to repaint.
    app.needs_redraw = true;
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
        Event::Resize(_, _) => {
            app.pending_resize = true;
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
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(false);
    }

    // Any keypress while a turn-done notification is up counts as the
    // user acknowledging it — drop the title back to cleared so the
    // emulator's tab/window stops showing the bulb glyph.
    if app.terminal_title_state == TerminalTitleState::Notification {
        app.terminal_title_state = TerminalTitleState::Cleared;
    }

    // Rebindable actions (F11/Ctrl+T/Ctrl+P/Ctrl+Y/Ctrl+R/PageUp/PageDown/
    // Home/End by default) resolve through the keymap before the legacy
    // hardcoded handlers below get a look. The Home/End actions only fire
    // when the composer is empty; otherwise we fall through so the
    // hardcoded line-start/line-end edit semantics keep working.
    if dispatch_keymap_action(app, agent, key) {
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if request_turn_interrupt(app) {
            app.exit_confirm_armed = false;
            return Ok(false);
        }
        if app.exit_confirm_armed {
            return Ok(true);
        }
        app.exit_confirm_armed = true;
        app.status = "press Ctrl+C or Y to exit · any other key to cancel".to_string();
        return Ok(false);
    }

    // While the config screen is open, route all other keys to it.
    if app.config_screen.is_some() {
        let state = app.config_screen.as_mut().expect("checked above");
        let outcome = config_screen::handle_key(state, agent, &mut app.app_notifications, key);
        if matches!(outcome, config_screen::KeyOutcome::Close) {
            app.config_screen = None;
        }
        return Ok(false);
    }

    // `/statusline` overlay swallows all keys until closed.
    if app.status_line_setup.is_some() {
        let outcome = app
            .status_line_setup
            .as_mut()
            .expect("checked above")
            .handle_key(key);
        match outcome {
            status_line_setup::KeyOutcome::Continue => {}
            status_line_setup::KeyOutcome::Cancel => {
                app.status_line_setup = None;
                app.status = "/statusline cancelled".to_string();
            }
            status_line_setup::KeyOutcome::Save { items, use_colors } => {
                app.status_line_setup = None;
                save_status_line(app, agent, items, use_colors);
            }
        }
        return Ok(false);
    }

    // `n` dismisses the current notification, `N` clears all. Only fires
    // when the prompt is empty so we don't eat keystrokes the user is
    // typing into the input area.
    if app.input.is_empty() && !app.app_notifications.is_empty() && key.modifiers.is_empty() {
        if key.code == KeyCode::Char('n') {
            if app.app_notifications.dismiss_current() {
                return Ok(false);
            }
        } else if key.code == KeyCode::Char('N') {
            let removed = app.app_notifications.clear_all();
            if removed > 0 {
                return Ok(false);
            }
        }
    }

    if app.exit_confirm_armed
        && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
        && key.modifiers.is_empty()
    {
        return Ok(true);
    }

    if app.exit_confirm_armed && key.code != KeyCode::Esc {
        app.exit_confirm_armed = false;
        app.status = "exit cancelled".to_string();
        // fall through — the keystroke still performs its normal action
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
        if app.input.is_empty() {
            toggle_selected_transcript_entry(app);
        } else {
            move_input_cursor_line_end(app);
        }
        return Ok(false);
    }

    // The transcript-overlay open/close action is dispatched up top
    // by `dispatch_keymap_action`. While the overlay is open we still
    // need to forward navigation keys to its own handler before the
    // composer takes over.
    if app.transcript_overlay.is_some() && handle_transcript_overlay_key(app, key) {
        return Ok(false);
    }

    if handle_mcp_elicitation_key(app, key) {
        return Ok(false);
    }

    if handle_request_user_input_key(app, key) {
        return Ok(false);
    }

    if handle_plan_choice_key(app, agent, key).await {
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && (key.code == KeyCode::Char('j') || key.code == KeyCode::Enter)
    {
        insert_input_char(app, '\n');
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
        move_input_cursor_line_start(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
        delete_to_line_end(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
        delete_to_line_start(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
        delete_previous_word(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
        delete_at_cursor(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('h') {
        delete_before_cursor(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
        move_input_cursor_left(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
        move_input_cursor_right(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('b') {
        move_input_cursor_word_left(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('f') {
        move_input_cursor_word_right(app);
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('d') {
        delete_next_word(app);
        return Ok(false);
    }

    if key.modifiers == KeyModifiers::ALT
        && let KeyCode::Char(ch) = key.code
        && let Some(slot) = ch.to_digit(10)
        && (1..=9).contains(&slot)
        && handle_session_quick_switch(app, agent, slot as usize).await
    {
        return Ok(false);
    }

    if key.code == KeyCode::BackTab {
        switch_mode(app, agent, None, "tui_shift_tab");
        return Ok(false);
    }

    // `/plan` and `/build` flow through `handle_slash_command` like every
    // other slash command — see the dispatcher arms there. The old
    // Enter-time pre-intercept used to live here, but it silently dropped
    // any input with a trailing space (e.g. `/plan `).

    if key.code == KeyCode::Esc && request_turn_interrupt(app) {
        return Ok(false);
    }

    if handle_approval_key(app, key) {
        return Ok(false);
    }

    if app.overlay.is_some() && handle_overlay_key(app, key) {
        return Ok(false);
    }

    if app.mention_popup.is_some() && handle_mention_popup_key(app, key) {
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc => {
            // ESC never exits; the pre-check above already handled in-flight
            // turn/approval interrupts, so a bare ESC at this point is a no-op.
            Ok(false)
        }
        // PageUp/PageDown and (empty-composer) Home/End are routed via
        // `dispatch_keymap_action` so users can rebind them via
        // `[tui.keymap]`. When dispatch returned `false` for Home/End
        // (composer non-empty) the line-cursor cases below execute.
        KeyCode::Home => {
            move_input_cursor_line_start(app);
            Ok(false)
        }
        KeyCode::End => {
            move_input_cursor_line_end(app);
            Ok(false)
        }
        KeyCode::Left => {
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            {
                move_input_cursor_word_left(app);
            } else {
                move_input_cursor_left(app);
            }
            Ok(false)
        }
        KeyCode::Right => {
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            {
                move_input_cursor_word_right(app);
            } else {
                move_input_cursor_right(app);
            }
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
            // Stash the typed prompt before clearing so that a Ctrl-C/Esc
            // during the turn can restore it via Ctrl-R. Completion clears
            // this field; only Cancelled/Failed leave it set.
            app.cancelled_prompt = Some(input.clone());
            clear_input(app);
            push_input_history(app, input.clone());
            start_user_turn(app, agent, input);
            Ok(false)
        }
        KeyCode::Backspace => {
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::META)
            {
                delete_to_line_start(app);
            } else if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            {
                delete_previous_word(app);
            } else {
                delete_before_cursor(app);
            }
            Ok(false)
        }
        KeyCode::Delete => {
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::META)
            {
                delete_to_line_end(app);
            } else if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            {
                delete_next_word(app);
            } else {
                delete_at_cursor(app);
            }
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
    let normalized = normalize_pasted_text(&text);
    // The config screen owns its own set of focusable text inputs (secret
    // entry, search, picker filter, field editor). Route paste there
    // instead of attaching it as transcript context when the screen is up.
    if let Some(state) = app.config_screen.as_mut() {
        config_screen::handle_paste(state, &normalized);
        return Ok(());
    }
    if app
        .pending_mcp_elicitation
        .as_ref()
        .is_some_and(|pending| pending.request.kind == McpElicitationKind::Form)
    {
        insert_input_text(app, &normalized);
        return Ok(());
    }
    if app.turn_rx.is_some()
        || app.pending_approval.is_some()
        || app.pending_mcp_elicitation.is_some()
    {
        app.status = "paste unavailable during active turn".to_string();
        return Ok(());
    }
    if is_inline_paste(&normalized) {
        insert_input_text(app, &normalized);
        return Ok(());
    }
    match agent.attach_pasted_context(normalized).await {
        Ok(update) => {
            app.attachments = agent.context_attachments_snapshot().await;
            app.status = attachment_update_status("paste", &update);
        }
        Err(error) => app.status = format!("paste attach failed: {error}"),
    }
    Ok(())
}

fn normalize_pasted_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn is_inline_paste(text: &str) -> bool {
    text.len() <= INLINE_PASTE_MAX_BYTES && !text.contains('\n')
}

/// Resolve `key` against the user-configurable keymap and execute the
/// matched action. Returns `true` if the action consumed the keystroke
/// so the caller skips the legacy hardcoded handlers. Bindings that
/// have no override behave exactly like the pre-keymap build.
///
/// Composer basics (Enter / Esc / Backspace / character input) are
/// intentionally outside this dispatch — they stay hardcoded below
/// since rebinding them breaks every workflow.
fn dispatch_keymap_action(app: &mut TuiApp, agent: &mut Agent, key: KeyEvent) -> bool {
    let Some(action) = app.keymap.lookup(key.code, key.modifiers) else {
        return false;
    };
    match action {
        keymap::Action::ToggleConfigScreen => {
            toggle_config_screen(app, agent, None);
            true
        }
        keymap::Action::ToggleTranscriptOverlay => {
            // Skip while the config screen is in the foreground; that
            // overlay owns its own key routing.
            if app.config_screen.is_some() || app.status_line_setup.is_some() {
                return false;
            }
            app.transcript_overlay = if app.transcript_overlay.is_some() {
                None
            } else {
                Some(TranscriptOverlayState::default())
            };
            app.status = if app.transcript_overlay.is_some() {
                "transcript overlay (Esc to close)".to_string()
            } else {
                "transcript overlay closed".to_string()
            };
            true
        }
        keymap::Action::ToggleTaskPanel => {
            if app.config_screen.is_some() || app.status_line_setup.is_some() {
                return false;
            }
            if app.task_state.is_some() {
                app.task_panel_collapsed = !app.task_panel_collapsed;
                app.status = if app.task_panel_collapsed {
                    "task panel collapsed".to_string()
                } else {
                    "task panel expanded".to_string()
                };
            }
            true
        }
        keymap::Action::CopyLastAssistant => {
            if app.config_screen.is_some() || app.status_line_setup.is_some() {
                return false;
            }
            copy_to_clipboard(app, ClipboardTarget::LastAssistant);
            true
        }
        keymap::Action::RestoreCancelledPrompt => {
            if app.config_screen.is_some() || app.status_line_setup.is_some() {
                return false;
            }
            restore_cancelled_prompt(app)
        }
        keymap::Action::ScrollTranscriptPageUp => {
            if app.config_screen.is_some() || app.status_line_setup.is_some() {
                return false;
            }
            scroll_transcript_up(app, 8);
            true
        }
        keymap::Action::ScrollTranscriptPageDown => {
            if app.config_screen.is_some() || app.status_line_setup.is_some() {
                return false;
            }
            scroll_transcript_down(app, 8);
            true
        }
        keymap::Action::TranscriptHome => {
            // The legacy binding only jumps to top when the composer
            // is empty; otherwise it acts as a line-start cursor move.
            // Defer to the composer handler in that case.
            if app.config_screen.is_some() || app.status_line_setup.is_some() {
                return false;
            }
            if app.input.is_empty() {
                app.transcript_scroll_from_bottom = u16::MAX;
                true
            } else {
                false
            }
        }
        keymap::Action::TranscriptEnd => {
            if app.config_screen.is_some() || app.status_line_setup.is_some() {
                return false;
            }
            if app.input.is_empty() {
                app.transcript_scroll_from_bottom = 0;
                true
            } else {
                false
            }
        }
    }
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
    if let Some(pending) = app.pending_mcp_elicitation.take() {
        let _ = pending.response_tx.send(McpElicitationResponse::cancel());
        interrupted = true;
    }
    if interrupted {
        app.status = "interrupting".to_string();
        app.turn_visual = TurnVisualState::Failed;
        app.clear_active_tools();
    }
    interrupted
}

/// Kick off a new model turn for the given input. Shared between the
/// Enter-key submission path and slash commands like `/plan <prompt>` that
/// want to launch a turn after their side-effect.
fn toggle_config_screen(
    app: &mut TuiApp,
    agent: &Agent,
    focus: Option<squeezy_core::config_schema::SectionId>,
) {
    if app.config_screen.is_some() {
        app.config_screen = None;
        app.status = "config closed".to_string();
        return;
    }
    let state = config_screen::ConfigScreenState::new(agent.config_snapshot(), focus);
    app.config_screen = Some(state);
    app.status = "config".to_string();
}

fn toggle_status_line_setup(app: &mut TuiApp) {
    if app.status_line_setup.is_some() {
        app.status_line_setup = None;
        app.status = "/statusline closed".to_string();
        return;
    }
    app.status_line_setup = Some(status_line_setup::StatusLineSetupState::new(
        app.status_line_items.as_deref(),
        app.status_line_use_colors,
    ));
    app.status = "/statusline".to_string();
}

/// Convert a [`TuiTheme`] preference into the runtime palette tone override.
/// `System` clears the override so terminal detection (`COLORFGBG`) wins;
/// `Catppuccin` pins Dark and `HighContrast` pins Light because each named
/// theme expects a specific background tone for its accent palette to read
/// correctly.
fn theme_to_tone_override(theme: TuiTheme) -> Option<render::palette::PaletteTone> {
    match theme {
        TuiTheme::System => None,
        TuiTheme::Dark | TuiTheme::Catppuccin => Some(render::palette::PaletteTone::Dark),
        TuiTheme::Light | TuiTheme::HighContrast => Some(render::palette::PaletteTone::Light),
    }
}

/// Map a [`TuiTheme`] to the accent family it owns. The amber/gold default
/// is shared by `System`, `Dark`, and `Light`; only the named themes flip
/// the accent.
fn theme_to_accent_variant(theme: TuiTheme) -> render::palette::AccentVariant {
    match theme {
        TuiTheme::Catppuccin => render::palette::AccentVariant::Catppuccin,
        TuiTheme::HighContrast => render::palette::AccentVariant::HighContrast,
        _ => render::palette::AccentVariant::Default,
    }
}

/// Apply both the tone and accent overrides for `theme` in one shot so
/// callers don't accidentally update one and forget the other.
fn apply_theme_overrides(theme: TuiTheme) {
    render::palette::set_palette_tone_override(theme_to_tone_override(theme));
    render::palette::set_accent_variant(theme_to_accent_variant(theme));
}

/// Apply a `/theme` switch: flip the runtime palette override, mirror the
/// new value into the agent's in-memory config, and persist to the user-
/// scope settings file so the choice survives a restart. Persistence failures
/// surface in the status line but the live switch still takes effect — the
/// user can re-run later to retry the save.
fn apply_theme_change(app: &mut TuiApp, agent: &mut Agent, theme: TuiTheme) {
    use squeezy_core::settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits};

    apply_theme_overrides(theme);

    let mut next = agent.config_snapshot();
    next.tui.theme = theme;
    agent.replace_config(next);

    let target_path = squeezy_core::default_settings_path();
    let scope_target = SettingsScope::user(&target_path);
    let edits = [SettingsEdit {
        path: &["tui", "theme"],
        op: EditOp::SetString(theme.as_str().to_string()),
    }];
    match apply_edits(&scope_target, &edits) {
        Ok(_) => {
            app.app_notifications.push(
                format!("theme → {}", theme.as_str()),
                NotifySeverity::Success,
            );
            app.status = format!("theme saved to {}", target_path.display());
        }
        Err(err) => {
            app.app_notifications.push(
                format!("theme switched but save failed: {err}"),
                NotifySeverity::Warn,
            );
            app.status = format!("theme switched (not persisted): {err}");
        }
    }
    app.needs_redraw = true;
}

/// Persist the picker's selection to `[tui].status_line` /
/// `[tui].status_line_use_colors` in the user-scope settings file and
/// apply it in-memory immediately.
fn save_status_line(
    app: &mut TuiApp,
    agent: &mut Agent,
    items: Vec<status::StatusLineItem>,
    use_colors: bool,
) {
    use squeezy_core::settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits};

    let target_path = squeezy_core::default_settings_path();
    let scope_target = SettingsScope::user(&target_path);
    let slug_list: Vec<String> = items.iter().map(|i| i.slug().to_string()).collect();
    let edits = [
        SettingsEdit {
            path: &["tui", "status_line"],
            op: EditOp::SetArrayOfStrings(slug_list.clone()),
        },
        SettingsEdit {
            path: &["tui", "status_line_use_colors"],
            op: EditOp::SetBool(use_colors),
        },
    ];
    match apply_edits(&scope_target, &edits) {
        Ok(_) => {
            // Apply immediately in-memory.
            let mut cfg = agent.config_snapshot();
            cfg.tui.status_line = if slug_list.is_empty() {
                None
            } else {
                Some(slug_list.clone())
            };
            cfg.tui.status_line_use_colors = use_colors;
            agent.replace_config(cfg);
            app.status_line_items = Some(items);
            app.status_line_use_colors = use_colors;
            app.status = format!("status line saved to {}", target_path.display());
            let summary = if slug_list.is_empty() {
                format!(
                    "status line cleared (colors {}); written to {}",
                    if use_colors { "on" } else { "off" },
                    target_path.display(),
                )
            } else {
                format!(
                    "status line saved: {} (colors {}); written to {}",
                    slug_list.join(", "),
                    if use_colors { "on" } else { "off" },
                    target_path.display(),
                )
            };
            app.push_transcript_item(TranscriptItem::system(summary));
        }
        Err(err) => {
            let msg = format!("/statusline save failed: {err}");
            app.status = msg.clone();
            app.push_transcript_item(TranscriptItem::system(msg));
        }
    }
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
    if let Some(spec) = SLASH_COMMANDS.iter().find(|spec| spec.name == command)
        && !spec.available_during_task
        && turn_in_progress(app)
    {
        app.status = format!("{command} unavailable during turn");
        return true;
    }
    match command {
        "/config" => {
            let section = if rest.is_empty() {
                None
            } else {
                squeezy_core::config_schema::section_from_slug(rest)
            };
            toggle_config_screen(app, agent, section);
            return true;
        }
        "/statusline" => {
            toggle_status_line_setup(app);
            return true;
        }
        "/plan" => {
            switch_mode(app, agent, Some(SessionMode::Plan), "tui_command");
            if !rest.is_empty() {
                let prompt = rest.to_string();
                app.cancelled_prompt = Some(prompt.clone());
                clear_input(app);
                push_input_history(app, prompt.clone());
                start_user_turn(app, agent, prompt);
            }
            return true;
        }
        "/build" => {
            switch_mode(app, agent, Some(SessionMode::Build), "tui_command");
            if !rest.is_empty() {
                let prompt = rest.to_string();
                app.cancelled_prompt = Some(prompt.clone());
                clear_input(app);
                push_input_history(app, prompt.clone());
                start_user_turn(app, agent, prompt);
            }
            return true;
        }
        "/plans" => {
            handle_plans_command(app, rest);
            return true;
        }
        "/cost" => {
            let snapshot = agent.session_accounting_snapshot().await;
            app.status = "cost snapshot".to_string();
            app.push_transcript_item(TranscriptItem::system(commands::format_cost_command(
                &snapshot,
            )));
            return true;
        }
        "/context" => {
            let snapshot = agent.session_accounting_snapshot().await;
            app.status = "context snapshot".to_string();
            app.push_transcript_item(TranscriptItem::system(commands::format_context_command(
                &snapshot,
            )));
            return true;
        }
        "/reviewer" => {
            let entries = agent.reviewer_audit_snapshot();
            if entries.is_empty() {
                app.status = "no AI reviewer decisions recorded yet".to_string();
            } else {
                app.status = format!("{} AI reviewer decision(s)", entries.len());
            }
            app.push_transcript_item(TranscriptItem::system(commands::format_reviewer_command(
                &entries,
                std::time::SystemTime::now(),
            )));
            return true;
        }
        "/help" => {
            handle_help_command(app, agent, rest);
            return true;
        }
        "/model" => {
            toggle_config_screen(
                app,
                agent,
                Some(squeezy_core::config_schema::SectionId::Models),
            );
            return true;
        }
        "/permissions" => {
            toggle_config_screen(
                app,
                agent,
                Some(squeezy_core::config_schema::SectionId::Permissions),
            );
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
            let subcommand = parts.next().map(str::trim).unwrap_or("");
            if subcommand.eq_ignore_ascii_case("undo") {
                match agent.compact_context_undo().await {
                    Ok(Some(record)) => {
                        app.context_compaction = agent.context_compaction_snapshot().await;
                        app.context_estimate = agent.context_estimate_snapshot().await;
                        app.status = format!(
                            "undid compaction gen={} ({} item(s) restored)",
                            record.generation, record.dropped_items,
                        );
                        app.push_log(format!(
                            "context compaction undone gen={} items={}",
                            record.generation, record.dropped_items,
                        ));
                    }
                    Ok(None) => {
                        app.status = "no compaction checkpoint to undo".to_string();
                    }
                    Err(error) => app.status = format!("compact undo failed: {error}"),
                }
                return true;
            }
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
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "/compact discarded {dropped} item(s); context {before}→{after} tokens. \
                         Run `/compact undo` to restore.",
                        dropped = report.record.dropped_items,
                        before = report.record.before.estimated_tokens,
                        after = report.record.after.estimated_tokens,
                    )));
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
        "/diff" => {
            handle_slash_diff(app);
            return true;
        }
        "/effort" => {
            handle_slash_effort(app, agent, parts.next());
            return true;
        }
        "/verbosity" => {
            // Back-compat: `/verbosity concise|normal|verbose` still works as
            // a quick set. Without an arg, opens the config screen on the
            // Verbosity section.
            if let Some(value) = parts.next()
                && let Some(verbosity) = parse_response_verbosity(value)
            {
                app.response_verbosity = verbosity;
                let mut next = agent.config_snapshot();
                next.tui.response_verbosity = verbosity;
                agent.replace_config(next);
                app.app_notifications.push(
                    format!("response verbosity → {}", verbosity.as_str()),
                    NotifySeverity::Success,
                );
                return true;
            }
            toggle_config_screen(
                app,
                agent,
                Some(squeezy_core::config_schema::SectionId::Verbosity),
            );
            return true;
        }
        "/tool-verbosity" => {
            if let Some(value) = parts.next()
                && let Some(verbosity) = parse_tool_output_verbosity(value)
            {
                app.tool_output_verbosity = verbosity;
                let mut next = agent.config_snapshot();
                next.tui.tool_output_verbosity = verbosity;
                agent.replace_config(next);
                app.app_notifications.push(
                    format!("tool output verbosity → {}", verbosity.as_str()),
                    NotifySeverity::Success,
                );
                return true;
            }
            toggle_config_screen(
                app,
                agent,
                Some(squeezy_core::config_schema::SectionId::Verbosity),
            );
            return true;
        }
        "/theme" => {
            let Some(raw) = parts.next() else {
                app.status =
                    "usage: /theme [system|dark|light|catppuccin|high-contrast]".to_string();
                return true;
            };
            let Some(theme) = TuiTheme::parse(raw) else {
                app.status = format!(
                    "unknown theme {raw:?}; expected system, dark, light, catppuccin, or high-contrast",
                );
                return true;
            };
            apply_theme_change(app, agent, theme);
            return true;
        }
        "/keymap" => {
            let body = keymap::format_keymap_command(&app.keymap);
            let overrides = keymap::Action::ALL
                .iter()
                .copied()
                .filter(|a| app.keymap.binding(*a) != a.default_binding())
                .count();
            app.status = if overrides == 0 {
                "keymap (defaults)".to_string()
            } else {
                format!("keymap ({overrides} override(s))")
            };
            app.push_transcript_item(TranscriptItem::system(body));
            return true;
        }
        "/tasks" | "/jobs" => {
            sync_jobs_from_agent(app, agent);
            let body = format_tasks_list(app, agent);
            app.status = format!("{} tasks", app.jobs.len());
            app.push_transcript_item(TranscriptItem::system(body));
            return true;
        }
        "/task" | "/job" => {
            let Some(raw_id) = parts.next() else {
                app.status = format!("usage: {command} <id>");
                return true;
            };
            let Some(id) = parse_job_id(raw_id) else {
                app.status = "task id must be a number".to_string();
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
                    app.status = format!("task {} {}", job.id, job.status.as_str());
                    app.push_transcript_item(TranscriptItem::system(format_job_detail(&job)));
                }
                None => app.status = format!("task {id} not found"),
            }
            return true;
        }
        "/task-cancel" | "/job-cancel" => {
            let Some(raw_id) = parts.next() else {
                app.status = format!("usage: {command} <id>");
                return true;
            };
            let Some(id) = parse_job_id(raw_id) else {
                app.status = "task id must be a number".to_string();
                return true;
            };
            if agent.cancel_job(id) {
                app.status = format!("cancelling task {id}");
                sync_jobs_from_agent(app, agent);
            } else {
                app.status = format!("task {id} not active");
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
        "/fork" => {
            match agent.fork_current().await {
                Ok(new_id) => {
                    app.status = format!("forked session → {new_id}");
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "/fork started session {new_id}; the original session is saved and \
                         remains resumable via /resume."
                    )));
                }
                Err(error) => app.status = format!("fork failed: {error}"),
            }
            return true;
        }
        "/resume" => {
            let Some(session_id) = parts.next() else {
                app.status = "usage: /resume <session_id>".to_string();
                return true;
            };
            switch_to_session(app, agent, session_id).await;
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

/// Dispatch for `/plans [list|show|delete|set-active|open] [<id>]`.
/// Bare `/plans` (or `/plans list`) renders a table; the other
/// subcommands take an id (or any unique prefix of one). Plan ids are
/// resolved within the active session's plan dir; sibling sessions
/// are intentionally invisible.
fn handle_plans_command(app: &mut TuiApp, rest: &str) {
    let mut parts = rest.split_whitespace();
    let sub = parts.next().unwrap_or("list");
    let arg = parts.next();
    let sid = app.plan_session_id().to_string();
    match sub {
        "list" | "ls" => render_plans_list(app, &sid),
        "show" | "view" | "cat" => {
            if let Some(plan_id) = plans_resolve(app, &sid, arg) {
                plans_show(app, &sid, &plan_id);
            }
        }
        "delete" | "rm" => {
            if let Some(plan_id) = plans_resolve(app, &sid, arg) {
                plans_delete(app, &sid, &plan_id, parts.next());
            }
        }
        "set-active" | "activate" | "use" => {
            if let Some(plan_id) = plans_resolve(app, &sid, arg) {
                plans_set_active(app, &sid, &plan_id);
            }
        }
        "open" | "edit" => {
            if let Some(plan_id) = plans_resolve(app, &sid, arg) {
                plans_open(app, &sid, &plan_id);
            }
        }
        "help" | "?" | "--help" | "-h" => {
            app.status = "plans help".to_string();
            app.push_transcript_item(TranscriptItem::system(plans_usage()));
        }
        other => {
            app.status = format!("unknown /plans subcommand `{other}`");
            app.push_transcript_item(TranscriptItem::system(plans_usage()));
        }
    }
}

fn plans_usage() -> String {
    [
        "usage:",
        "  /plans              — list saved plans in this session",
        "  /plans list",
        "  /plans show <id>    — render a plan body",
        "  /plans delete <id>  — remove a plan (add --yes to skip confirm)",
        "  /plans set-active <id>",
        "  /plans open <id>    — print path for opening in your editor",
    ]
    .join("\n")
}

/// Resolve the user's `<id>` argument to an exact plan id within the
/// session. Sets status/transcript on error and returns `None` so the
/// caller can early-out. On success returns the canonical plan id.
fn plans_resolve(app: &mut TuiApp, sid: &str, raw: Option<&str>) -> Option<String> {
    let Some(needle) = raw else {
        app.status = "usage: /plans <subcommand> <id-or-prefix>".to_string();
        return None;
    };
    match proposed_plan::resolve_plan_prefix(&app.workspace_root, sid, needle) {
        Ok(plan_id) => Some(plan_id),
        Err(proposed_plan::PlanLookupError::NotFound) => {
            app.status = format!("no plan matches `{needle}` in this session");
            None
        }
        Err(proposed_plan::PlanLookupError::Ambiguous(matches)) => {
            app.status = format!("`{needle}` is ambiguous ({} matches)", matches.len());
            let body = format!(
                "Multiple plans match `{needle}`. Disambiguate by re-running with a longer prefix:\n{}",
                matches
                    .iter()
                    .map(|id| format!("  {id}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            app.push_transcript_item(TranscriptItem::system(body));
            None
        }
    }
}

fn render_plans_list(app: &mut TuiApp, sid: &str) {
    let entries = proposed_plan::list_plans(&app.workspace_root, sid);
    if entries.is_empty() {
        app.status = "no plans persisted in this session".to_string();
        return;
    }
    app.status = format!("{} plan(s) in this session", entries.len());
    let now = std::time::SystemTime::now();
    let mut lines = Vec::with_capacity(entries.len() + 1);
    lines.push("  ACTIVE  ID                    AGE       OBJECTIVE".to_string());
    for entry in &entries {
        let age = format_age_short(now, entry.modified);
        let marker = if entry.is_active {
            "  *     "
        } else {
            "        "
        };
        let objective = truncate_for_display(&entry.objective, 60);
        lines.push(format!(
            "{marker}{id:<22} {age:<9} {objective}",
            id = entry.plan_id,
            age = age,
            objective = objective,
        ));
    }
    app.push_transcript_item(TranscriptItem::system(lines.join("\n")));
}

/// Render a short relative-age string (`12s`, `4m`, `3h`, `5d`) for
/// `/plans list`. Anything older than 99 days collapses to `>99d`.
fn format_age_short(now: std::time::SystemTime, when: std::time::SystemTime) -> String {
    let elapsed = now.duration_since(when).unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 86_400 * 100 {
        format!("{}d", secs / 86_400)
    } else {
        ">99d".to_string()
    }
}

/// Trim `s` to at most `max_chars` codepoints, appending `…` when
/// truncation actually happens. Empty input is rendered as `-`.
fn truncate_for_display(s: &str, max_chars: usize) -> String {
    if s.is_empty() {
        return "-".to_string();
    }
    let mut iter = s.chars();
    let head: String = (&mut iter).take(max_chars).collect();
    if iter.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn plans_show(app: &mut TuiApp, sid: &str, plan_id: &str) {
    let path = proposed_plan::plan_file_for(&app.workspace_root, sid, plan_id);
    match proposed_plan::read_plan_body(&path) {
        Ok(body) => {
            app.status = format!("plan {plan_id}");
            let header = format!("# Plan {plan_id}\n{}", compact_path(&path));
            app.push_transcript_item(TranscriptItem::system(format!(
                "{header}\n\n{}",
                body.trim_end()
            )));
        }
        Err(err) => app.status = format!("plans show failed: {err}"),
    }
}

fn plans_delete(app: &mut TuiApp, sid: &str, plan_id: &str, flag: Option<&str>) {
    // Confirmation gate: destructive ops require an explicit `--yes`
    // (or `-y`). The user can re-run the same command after seeing the
    // prompt without losing context.
    let confirmed = matches!(flag, Some("--yes") | Some("-y") | Some("yes"));
    if !confirmed {
        app.status = format!("re-run with --yes to delete {plan_id}");
        app.push_transcript_item(TranscriptItem::system(format!(
            "/plans delete is destructive. Re-run as `/plans delete {plan_id} --yes` to confirm."
        )));
        return;
    }
    match proposed_plan::delete_plan(&app.workspace_root, sid, plan_id) {
        Ok(path) => {
            // Clear the in-memory active plan id if this was it; the
            // pointer file has already been cleaned by `delete_plan`.
            if app.current_plan_id.as_deref() == Some(plan_id) {
                app.current_plan_id = None;
                app.pending_plan_handoff = None;
                app.plan_handoff_turns_seen = 0;
            }
            app.status = format!("deleted plan {plan_id}");
            app.push_log(format!("plan {plan_id} deleted ({})", compact_path(&path)));
        }
        Err(err) => app.status = format!("plans delete failed: {err}"),
    }
}

fn plans_set_active(app: &mut TuiApp, sid: &str, plan_id: &str) {
    match proposed_plan::set_active_plan(&app.workspace_root, sid, plan_id) {
        Ok(()) => {
            app.current_plan_id = Some(plan_id.to_string());
            app.status = format!("active plan → {plan_id}");
            app.push_log(format!("set active plan: {plan_id}"));
        }
        Err(err) => app.status = format!("plans set-active failed: {err}"),
    }
}

fn plans_open(app: &mut TuiApp, sid: &str, plan_id: &str) {
    // The TUI owns the terminal in alternate-screen mode, so launching
    // a foreground editor (vi/nano) inline would scramble the display.
    // PR-E scope keeps this surface simple: print the path and the
    // recommended editor command so the user can run it from another
    // shell. The terminal-suspend integration is a follow-up.
    let path = proposed_plan::plan_file_for(&app.workspace_root, sid, plan_id);
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });
    app.status = format!("plan {plan_id} path printed");
    app.push_transcript_item(TranscriptItem::system(format!(
        "plan {plan_id}\nfile: {}\nopen with: {editor} {}",
        compact_path(&path),
        path.display()
    )));
}

fn handle_help_command(app: &mut TuiApp, agent: &mut Agent, rest: &str) {
    let prompt = if rest.trim().is_empty() {
        "/help".to_string()
    } else {
        format!("/help {}", rest.trim())
    };
    start_user_turn(app, agent, prompt);
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
            let (session_id, excluded_sections) =
                match commands::parse_report_preview_args(agent, rest) {
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

pub(crate) fn compaction_status_line(record: &ContextCompactionRecord) -> String {
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

/// Render the `/tasks` view: every background job followed by a `reviewer`
/// line so the AI reviewer surfaces alongside ordinary jobs even though it
/// does not yet have its own task type. Each line is "{id} {status} {kind}
/// {title}"; reviewer uses `reviewer` for both id and kind.
fn format_tasks_list(app: &TuiApp, agent: &Agent) -> String {
    let mut lines: Vec<String> = app
        .jobs
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
        .collect();
    lines.push(format_reviewer_task_line(agent));
    lines.join("\n")
}

fn format_reviewer_task_line(agent: &Agent) -> String {
    let entries = agent.reviewer_audit_snapshot();
    if entries.is_empty() {
        "reviewer idle ai-reviewer no decisions yet".to_string()
    } else {
        format!(
            "reviewer ready ai-reviewer {} recent decision(s)",
            entries.len()
        )
    }
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
            if !app.pending_assistant.trim_is_empty() {
                return Some(app.pending_assistant.text());
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
        lines.push(format!("assistant: {}", app.pending_assistant.text()));
    }
    lines.join("\n")
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

/// `/effort` sets `model.reasoning_effort` in the live in-memory config so the
/// next turn picks it up via `request_reasoning_effort`. `auto` (or `clear`,
/// `unset`, `none`) drops the override and falls back to the model default. The
/// command is session-scoped — to persist across runs, edit `model.reasoning_effort`
/// via `/config`. `SQUEEZY_REASONING_EFFORT` env var still wins on next load, so
/// surface that fact when set.
fn handle_slash_effort(app: &mut TuiApp, agent: &mut Agent, value: Option<&str>) {
    let Some(raw) = value else {
        let current = agent.config_snapshot().reasoning_effort.map_or_else(
            || "unset (model default)".to_string(),
            |e| e.as_str().to_string(),
        );
        app.status = format!("reasoning effort: {current}");
        app.push_transcript_item(TranscriptItem::system(format!(
            "reasoning effort = {current}\nusage: /effort [low|medium|high|xhigh|auto]"
        )));
        return;
    };
    let next_effort = match raw.trim().to_ascii_lowercase().as_str() {
        "auto" | "clear" | "unset" | "none" => None,
        other => match squeezy_core::ReasoningEffort::parse(other) {
            Some(effort) => Some(effort),
            None => {
                app.status =
                    format!("unknown effort {raw:?}; expected low, medium, high, xhigh, or auto");
                return;
            }
        },
    };
    let mut next = agent.config_snapshot();
    next.reasoning_effort = next_effort;
    agent.replace_config(next);
    let label = next_effort.map_or_else(
        || "auto (model default)".to_string(),
        |e| e.as_str().to_string(),
    );
    app.app_notifications.push(
        format!("reasoning effort → {label}"),
        NotifySeverity::Success,
    );
    app.status = format!("reasoning effort → {label}");
    if std::env::var("SQUEEZY_REASONING_EFFORT").is_ok() {
        app.app_notifications.push(
            "SQUEEZY_REASONING_EFFORT overrides this on next load".to_string(),
            NotifySeverity::Warn,
        );
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

/// Handle a keystroke while the full-screen transcript overlay is open.
/// Returns `true` when the key was consumed so the caller does not also
/// dispatch it to the normal input/turn paths.
fn handle_transcript_overlay_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    if app.transcript_overlay.is_none() {
        return false;
    }
    const PAGE: u16 = 10;
    match key.code {
        KeyCode::Esc => {
            app.transcript_overlay = None;
            app.status = "transcript overlay closed".to_string();
            true
        }
        KeyCode::PageUp => {
            if let Some(state) = app.transcript_overlay.as_mut() {
                state.scroll = state.scroll.saturating_sub(PAGE);
            }
            true
        }
        KeyCode::PageDown => {
            if let Some(state) = app.transcript_overlay.as_mut() {
                state.scroll = state.scroll.saturating_add(PAGE);
            }
            true
        }
        KeyCode::Up => {
            if let Some(state) = app.transcript_overlay.as_mut() {
                state.scroll = state.scroll.saturating_sub(1);
            }
            true
        }
        KeyCode::Down => {
            if let Some(state) = app.transcript_overlay.as_mut() {
                state.scroll = state.scroll.saturating_add(1);
            }
            true
        }
        KeyCode::Home => {
            if let Some(state) = app.transcript_overlay.as_mut() {
                state.scroll = 0;
            }
            true
        }
        KeyCode::End => {
            if let Some(state) = app.transcript_overlay.as_mut() {
                state.scroll = u16::MAX;
            }
            true
        }
        _ => true, // swallow everything else so the overlay stays modal
    }
}

fn toggle_selected_transcript_entry(app: &mut TuiApp) {
    let selected = app.selected_entry.filter(|index| {
        app.transcript
            .get(*index)
            .is_some_and(|entry| entry.is_toggleable())
    });
    let Some(index) = selected
        .or_else(|| latest_collapsed_transcript_entry(app))
        .or_else(|| latest_toggleable_transcript_entry(app))
    else {
        app.status = "nothing expandable yet".to_string();
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

fn latest_collapsed_transcript_entry(app: &TuiApp) -> Option<usize> {
    // Prefer the most recent collapsed reasoning entry when one exists.
    // Without this preference Ctrl-E falls through to the most recent
    // collapsed *peer* — which after a turn lands is the assistant
    // message (also collapsed on `transcript_default = compact`). The
    // assistant body is usually a short single line, so toggling it has
    // no visible effect and the reasoning chevron the user actually
    // wanted to expand stays collapsed. Reasoning blocks are by far the
    // most common Ctrl-E target, so prefer them when the user hasn't
    // explicitly navigated to a specific entry.
    let latest_reasoning = app
        .transcript
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| {
            matches!(entry.kind, TranscriptEntryKind::Reasoning(_))
                && entry.collapsed
                && entry.is_toggleable()
        })
        .map(|(index, _)| index);
    if latest_reasoning.is_some() {
        return latest_reasoning;
    }
    app.transcript
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| entry.collapsed && entry.is_toggleable())
        .map(|(index, _)| index)
}

/// Kick off a user-driven turn. Drains any pending config swap, consumes a
/// queued plan handoff (prepending the plan body to `input`), and hands
/// the resulting prompt to the agent. Used by the Enter key handler and
/// by the post-plan Execute action so both paths share the same plan
/// prefix and turn-state bookkeeping.
fn start_user_turn(app: &mut TuiApp, agent: &mut Agent, input: String) {
    if let Some(swap) = agent.drain_pending_swap() {
        let note = swap
            .display_note
            .clone()
            .unwrap_or_else(|| "config applied".to_string());
        app.app_notifications
            .push(format!("✓ applied: {note}"), NotifySeverity::Success);
    }
    let cancel = CancellationToken::new();
    app.task_state = None;
    app.task_panel_collapsed = false;
    app.note_turn_started();
    let prefixed_input = match take_pending_plan_prefix(app) {
        Some(prefix) => format!("{prefix}{input}"),
        None => input,
    };
    app.turn_rx = Some(agent.start_turn_with_response_verbosity(
        prefixed_input,
        cancel.clone(),
        app.response_verbosity,
    ));
    app.cancel = Some(cancel);
    app.status = "starting turn".to_string();
    app.turn_visual = TurnVisualState::Running;
}

/// If a Plan→Build handoff is queued, return the prefix to prepend to the
/// next user input. Turn 0 (the first call after Plan→Build) returns the
/// full plan body so the model receives it verbatim; turns 1+ return a
/// short `[plan still in effect — <path>]` marker so the plan continues
/// to anchor the conversation without re-paying body tokens each turn
/// (issue 16). The handoff is cleared automatically when the file is
/// missing; routine clears (Build→Plan, discard, successful apply_patch)
/// happen elsewhere.
fn take_pending_plan_prefix(app: &mut TuiApp) -> Option<String> {
    let plan_path = app.pending_plan_handoff.clone()?;
    if app.plan_handoff_turns_seen > 0 {
        // Subsequent turns: lightweight marker.
        let marker = proposed_plan::BUILD_PLAN_STILL_IN_EFFECT_FORMAT
            .replace("{path}", &plan_path.display().to_string());
        return Some(marker);
    }
    // Strip the YAML front-matter (PR-D) before handing the body off
    // to the model: the model should see the plan content, not the
    // metadata block that the TUI uses for /plans rendering.
    match proposed_plan::read_plan_body(&plan_path) {
        Ok(body) => {
            let trimmed = body.trim_end();
            app.plan_handoff_turns_seen = app.plan_handoff_turns_seen.saturating_add(1);
            // PR-G item 6: when this Plan→Build crossing is a resume
            // from a Shift+Tab pause, prefix the body with a short
            // marker so the model knows it's mid-execution and learns
            // whether the plan was refined during the pause window.
            let resume_marker = app.plan_resume_marker.take().unwrap_or_default();
            Some(format!(
                "{resume_marker}[plan from previous session — {path}]\n{trimmed}\n[end plan]\n\n",
                path = plan_path.display(),
            ))
        }
        Err(err) => {
            app.push_log(format!(
                "could not read plan file {} for Build handoff: {err}",
                compact_path(&plan_path)
            ));
            app.pending_plan_handoff = None;
            app.plan_handoff_turns_seen = 0;
            None
        }
    }
}

fn switch_mode(
    app: &mut TuiApp,
    agent: &Agent,
    requested: Option<SessionMode>,
    source: &'static str,
) {
    let target = requested.unwrap_or(match app.mode {
        SessionMode::Plan => SessionMode::Build,
        SessionMode::Build => SessionMode::Plan,
    });
    // PR-G item 6: Build→Plan with an in-flight turn AND an active
    // plan is a *pause*, not a refusal. Cancel the turn, capture the
    // plan id so the next Plan→Build crossing can emit a resume
    // marker, and fall through to the normal switch path.
    let is_build_to_plan_pause = app.mode == SessionMode::Build
        && target == SessionMode::Plan
        && app.turn_rx.is_some()
        && app.current_plan_id.is_some();
    if is_build_to_plan_pause {
        request_turn_interrupt(app);
        app.plan_pause = app
            .current_plan_id
            .clone()
            .map(|plan_id| PlanPauseState { plan_id });
        app.push_log("plan execution paused (Shift+Tab)".to_string());
    }

    if !is_build_to_plan_pause
        && (app.turn_rx.is_some()
            || app.pending_approval.is_some()
            || app.pending_mcp_elicitation.is_some()
            || app.pending_request_user_input.is_some())
    {
        app.status = "mode switch unavailable during active turn".to_string();
        return;
    }

    if target == app.mode {
        app.status = format!("already in {} mode", app.mode.as_str());
        return;
    }
    let previous = app.mode;
    if agent.set_session_mode(target, source) {
        app.mode = target;
        app.status = format!("mode switched to {}", app.mode.as_str());
        // A mode switch supersedes any post-plan choice prompt — the
        // user's decision has been made by toggling the mode itself.
        app.pending_plan_choice = None;
        match (previous, target) {
            (SessionMode::Plan, SessionMode::Build) => {
                if let Some(plan_id) = app.current_plan_id.as_deref() {
                    let sid = app.plan_session_id().to_string();
                    let plan_path =
                        proposed_plan::plan_file_for(&app.workspace_root, &sid, plan_id);
                    if plan_path.exists() {
                        app.pending_plan_handoff = Some(plan_path.clone());
                        // Fresh handoff: next Build turn gets the full plan
                        // body; turns 2+ get the lighter marker.
                        app.plan_handoff_turns_seen = 0;
                        // Resume marker (PR-G item 6): if the previous
                        // Build→Plan was a pause, tell the model which
                        // plan id it resumes from and whether the body
                        // changed during the pause window.
                        if let Some(pause) = app.plan_pause.take() {
                            let refined = pause.plan_id != plan_id;
                            let note = if refined {
                                format!(
                                    "[resuming from plan {plan_id} — plan refined since previous attempt ({prev})]\n",
                                    prev = pause.plan_id,
                                )
                            } else {
                                format!("[resuming from plan {plan_id} — plan unchanged]\n")
                            };
                            app.plan_resume_marker = Some(note);
                        }
                        app.push_log(format!(
                            "plan attached for next Build turn: {}",
                            compact_path(&plan_path)
                        ));
                    }
                }
            }
            (SessionMode::Build, SessionMode::Plan) => {
                // A handoff queued by an earlier Plan→Build switch is no
                // longer relevant once the user goes back to Plan; drop it
                // so the next Build entry recomputes from the current
                // plan file instead of attaching a stale path.
                app.pending_plan_handoff = None;
                app.plan_handoff_turns_seen = 0;
            }
            _ => {}
        }
    } else {
        // Agent saw no change (lock-free path is infallible, so this only
        // fires when the agent observed the same mode we requested). Resync
        // the visible status with the underlying truth so the user sees the
        // authoritative state.
        app.mode = agent.session_mode();
        app.status = format!("already in {} mode", app.mode.as_str());
    }
}

fn handle_mcp_elicitation_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    let Some(pending) = app.pending_mcp_elicitation.take() else {
        return false;
    };
    let is_form = pending.request.kind == McpElicitationKind::Form;

    if is_form
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && (key.code == KeyCode::Char('j') || key.code == KeyCode::Enter)
    {
        insert_input_char(app, '\n');
        keep_mcp_elicitation_pending(app, pending);
        return true;
    }

    match key.code {
        KeyCode::Up => {
            app.mcp_elicitation_selection_index =
                app.mcp_elicitation_selection_index.saturating_sub(1);
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        KeyCode::Down => {
            app.mcp_elicitation_selection_index =
                (app.mcp_elicitation_selection_index + 1).min(mcp_elicitation_options().len() - 1);
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        KeyCode::Enter => {
            let option = mcp_elicitation_options()
                .get(app.mcp_elicitation_selection_index)
                .copied()
                .unwrap_or(MCP_ELICITATION_ACCEPT);
            send_mcp_elicitation_response(app, pending, option.choice)
        }
        KeyCode::Esc => send_mcp_elicitation_cancel(app, pending),
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            send_mcp_elicitation_response(app, pending, McpElicitationChoice::Accept)
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('d') | KeyCode::Char('D') => {
            send_mcp_elicitation_response(app, pending, McpElicitationChoice::Decline)
        }
        KeyCode::Backspace if is_form => {
            delete_before_cursor(app);
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        KeyCode::Delete if is_form => {
            delete_at_cursor(app);
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        KeyCode::Left if is_form => {
            move_input_cursor_left(app);
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        KeyCode::Right if is_form => {
            move_input_cursor_right(app);
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        KeyCode::Home if is_form => {
            app.input_cursor = 0;
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        KeyCode::End if is_form => {
            app.input_cursor = app.input.len();
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        KeyCode::Char(ch)
            if is_form && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
        {
            insert_input_char(app, ch);
            keep_mcp_elicitation_pending(app, pending);
            true
        }
        _ => {
            keep_mcp_elicitation_pending(app, pending);
            true
        }
    }
}

fn keep_mcp_elicitation_pending(app: &mut TuiApp, pending: PendingMcpElicitation) {
    app.status = format_mcp_elicitation_status_line(&pending.request);
    app.pending_mcp_elicitation = Some(pending);
}

fn send_mcp_elicitation_response(
    app: &mut TuiApp,
    pending: PendingMcpElicitation,
    choice: McpElicitationChoice,
) -> bool {
    match choice {
        McpElicitationChoice::Accept => {
            let content = if pending.request.kind == McpElicitationKind::Form {
                match parse_mcp_elicitation_form_content(&app.input) {
                    Ok(content) => Some(content),
                    Err(error) => {
                        app.status = error;
                        app.pending_mcp_elicitation = Some(pending);
                        return true;
                    }
                }
            } else {
                None
            };
            let server = pending.request.server.clone();
            let _ = pending
                .response_tx
                .send(McpElicitationResponse::accept(content));
            clear_input(app);
            app.status = format!("accepted mcp request from {server}");
            true
        }
        McpElicitationChoice::Decline => {
            let server = pending.request.server.clone();
            let _ = pending.response_tx.send(McpElicitationResponse::decline());
            app.status = format!("declined mcp request from {server}");
            true
        }
    }
}

fn send_mcp_elicitation_cancel(app: &mut TuiApp, pending: PendingMcpElicitation) -> bool {
    let server = pending.request.server.clone();
    let _ = pending.response_tx.send(McpElicitationResponse::cancel());
    app.status = format!("cancelled mcp request from {server}");
    true
}

fn parse_mcp_elicitation_form_content(
    input: &str,
) -> std::result::Result<serde_json::Value, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value @ serde_json::Value::Object(_)) => Ok(value),
        Ok(_) => Err("MCP form response must be a JSON object".to_string()),
        Err(error) => Err(format!("MCP form response JSON is invalid: {error}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpElicitationChoice {
    Accept,
    Decline,
}

#[derive(Debug, Clone, Copy)]
struct McpElicitationOption {
    choice: McpElicitationChoice,
    label: &'static str,
    hint: &'static str,
}

const MCP_ELICITATION_ACCEPT: McpElicitationOption = McpElicitationOption {
    choice: McpElicitationChoice::Accept,
    label: "Accept",
    hint: "send response",
};

const MCP_ELICITATION_DECLINE: McpElicitationOption = McpElicitationOption {
    choice: McpElicitationChoice::Decline,
    label: "Decline",
    hint: "deny request",
};

fn mcp_elicitation_options() -> &'static [McpElicitationOption] {
    &[MCP_ELICITATION_ACCEPT, MCP_ELICITATION_DECLINE]
}

fn handle_approval_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    let Some(pending) = app.pending_approval.take() else {
        return false;
    };
    let options = approval_options_for(&pending.request);

    match key.code {
        KeyCode::Up => {
            app.approval_selection_index = app.approval_selection_index.saturating_sub(1);
            app.status = format_approval_status_line(&pending.request);
            app.pending_approval = Some(pending);
            true
        }
        KeyCode::Down => {
            app.approval_selection_index =
                (app.approval_selection_index + 1).min(options.len() - 1);
            app.status = format_approval_status_line(&pending.request);
            app.pending_approval = Some(pending);
            true
        }
        KeyCode::Enter => {
            let option = options
                .get(app.approval_selection_index)
                .cloned()
                .unwrap_or_else(approval_once);
            send_approval_decision(app, pending, option)
        }
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            send_approval_decision(app, pending, approval_once())
        }
        KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Char('p') | KeyCode::Char('P') => {
            // Capability-scoped "always allow" — picks the per-capability
            // project option built by `approval_options_for`.
            let project = options
                .iter()
                .find(|opt| opt.choice == ApprovalChoice::ApproveProject)
                .cloned()
                .unwrap_or_else(approval_once);
            send_approval_decision(app, pending, project)
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('d') | KeyCode::Char('D') => {
            send_approval_decision(app, pending, approval_deny())
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
        ApprovalChoice::ApproveSession => format!("saved session approval for {tool_name}"),
        ApprovalChoice::ApproveProject => format!("saved repo approval for {tool_name}"),
        ApprovalChoice::Deny => format!("denied {tool_name}"),
        ApprovalChoice::DenySession => format!("saved session deny for {tool_name}"),
    };
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalChoice {
    Approve,
    ApproveSession,
    ApproveProject,
    Deny,
    DenySession,
}

#[derive(Debug, Clone)]
struct ApprovalOption {
    choice: ApprovalChoice,
    label: std::borrow::Cow<'static, str>,
    hint: std::borrow::Cow<'static, str>,
    decision: ToolApprovalDecision,
}

impl ApprovalOption {
    const fn new_static(
        choice: ApprovalChoice,
        label: &'static str,
        hint: &'static str,
        decision: ToolApprovalDecision,
    ) -> Self {
        Self {
            choice,
            label: std::borrow::Cow::Borrowed(label),
            hint: std::borrow::Cow::Borrowed(hint),
            decision,
        }
    }
}

fn approval_once() -> ApprovalOption {
    ApprovalOption::new_static(
        ApprovalChoice::Approve,
        "Approve",
        "run this once",
        ToolApprovalDecision::AllowOnce,
    )
}

fn approval_deny() -> ApprovalOption {
    ApprovalOption::new_static(
        ApprovalChoice::Deny,
        "Deny",
        "skip this run",
        ToolApprovalDecision::DenyOnce,
    )
}

fn approval_deny_session() -> ApprovalOption {
    ApprovalOption::new_static(
        ApprovalChoice::DenySession,
        "Deny for this session",
        "save an in-memory deny rule",
        ToolApprovalDecision::DenySession,
    )
}

/// Build the per-capability allow/deny menu for a pending approval. Codex's
/// `ExecApprovalRequestEvent::default_available_decisions` shapes the option
/// list to the request (network host vs exec amendment vs plain accept); the
/// audit (`ux.md#E-UX-06`) calls out that squeezy's fixed five-option set hides
/// what scope each "Approve" actually saves. Labels here name the binary, host,
/// server, or path that the resulting rule will cover so the user can codify
/// *why* in one keystroke.
fn approval_options_for(request: &ToolApprovalRequest) -> Vec<ApprovalOption> {
    let (session_label, session_hint, project_label, project_hint) =
        capability_scope_labels(request);
    let session = ApprovalOption {
        choice: ApprovalChoice::ApproveSession,
        label: session_label,
        hint: session_hint,
        decision: ToolApprovalDecision::AllowSession,
    };
    let project = ApprovalOption {
        choice: ApprovalChoice::ApproveProject,
        label: project_label,
        hint: project_hint,
        decision: ToolApprovalDecision::AllowRuleProject,
    };
    vec![
        approval_once(),
        session,
        project,
        approval_deny(),
        approval_deny_session(),
    ]
}

/// Returns `(session_label, session_hint, project_label, project_hint)` for
/// the allow options. Each label names the capability-specific target (binary,
/// host, MCP server/tool, write root) so the prompt makes the persisted rule
/// shape visible without forcing the user to read the rule-preview line.
fn capability_scope_labels(
    request: &ToolApprovalRequest,
) -> (
    std::borrow::Cow<'static, str>,
    std::borrow::Cow<'static, str>,
    std::borrow::Cow<'static, str>,
    std::borrow::Cow<'static, str>,
) {
    use std::borrow::Cow;
    let permission = &request.permission;
    let scope_name: Option<String> = match permission.capability {
        PermissionCapability::Shell => permission
            .metadata
            .get("binary")
            .cloned()
            .or_else(|| permission.metadata.get("shell_prefix").cloned()),
        PermissionCapability::Network => permission.metadata.get("host").cloned().or_else(|| {
            permission
                .target
                .strip_prefix("domain:")
                .map(str::to_string)
        }),
        PermissionCapability::Mcp => {
            let server = permission.metadata.get("server").cloned();
            let tool = permission.metadata.get("tool").cloned();
            match (server, tool) {
                (Some(server), Some(tool)) => Some(format!("{server}/{tool}")),
                (Some(server), None) => Some(server),
                _ => None,
            }
        }
        PermissionCapability::Edit => permission
            .metadata
            .get("write_root")
            .cloned()
            .or_else(|| permission.metadata.get("path").cloned()),
        PermissionCapability::Read | PermissionCapability::Search => permission
            .metadata
            .get("path")
            .cloned()
            .or_else(|| permission.metadata.get("query").cloned()),
        PermissionCapability::Git
        | PermissionCapability::Compiler
        | PermissionCapability::Destructive => None,
    };
    let Some(scope) = scope_name.and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() || trimmed == "*" {
            None
        } else {
            Some(trimmed)
        }
    }) else {
        return (
            Cow::Borrowed("Approve for this session"),
            Cow::Borrowed("save an in-memory rule"),
            Cow::Borrowed("Always approve this command in this repo"),
            Cow::Borrowed("save a project rule"),
        );
    };
    let scope_display = compact_text(&scope, 60);
    let (kind, hint_kind) = match permission.capability {
        PermissionCapability::Shell => ("command", "command"),
        PermissionCapability::Network => ("host", "host"),
        PermissionCapability::Mcp => ("MCP tool", "MCP tool"),
        PermissionCapability::Edit => ("edits to", "edit target"),
        PermissionCapability::Read | PermissionCapability::Search => ("reads of", "read target"),
        PermissionCapability::Git
        | PermissionCapability::Compiler
        | PermissionCapability::Destructive => unreachable!(),
    };
    (
        Cow::Owned(format!("Allow {kind} {scope_display} (session)")),
        Cow::Owned(format!("save an in-memory {hint_kind} rule")),
        Cow::Owned(format!("Always allow {kind} {scope_display}")),
        Cow::Owned(format!("save a project {hint_kind} rule")),
    )
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

pub(crate) fn format_mcp_elicitation_status_line(request: &McpElicitationRequest) -> String {
    format!(
        "mcp request: {} {} {}",
        request.server,
        mcp_elicitation_kind_label(&request.kind),
        compact_text(&request.message, 120),
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

fn format_mcp_elicitation_menu_lines(
    request: &McpElicitationRequest,
    selected: usize,
    input: &str,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "MCP request",
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " · {} · {}",
                request.server,
                mcp_elicitation_kind_label(&request.kind)
            ),
            Style::default().fg(QUIET),
        ),
    ])];
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            compact_text(&request.message, 180),
            Style::default().fg(Color::White),
        ),
    ]));
    match request.kind {
        McpElicitationKind::Form => {
            if let Some(schema) = request.schema.as_ref() {
                let schema = serde_json::to_string(schema)
                    .unwrap_or_else(|_| "<schema unavailable>".to_string());
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("schema {}", compact_text(&schema, 160)),
                        Style::default().fg(QUIET),
                    ),
                ]));
            }
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("response {}", mcp_elicitation_response_preview(input)),
                    Style::default().fg(QUIET),
                ),
            ]));
        }
        McpElicitationKind::Url => {
            if let Some(url) = request.url.as_ref() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(compact_text(url, 180), Style::default().fg(QUIET)),
                ]));
            }
        }
    }
    for (index, option) in mcp_elicitation_options().iter().enumerate() {
        let is_selected = index == selected.min(mcp_elicitation_options().len() - 1);
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

fn mcp_elicitation_kind_label(kind: &McpElicitationKind) -> &'static str {
    match kind {
        McpElicitationKind::Form => "form",
        McpElicitationKind::Url => "url",
    }
}

fn mcp_elicitation_response_preview(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        "{}".to_string()
    } else {
        compact_text(trimmed, 160)
    }
}

fn format_plan_choice_menu_lines(pending: &PendingPlanChoice) -> Vec<Line<'static>> {
    let selected = pending.selection_index.min(PLAN_CHOICES.len() - 1);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "Plan ready",
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", pending.plan_id),
            Style::default().fg(QUIET),
        ),
    ])];
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            compact_path(&pending.plan_path),
            Style::default().fg(Color::White),
        ),
    ]));
    for (idx, option) in PLAN_CHOICES.iter().enumerate() {
        let is_selected = idx == selected;
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
            Span::styled(
                format!("[{}] {}", option.shortcut, option.label),
                label_style,
            ),
            Span::styled(format!(" · {}", option.hint), Style::default().fg(QUIET)),
        ]));
    }
    lines
}

fn format_request_user_input_menu_lines(
    request: &RequestUserInputRequest,
    selected: usize,
    input: &str,
) -> Vec<Line<'static>> {
    let mut lines = vec![{
        let mut spans = vec![Span::styled(
            "Plan-mode question",
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        )];
        if request.allow_freeform {
            spans.push(Span::styled(
                " · freeform allowed",
                Style::default().fg(QUIET),
            ));
        }
        Line::from(spans)
    }];
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            compact_text(&request.question, 240),
            Style::default().fg(Color::White),
        ),
    ]));
    for (index, choice) in request.choices.iter().enumerate() {
        let is_selected = index == selected.min(request.choices.len().saturating_sub(1));
        let marker = if is_selected { "› " } else { "  " };
        let label_style = if is_selected {
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let mut spans = vec![
            Span::styled(
                marker,
                Style::default().fg(if is_selected { GOLD } else { QUIET }),
            ),
            Span::styled(compact_text(&choice.label, 180), label_style),
        ];
        if choice.value != choice.label {
            spans.push(Span::styled(
                format!(" · {}", compact_text(&choice.value, 120)),
                Style::default().fg(QUIET),
            ));
        }
        lines.push(Line::from(spans));
    }
    if request.allow_freeform {
        let preview = if input.trim().is_empty() {
            "(type in the prompt below)".to_string()
        } else {
            compact_text(input.trim(), 180)
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("freeform: {preview}"), Style::default().fg(QUIET)),
        ]));
    }
    lines
}

fn format_approval_menu_lines(
    request: &ToolApprovalRequest,
    selected: usize,
) -> Vec<Line<'static>> {
    let mut lines = approval::render_preview(request);
    let options = approval_options_for(request);
    let max_index = options.len().saturating_sub(1);
    for (index, option) in options.iter().enumerate() {
        let is_selected = index == selected.min(max_index);
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
            Span::styled(option.label.to_string(), label_style),
            Span::styled(format!(" · {}", option.hint), Style::default().fg(QUIET)),
        ]));
    }
    lines
}

fn render(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    if app.transcript_overlay.is_some() {
        render_transcript_overlay(frame, area, app);
        return;
    }
    if let Some(state) = &app.status_line_setup {
        let notif_h = app.app_notifications.height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if notif_h > 0 {
                vec![Constraint::Min(0), Constraint::Length(notif_h)]
            } else {
                vec![Constraint::Min(0)]
            })
            .split(area);
        status_line_setup::render(frame, chunks[0], state, app);
        if notif_h > 0 {
            render_notification_pane(frame, chunks[1], app);
        }
        return;
    }
    if let Some(state) = &app.config_screen {
        let notif_h = app.app_notifications.height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if notif_h > 0 {
                vec![Constraint::Min(0), Constraint::Length(notif_h)]
            } else {
                vec![Constraint::Min(0)]
            })
            .split(area);
        config_screen::render(frame, chunks[0], state);
        if notif_h > 0 {
            render_notification_pane(frame, chunks[1], app);
        }
        return;
    }
    let include_startup_card = area.height >= 16;
    let input_height = input_panel_height(app, area.width);
    let approval_height = approval_menu_height(app);
    let plan_indicator_height = plan_mode_indicator_height(app);
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
        .saturating_add(plan_indicator_height)
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
    if plan_indicator_height > 0 {
        constraints.push(Constraint::Length(plan_indicator_height));
    }
    constraints.push(Constraint::Length(input_height));
    if approval_height > 0 {
        constraints.push(Constraint::Length(approval_height));
    }
    let notification_height = app.app_notifications.height();
    if notification_height > 0 {
        constraints.push(Constraint::Length(notification_height));
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
    if plan_indicator_height > 0 {
        render_plan_mode_indicator(frame, chunks[index], app);
        index += 1;
    }
    render_input(frame, chunks[index], app);
    index += 1;
    if approval_height > 0 {
        render_approval(frame, chunks[index], app);
        index += 1;
    }
    if notification_height > 0 {
        render_notification_pane(frame, chunks[index], app);
        index += 1;
    }
    render_status(frame, chunks[index], app);
    index += 1;
    // Flexible filler keeps the prompt/status block attached to the transcript
    // instead of pinning it to the terminal bottom.
    let _ = chunks[index];
    render_toast_overlay(frame, area, app);
}

fn render_notification_pane(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    use ratatui::{
        style::{Modifier, Style},
        text::{Line, Span},
        widgets::Paragraph,
    };
    let Some(current) = app.app_notifications.current() else {
        return;
    };
    let remaining_secs = current.remaining().as_secs();
    let mut spans = vec![
        Span::styled(
            current.severity.glyph(),
            Style::default()
                .fg(current.severity.color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(current.message.as_str(), Style::default().fg(Color::White)),
    ];
    if let Some(hint) = current.action_hint {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(hint, Style::default().fg(QUIET)));
    }
    if app.app_notifications.len() > 1 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("({}+)", app.app_notifications.len() - 1),
            Style::default().fg(QUIET),
        ));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("· {remaining_secs}s"),
        Style::default().fg(QUIET),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Overlay the corner-toast stack on the top-right of `area`. Each visible
/// toast renders as a single-line tinted glyph + message. Toasts draw
/// after every other surface so they sit on top of the transcript and
/// modal flows; layout-wise they reserve zero rows in the constraint
/// solver and simply clip whatever pixels they overlap.
fn render_toast_overlay(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    use ratatui::{
        style::{Modifier, Style},
        text::{Line, Span},
        widgets::{Clear, Paragraph},
    };
    let visible = app.toasts.visible();
    if visible.is_empty() || area.width < 8 || area.height == 0 {
        return;
    }
    let max_width = area.width.saturating_sub(2).clamp(8, 40);
    for (row_offset, toast) in visible.iter().enumerate() {
        if row_offset as u16 >= area.height {
            break;
        }
        let label = format!("{} {}", toast.variant.glyph(), toast.message);
        let visual: String = label.chars().take(max_width as usize).collect();
        let line_width = visual.chars().count() as u16;
        let x = area.right().saturating_sub(line_width + 1).max(area.left());
        let y = area.top() + row_offset as u16;
        let rect = Rect {
            x,
            y,
            width: line_width.min(area.right().saturating_sub(x)),
            height: 1,
        };
        if rect.width == 0 {
            continue;
        }
        let style = Style::default()
            .fg(toast.variant.color())
            .add_modifier(Modifier::BOLD);
        let span = Span::styled(visual, style);
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new(Line::from(span)), rect);
    }
}

fn render_inline(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    if app.transcript_overlay.is_some() {
        render_transcript_overlay(frame, area, app);
        return;
    }
    if let Some(state) = &app.status_line_setup {
        let notif_h = app.app_notifications.height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if notif_h > 0 {
                vec![Constraint::Min(0), Constraint::Length(notif_h)]
            } else {
                vec![Constraint::Min(0)]
            })
            .split(area);
        status_line_setup::render(frame, chunks[0], state, app);
        if notif_h > 0 {
            render_notification_pane(frame, chunks[1], app);
        }
        return;
    }
    if let Some(state) = &app.config_screen {
        let notif_h = app.app_notifications.height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if notif_h > 0 {
                vec![Constraint::Min(0), Constraint::Length(notif_h)]
            } else {
                vec![Constraint::Min(0)]
            })
            .split(area);
        config_screen::render(frame, chunks[0], state);
        if notif_h > 0 {
            render_notification_pane(frame, chunks[1], app);
        }
        return;
    }
    let input_height = input_panel_height(app, area.width);
    let approval_height = approval_menu_height(app);
    let plan_indicator_height = plan_mode_indicator_height(app);
    let task_height = should_show_task_panel(app).then_some(task_panel_height(app));
    let status_height = 2;
    let live_lines = pending_assistant_lines(app);
    let live_visual_height = visual_line_count(&live_lines, area.width);
    let live_gap = if live_visual_height > 0 { 1 } else { 0 };
    let required_height = task_height
        .unwrap_or(0)
        .saturating_add(input_height)
        .saturating_add(approval_height)
        .saturating_add(plan_indicator_height)
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
    if plan_indicator_height > 0 {
        constraints.push(Constraint::Length(plan_indicator_height));
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
    if plan_indicator_height > 0 {
        render_plan_mode_indicator(frame, chunks[index], app);
        index += 1;
    }
    render_input(frame, chunks[index], app);
    index += 1;
    if approval_height > 0 {
        render_approval(frame, chunks[index], app);
        index += 1;
    }
    render_status(frame, chunks[index], app);
    render_toast_overlay(frame, area, app);
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

fn task_panel_height(app: &TuiApp) -> u16 {
    if turn_in_progress(app) && working_detail_line(app).is_some() {
        2
    } else {
        1
    }
}

fn render_task_state(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut lines = if turn_in_progress(app) {
        let mut rows = vec![working_line(app)];
        if let Some(detail) = working_detail_line(app) {
            rows.push(detail);
        }
        rows
    } else if let Some(duration) = app.last_turn_duration {
        vec![worked_divider_line(duration, area.width)]
    } else if let Some(snapshot) = app.task_state.as_ref() {
        vec![compact_task_state_line(snapshot)]
    } else {
        vec![working_line(app)]
    };
    if lines.len() < area.height as usize {
        // Pad so the bottom row doesn't shift when the detail goes away.
        while lines.len() < area.height as usize {
            lines.push(Line::from(""));
        }
    }
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(QUIET))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// Detail row rendered below the spinner when there is something
/// actionable to show. Returns `None` when the spinner alone suffices —
/// keeps the working cell single-row in the common case.
fn working_detail_line(app: &TuiApp) -> Option<Line<'static>> {
    // Highest priority: MCP startup in progress.
    if let Some(snapshot) = app.mcp_status.as_ref() {
        let mut starting = 0usize;
        let mut total = 0usize;
        let mut ready = 0usize;
        for status in snapshot.per_server.values() {
            total += 1;
            match status {
                McpServerStatus::Starting => starting += 1,
                McpServerStatus::Ready { .. } => ready += 1,
                _ => {}
            }
        }
        if starting > 0 && total > 0 {
            let text = format!("    ↳ mcp: starting {ready}/{total} servers");
            return Some(Line::from(Span::styled(text, Style::default().fg(QUIET))));
        }
    }

    // Otherwise: count of additional queued tools beyond the visible one.
    let visible_tools = app
        .active_tool_calls
        .values()
        .filter(|call| !is_control_tool_name(&call.name))
        .count();
    if visible_tools > 1 {
        let extra = visible_tools - 1;
        let text = format!(
            "    ↳ +{extra} more tool call{} queued",
            if extra == 1 { "" } else { "s" }
        );
        return Some(Line::from(Span::styled(text, Style::default().fg(QUIET))));
    }
    None
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
    let activity_color = if interrupting {
        ERROR_RED
    } else {
        render::palette::accent_primary()
    };
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
        if let Some(elapsed_ms) = app.active_tool_elapsed_ms {
            spans.extend(active_tool_elapsed_spans(elapsed_ms));
        }
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
                .fg(blend_color(
                    render::palette::accent_primary(),
                    render::palette::accent_working_highlight(),
                    intensity,
                ))
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
    if let Some(pending) = app.pending_approval.as_ref() {
        format_approval_menu_lines(&pending.request, app.approval_selection_index).len() as u16
    } else if let Some(pending) = app.pending_mcp_elicitation.as_ref() {
        match pending.request.kind {
            McpElicitationKind::Form => {
                if pending.request.schema.is_some() {
                    6
                } else {
                    5
                }
            }
            McpElicitationKind::Url => {
                if pending.request.url.is_some() {
                    5
                } else {
                    4
                }
            }
        }
    } else if let Some(pending) = app.pending_request_user_input.as_ref() {
        format_request_user_input_menu_lines(&pending.request, pending.selection_index, &app.input)
            .len() as u16
    } else if let Some(pending) = app.pending_plan_choice.as_ref() {
        format_plan_choice_menu_lines(pending).len() as u16
    } else {
        0
    }
}

fn render_approval(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph = Paragraph::new(approval_lines(app))
        .style(Style::default().fg(QUIET))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn approval_lines(app: &TuiApp) -> Vec<Line<'static>> {
    if let Some(pending) = app.pending_approval.as_ref() {
        format_approval_menu_lines(&pending.request, app.approval_selection_index)
    } else if let Some(pending) = app.pending_mcp_elicitation.as_ref() {
        format_mcp_elicitation_menu_lines(
            &pending.request,
            app.mcp_elicitation_selection_index,
            &app.input,
        )
    } else if let Some(pending) = app.pending_request_user_input.as_ref() {
        format_request_user_input_menu_lines(&pending.request, pending.selection_index, &app.input)
    } else if let Some(pending) = app.pending_plan_choice.as_ref() {
        format_plan_choice_menu_lines(pending)
    } else {
        Vec::new()
    }
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

/// State for the full-screen transcript overlay (Ctrl+T). All transcript
/// entries are rendered in their fully-expanded form regardless of each
/// entry's collapsed flag; the user scrolls with PgUp/PgDn/arrows.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TranscriptOverlayState {
    pub(crate) scroll: u16,
}

/// Render the full-screen transcript overlay. Replaces the normal
/// transcript + prompt layout while `app.transcript_overlay` is `Some`.
fn render_transcript_overlay(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let state = match app.transcript_overlay {
        Some(state) => state,
        None => return,
    };
    let title = " Transcript — Ctrl-T or Esc to close · PgUp/PgDn scroll ";
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(GOLD))
        .title(Span::styled(
            title,
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = transcript_lines_for_overlay(app, Some(inner.width));
    let paragraph = Paragraph::new(lines)
        .scroll((state.scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

/// Build the per-entry line list for the overlay: every entry is forced
/// to its expanded form. Skips the pending-assistant tail since the
/// overlay is a snapshot of committed transcript content.
fn transcript_lines_for_overlay(app: &TuiApp, width: Option<u16>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, entry) in app.transcript.iter().enumerate() {
        lines.extend(format_transcript_entry_expanded(
            entry,
            app.selected_entry == Some(index),
            app.tool_output_verbosity,
            message_outcome(&app.transcript, index),
            width,
            app.show_reasoning_usage,
        ));
    }
    lines
}

fn format_transcript_entry_expanded(
    entry: &TranscriptEntry,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
    outcome: MessageOutcome,
    width: Option<u16>,
    show_reasoning: bool,
) -> Vec<Line<'static>> {
    match &entry.kind {
        TranscriptEntryKind::Message(item) => {
            format_message_entry_with_width(item, false, selected, outcome, width, show_reasoning)
        }
        TranscriptEntryKind::ToolResult(tool) => {
            format_tool_result_entry(tool, false, selected, tool_output_verbosity, width)
        }
        TranscriptEntryKind::Log(message) => format_log_entry(message, false, selected),
        TranscriptEntryKind::PlanCard(data) => format_plan_card_entry(data, false),
        TranscriptEntryKind::Diff(data) => format_diff_card_entry(data, false, selected),
        TranscriptEntryKind::Reasoning(snapshot) => {
            if show_reasoning {
                let mut lines = reasoning_block_lines(&snapshot.display_text, false, selected);
                lines.push(Line::from(""));
                lines
            } else {
                Vec::new()
            }
        }
    }
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
            app.show_reasoning_usage,
        ));
    }
    if app.show_reasoning_usage && !app.pending_reasoning.trim().is_empty() {
        lines.extend(streaming_reasoning_lines(&app.pending_reasoning));
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

fn streaming_reasoning_lines(text: &str) -> Vec<Line<'static>> {
    let style = Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC);
    let mut lines = vec![Line::from(Span::styled("▾ thinking…".to_string(), style))];
    for raw in text.lines() {
        lines.push(Line::from(Span::styled(format!("▏ {}", raw), style)));
    }
    lines.push(Line::from(""));
    lines
}

fn pending_assistant_lines(app: &TuiApp) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if app.show_reasoning_usage && !app.pending_reasoning.trim().is_empty() {
        lines.extend(streaming_reasoning_lines(&app.pending_reasoning));
    }
    if let Some(content) = pending_assistant_display_content(app) {
        lines.extend(assistant_text_lines(
            false,
            turn_coin_span(app),
            &content,
            Style::default(),
        ));
    }
    lines
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

/// One-row constraint reserved above the composer for the PLAN MODE
/// indicator. Returns 0 in Build mode so the layout is unchanged for the
/// majority case (audit f07-plan-mode-prompt-overlay: only Plan mode pays
/// the row).
fn plan_mode_indicator_height(app: &TuiApp) -> u16 {
    match app.mode {
        SessionMode::Plan => 1,
        SessionMode::Build => 0,
    }
}

/// Build the styled "[PLAN MODE] Shift+Tab to exit" line. Uses the
/// existing `MODE_PURPLE` palette entry (no new colors) and ASCII
/// brackets with a Unicode `⊕` glyph — matches the other status glyphs
/// (`⟳`, `▸`) already used in this file.
pub(crate) fn format_plan_mode_indicator_line() -> Line<'static> {
    let label_style = Style::default()
        .fg(MODE_PURPLE)
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled("⊕ PLAN MODE", label_style),
        Span::styled(" · Shift+Tab to exit", Style::default().fg(QUIET)),
    ])
}

fn render_plan_mode_indicator(frame: &mut Frame<'_>, area: Rect, _app: &TuiApp) {
    let paragraph = Paragraph::new(format_plan_mode_indicator_line());
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
    format_transcript_entry_with_width(entry, selected, tool_output_verbosity, outcome, None, true)
}

fn format_transcript_entry_with_width(
    entry: &TranscriptEntry,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
    outcome: MessageOutcome,
    width: Option<u16>,
    show_reasoning: bool,
) -> Vec<Line<'static>> {
    match &entry.kind {
        TranscriptEntryKind::Message(item) => format_message_entry_with_width(
            item,
            entry.collapsed,
            selected,
            outcome,
            width,
            show_reasoning,
        ),
        TranscriptEntryKind::ToolResult(tool) => format_tool_result_entry(
            tool,
            entry.collapsed,
            selected,
            tool_output_verbosity,
            width,
        ),
        TranscriptEntryKind::Log(message) => format_log_entry(message, entry.collapsed, selected),
        TranscriptEntryKind::PlanCard(data) => format_plan_card_entry(data, entry.collapsed),
        TranscriptEntryKind::Diff(data) => format_diff_card_entry(data, entry.collapsed, selected),
        TranscriptEntryKind::Reasoning(snapshot) => {
            if show_reasoning {
                let mut lines =
                    reasoning_block_lines(&snapshot.display_text, entry.collapsed, selected);
                lines.push(Line::from(""));
                lines
            } else {
                Vec::new()
            }
        }
    }
}

fn format_plan_card_entry(
    data: &render::plan_card::PlanCardData,
    collapsed: bool,
) -> Vec<Line<'static>> {
    // Collapsed cards show just the header so users can fold a long
    // plan out of view without losing the anchor.
    let lines = render::plan_card::render_plan_card(data);
    if collapsed {
        return lines.into_iter().take(1).collect();
    }
    lines
}

/// Run the `/diff` slash command: capture a worktree diff (tracked +
/// untracked) via `GitVcs::snapshot` and push a styled card into the
/// transcript. On a clean tree or a non-git workspace we surface a log
/// advisory instead of an empty card. Mirrors codex's `get_git_diff` +
/// `disable_output_cap` UX — never truncated, always renderable via the
/// existing `render::diff` helpers.
fn handle_slash_diff(app: &mut TuiApp) {
    let workspace_root = app.workspace_root.clone();
    let vcs = match GitVcs::open(&workspace_root) {
        Ok(vcs) => vcs,
        Err(err) => {
            app.push_log(format!("/diff failed to open workspace VCS: {err}"));
            return;
        }
    };
    let snapshot = vcs.snapshot(
        DiffMode::Worktree,
        DiffOptions {
            include_patch: true,
            ..DiffOptions::default()
        },
    );
    if snapshot.vcs.kind != VcsKind::Git {
        app.push_log("/diff: workspace is not a git repository".to_string());
        return;
    }
    if !snapshot.errors.is_empty() {
        for error in &snapshot.errors {
            app.push_log(format!("/diff git error: {error}"));
        }
    }
    if snapshot.files.is_empty() {
        app.push_log("/diff: no uncommitted changes".to_string());
        return;
    }
    let card = build_diff_card(&snapshot);
    app.push_diff_card(card);
}

/// Build a renderable diff card from a worktree snapshot. Files are
/// listed in path order with a per-file header (`path +A -D`) followed
/// by the styled patch body. Binary files render a one-line marker.
fn build_diff_card(snapshot: &squeezy_vcs::DiffSnapshot) -> DiffCardData {
    let summary = format!(
        "{} file{} · +{} -{}{}",
        snapshot.summary.files_changed,
        if snapshot.summary.files_changed == 1 {
            ""
        } else {
            "s"
        },
        snapshot.summary.additions,
        snapshot.summary.deletions,
        if snapshot.summary.untracked_files > 0 {
            format!(" · {} untracked", snapshot.summary.untracked_files)
        } else {
            String::new()
        },
    );
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut plain = String::new();
    for (index, file) in snapshot.files.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
            plain.push('\n');
        }
        let header = format!(
            "{} {}{}",
            file.code,
            file.path,
            if file.additions > 0 || file.deletions > 0 {
                format!(" +{} -{}", file.additions, file.deletions)
            } else {
                String::new()
            }
        );
        lines.push(Line::from(Span::styled(
            header.clone(),
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        )));
        plain.push_str(&header);
        plain.push('\n');
        let file_lines = render::diff::render_diff_file(file);
        for line in &file_lines {
            plain.push_str(
                &line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join(""),
            );
            plain.push('\n');
        }
        lines.extend(file_lines);
    }
    DiffCardData {
        summary,
        plain,
        lines,
    }
}

fn format_diff_card_entry(
    data: &DiffCardData,
    collapsed: bool,
    selected: bool,
) -> Vec<Line<'static>> {
    let header = action_line_spans(
        selected,
        "✱",
        GOLD,
        "Diff",
        GOLD,
        vec![Span::styled(
            data.summary.clone(),
            Style::default().fg(QUIET),
        )],
    );
    if collapsed {
        return vec![header];
    }
    let mut lines = Vec::with_capacity(data.lines.len() + 1);
    lines.push(header);
    lines.extend(data.lines.iter().cloned());
    lines
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

pub(crate) fn dedupe_assistant_repeated_tool_output(
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
    let mut content = normalize_fence_boundaries(content);
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

/// Insert a newline before any ```` ``` ```` that's glued to non-whitespace
/// on the same line. Models occasionally emit `"prose.```text\nbody\n```"`
/// without breaking the line before the opening fence; the fence-aware
/// dedup helpers below scan line-by-line and would miss the duplicate.
fn normalize_fence_boundaries(content: &str) -> String {
    const FENCE: &str = "```";
    let mut out = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(FENCE.as_bytes()) {
            let preceded_by_break = matches!(out.chars().last(), None | Some('\n'));
            if !preceded_by_break {
                out.push('\n');
            }
            out.push_str(FENCE);
            i += FENCE.len();
            continue;
        }
        let ch = content[i..].chars().next().expect("non-empty");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn pending_assistant_display_content(app: &TuiApp) -> Option<String> {
    if app.pending_assistant.trim_is_empty() {
        return None;
    }
    let text = app.pending_assistant.text();
    let content = assistant_content_without_repeated_tool_output(app, &text);
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
    format_message_entry_with_width(item, collapsed, selected, outcome, None, true)
}

fn format_message_entry_with_width(
    item: &TranscriptItem,
    collapsed: bool,
    selected: bool,
    outcome: MessageOutcome,
    width: Option<u16>,
    show_reasoning: bool,
) -> Vec<Line<'static>> {
    if item.role == Role::User {
        return format_user_prompt_entry(item, selected, width);
    }
    if item.role == Role::Assistant {
        return format_assistant_message_entry(item, collapsed, selected, outcome, show_reasoning);
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
    if item.role == Role::System
        && !failed
        && let Some(lines) = format_accounting_block_entry(selected, &item.content)
    {
        return lines;
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

/// Render the `/cost` and `/context` outputs with per-token coloring:
/// group words pop in `GOLD`, `key=` labels dim to `QUIET`, dollar values
/// pop in `AMBER`, and zero/dash/unknown values fade to `QUIET` so the
/// real numbers carry the eye. Returns `None` for any system message that
/// is not an accounting block — the caller falls through to the default
/// single-style renderer.
fn format_accounting_block_entry(selected: bool, content: &str) -> Option<Vec<Line<'static>>> {
    let mut iter = content.lines();
    let header = iter.next()?;
    if header != "Cost accounting" && header != "Context accounting" {
        return None;
    }
    let header_span = Span::styled(
        header.to_string(),
        Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
    );
    let mut lines = Vec::with_capacity(content.lines().count());
    lines.push(action_line_spans(
        selected,
        "• ",
        GOLD,
        "Noted",
        GOLD,
        vec![header_span],
    ));
    for body in iter {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(accounting_body_spans(body));
        lines.push(Line::from(spans));
    }
    Some(lines)
}

fn accounting_body_spans(line: &str) -> Vec<Span<'static>> {
    // Sentences without any `key=value` token are dimmed wholesale (the
    // accuracy epilogue, the `provider_stored_context=...` narrative line).
    if !line.contains('=') || line.starts_with("accuracy=") {
        return vec![Span::styled(line.to_string(), Style::default().fg(QUIET))];
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut first_token = true;
    for token in line.split_inclusive(' ') {
        let (text, trailing) = if let Some(stripped) = token.strip_suffix(' ') {
            (stripped, " ")
        } else {
            (token, "")
        };
        if let Some(eq_idx) = text.find('=') {
            let (key, rest) = text.split_at(eq_idx);
            let value = &rest[1..];
            spans.push(Span::styled(format!("{key}="), Style::default().fg(QUIET)));
            spans.push(Span::styled(
                value.to_string(),
                accounting_value_style(value),
            ));
        } else {
            // A bare word — the leading group label (`tools`,
            // `subagents`, `transmitted_request`, …) or a trailing
            // parenthetical like `(estimated from ...)`.
            let style = if first_token {
                Style::default().fg(GOLD)
            } else {
                Style::default().fg(QUIET)
            };
            spans.push(Span::styled(text.to_string(), style));
        }
        if !trailing.is_empty() {
            spans.push(Span::raw(trailing));
        }
        first_token = false;
    }
    spans
}

fn accounting_value_style(value: &str) -> Style {
    if value.is_empty() {
        return Style::default().fg(QUIET);
    }
    if value.starts_with('$') {
        return Style::default().fg(AMBER);
    }
    // Values that signal absence/zero fade so the eye lands on the real
    // numbers. Percent strings like `0.00%` count as zero too.
    let is_zero_percent = value
        .strip_suffix('%')
        .and_then(|prefix| prefix.parse::<f64>().ok())
        .is_some_and(|value| value == 0.0);
    if matches!(
        value,
        "-" | "0" | "unknown" | "inactive" | "absent" | "false"
    ) || is_zero_percent
    {
        return Style::default().fg(QUIET);
    }
    Style::default()
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
    let mut spans = vec![user_prompt_marker_span(marker)];
    // First content line of a user message can carry a `/<command>` prefix.
    // We keep the rest of the line in white so multi-word prompts like
    // `/help changing the model` stay legible while the recognised command
    // pops in amber.
    let slash_len = (marker == "> ")
        .then(|| input::match_slash_command_prefix(line))
        .flatten();
    if let Some(len) = slash_len {
        let (prefix, rest) = line.split_at(len);
        spans.push(Span::styled(
            prefix.to_string(),
            Style::default().fg(AMBER).bg(PROMPT_BG),
        ));
        if !rest.is_empty() {
            spans.push(Span::styled(
                rest.to_string(),
                Style::default().fg(Color::White).bg(PROMPT_BG),
            ));
        }
    } else {
        spans.push(Span::styled(
            line.to_string(),
            Style::default().fg(Color::White).bg(PROMPT_BG),
        ));
    }
    spans.push(Span::styled(padding, Style::default().bg(PROMPT_BG)));
    Line::from(spans)
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
    show_reasoning: bool,
) -> Vec<Line<'static>> {
    let color = if outcome == MessageOutcome::Failed {
        ERROR_RED
    } else {
        SUCCESS_GREEN
    };
    let mut lines = Vec::new();
    if show_reasoning
        && let Some(snapshot) = item.reasoning.as_ref()
        && !snapshot.display_text.trim().is_empty()
    {
        lines.extend(reasoning_block_lines(
            &snapshot.display_text,
            collapsed,
            false,
        ));
    }
    if collapsed {
        lines.push(assistant_line(
            selected,
            assistant_static_span(color),
            collapsed_content_summary(&item.content),
            Style::default(),
        ));
    } else {
        lines.extend(assistant_text_lines(
            selected,
            assistant_static_span(color),
            &item.content,
            Style::default(),
        ));
    }
    lines.push(Line::from(""));
    lines
}

fn reasoning_block_lines(text: &str, collapsed: bool, selected: bool) -> Vec<Line<'static>> {
    let style = Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC);
    let marker = if selected { "> " } else { "" };
    let mut lines = Vec::new();
    let body_lines: Vec<&str> = text.lines().collect();
    if collapsed {
        let summary = body_lines
            .first()
            .copied()
            .map(|first| compact_text(first, 120))
            .unwrap_or_default();
        let suffix = if body_lines.len() > 1 {
            format!(" … +{} lines (Ctrl-E to expand)", body_lines.len() - 1)
        } else {
            String::new()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}▸ reasoning: {summary}{suffix}"),
            style,
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("{marker}▾ reasoning ({} lines)", body_lines.len().max(1)),
            style,
        )));
        for raw in body_lines {
            lines.push(Line::from(Span::styled(format!("▏ {}", raw), style)));
        }
    }
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

fn text_has_collapsible_content(content: &str) -> bool {
    content.lines().count() > 1 || content.len() > 160
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
    let mut lines = vec![action_line_spans(
        selected,
        marker,
        color,
        action,
        color,
        summary_spans,
    )];
    if collapsed {
        lines.extend(collapsed_tool_preview_lines(
            tool,
            tool_output_verbosity,
            width,
        ));
    } else {
        lines.extend(expanded_tool_detail_lines(tool, tool_output_verbosity));
    }
    lines
}

/// Whether this tool result was triggered by the user typing `!<command>`
/// (a "direct user shell" call), as opposed to being initiated by the
/// model. Mirrors codex's `ExecCall::is_user_shell_command`. The agent
/// stamps `direct_user_shell: true` on these calls in
/// `local_shell_command_call` (crates/squeezy-agent/src/lib.rs).
fn is_user_shell_call(tool: &ToolTranscript) -> bool {
    let from_call = tool
        .call
        .as_ref()
        .and_then(|call| call.arguments.get("direct_user_shell"))
        .and_then(|value| value.as_bool());
    if let Some(value) = from_call {
        return value;
    }
    tool.result
        .content
        .get("direct_user_shell")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Default cap for the collapsed tool-card preview.
fn tool_preview_line_cap(tool: &ToolTranscript) -> usize {
    if is_user_shell_call(tool) {
        USER_SHELL_TOOL_CALL_MAX_LINES
    } else {
        TOOL_CALL_MAX_LINES
    }
}

/// Tools whose expanded body is *the* point of the card (a small,
/// structured artifact like a diff or a per-file summary) and which we
/// therefore do not truncate by default. Matches codex's behaviour of
/// never capping patch output.
fn tool_bypasses_preview_cap(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "apply_patch" | "write_file" | "plan_patch" | "diff_context"
    )
}

fn collapsed_tool_preview_lines(
    tool: &ToolTranscript,
    tool_output_verbosity: ToolOutputVerbosity,
    _width: Option<u16>,
) -> Vec<Line<'static>> {
    if tool_bypasses_preview_cap(tool.result.tool_name.as_str()) {
        return expanded_tool_detail_lines(tool, tool_output_verbosity);
    }
    let detail = expanded_tool_detail_lines(tool, tool_output_verbosity);
    let cap = tool_preview_line_cap(tool);
    head_tail_truncate_lines(detail, cap)
}

/// Head-tail truncate a list of rendered detail lines, inserting a single
/// "… +N lines (Ctrl-E to expand)" ellipsis between the head and tail
/// when the total exceeds `2 * cap`. Cap is the maximum number of lines
/// to keep on EACH end. Mirrors codex's `output_ellipsis_line` UX —
/// wording stays consistent with the existing diff renderer
/// (see `render::diff::head_tail`).
fn head_tail_truncate_lines(lines: Vec<Line<'static>>, cap: usize) -> Vec<Line<'static>> {
    if cap == 0 || lines.len() <= cap.saturating_mul(2) {
        return lines;
    }
    let omitted = lines.len().saturating_sub(cap * 2);
    let mut out = Vec::with_capacity(cap * 2 + 1);
    out.extend(lines.iter().take(cap).cloned());
    out.push(detail_line(
        false,
        QUIET,
        format!("… +{omitted} lines (Ctrl-E to expand)"),
    ));
    out.extend(
        lines
            .into_iter()
            .rev()
            .take(cap)
            .collect::<Vec<_>>()
            .into_iter()
            .rev(),
    );
    out
}

fn format_log_entry(message: &str, collapsed: bool, selected: bool) -> Vec<Line<'static>> {
    let color = log_color(message);
    if collapsed && !is_failure_log(message) {
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

/// Extract the file paths an apply_patch / write_file invocation targets,
/// used to dedupe transient failure/success pairs in [`TuiApp::recent_edit_failures`].
/// Falls back across the several places the path may live: the result
/// content (success path), the result `failed_path` field (apply_patch
/// fallback), and the call arguments (write_file).
fn edit_target_paths(result: &ToolResult, call: Option<&ToolCall>) -> Vec<PathBuf> {
    if !matches!(result.tool_name.as_str(), "apply_patch" | "write_file") {
        return Vec::new();
    }
    let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
    if let Some(files) = result.content["files"].as_array() {
        for item in files {
            if let Some(path) = item["path"].as_str() {
                paths.insert(PathBuf::from(path));
            }
        }
    }
    if let Some(files) = result.content["checkpoint"]["files"].as_array() {
        for item in files {
            if let Some(path) = item["path"].as_str() {
                paths.insert(PathBuf::from(path));
            }
        }
    }
    if let Some(failed_path) = result.content["failed_path"].as_str() {
        paths.insert(PathBuf::from(failed_path));
    }
    if let Some(path) = result.content["path"].as_str() {
        paths.insert(PathBuf::from(path));
    }
    if paths.is_empty()
        && let Some(call) = call
        && let Some(path) = string_arg(&call.arguments, "path")
    {
        paths.insert(PathBuf::from(path));
    }
    paths.into_iter().collect()
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

pub(crate) fn tool_call_label(call: &ToolCall) -> String {
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
            .map(|query| format!("definition search for {}", compact_text(&query, 60)))
            .unwrap_or_else(|| "definition search".to_string()),
        "reference_search" => string_arg(&call.arguments, "query")
            .or_else(|| string_arg(&call.arguments, "symbol_id"))
            .map(|query| format!("reference search for {}", compact_text(&query, 60)))
            .unwrap_or_else(|| "reference search".to_string()),
        "symbol_context" => string_arg(&call.arguments, "query")
            .map(|query| format!("symbol context for {}", compact_text(&query, 60)))
            .unwrap_or_else(|| "symbol context".to_string()),
        "grep" => string_arg(&call.arguments, "query")
            .or_else(|| string_arg(&call.arguments, "pattern"))
            .map(|query| format!("grep {}", compact_text(&query, 60)))
            .unwrap_or_else(|| "grep".to_string()),
        "glob" => string_arg(&call.arguments, "pattern")
            .map(|pattern| format!("glob {}", compact_text(&pattern, 60)))
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
            .map(|query| format!("web search {}", compact_text(&query, 60)))
            .unwrap_or_else(|| "web search".to_string()),
        _ => call.name.clone(),
    }
}

fn active_tool_spans(call: &ToolCall) -> Vec<Span<'static>> {
    let name_span = Span::styled(
        friendly_tool_name(&call.name),
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    );
    let args = active_tool_args(call);
    if args.is_empty() {
        return vec![name_span];
    }
    let mut spans = vec![
        name_span,
        Span::styled(
            ": ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
    ];
    if matches!(call.name.as_str(), "shell" | "verify") {
        spans.extend(command_spans(&compact_text(&args, 80)));
    } else {
        spans.push(Span::styled(
            compact_text(&args, 80),
            Style::default().fg(Color::White),
        ));
    }
    spans
}

/// Per-tool elapsed segment ("· 3s") appended after the tool label.
/// Returns no spans when the heartbeat hasn't reported elapsed yet — the
/// turn-level "(Ns · esc to interrupt)" already covers that case.
fn active_tool_elapsed_spans(elapsed_ms: u64) -> Vec<Span<'static>> {
    let secs = elapsed_ms / 1000;
    if secs == 0 {
        return Vec::new();
    }
    vec![
        Span::styled(" · ", Style::default().fg(QUIET)),
        Span::styled(format!("{secs}s"), Style::default().fg(QUIET)),
    ]
}

pub(crate) fn is_control_tool_name(name: &str) -> bool {
    matches!(name, "update_task_state" | "load_tool_schema")
}

/// Argument snippet for the working-row label. Returns only the
/// arguments (no tool-name prefix), so the row reads
/// `Friendly: <args>` without duplicating the tool identity. Returns an
/// empty string when no useful snippet is available — the row then ends
/// at the colon.
fn active_tool_args(call: &ToolCall) -> String {
    match call.name.as_str() {
        "shell" | "verify" => string_arg(&call.arguments, "command")
            .or_else(|| string_arg(&call.arguments, "description"))
            .unwrap_or_default(),
        "decl_search" => {
            let language = string_arg(&call.arguments, "language");
            let kind = string_arg(&call.arguments, "kind").map(|value| kind_label(&value));
            let query = string_arg(&call.arguments, "query");
            let mut parts: Vec<String> = Vec::new();
            if let Some(language) = language {
                parts.push(language);
            }
            if let Some(kind) = kind {
                parts.push(kind);
            }
            if let Some(query) = query {
                parts.push(query);
            }
            parts.join(" ")
        }
        "definition_search" | "reference_search" | "symbol_context" | "grep" | "websearch" => {
            string_arg(&call.arguments, "query")
                .or_else(|| string_arg(&call.arguments, "pattern"))
                .or_else(|| string_arg(&call.arguments, "symbol_id"))
                .unwrap_or_default()
        }
        "glob" => string_arg(&call.arguments, "pattern").unwrap_or_default(),
        "read_file" | "read_slice" | "write_file" => {
            string_arg(&call.arguments, "path").unwrap_or_default()
        }
        "plan_patch" => string_arg(&call.arguments, "objective").unwrap_or_default(),
        "webfetch" => string_arg(&call.arguments, "url").unwrap_or_default(),
        _ => String::new(),
    }
}

/// Title-cased display name for the working-row label (codex-style
/// "Shell: …", "Read: …"). Known tools get explicit casing; unknown tools
/// fall back to ASCII-uppercase first letter so a server-defined tool
/// like `slack_search` reads as `Slack_search` rather than the raw slug.
fn friendly_tool_name(tool_name: &str) -> String {
    match tool_name {
        "shell" => "Shell".to_string(),
        "verify" => "Verify".to_string(),
        "read_file" | "read_slice" => "Read".to_string(),
        "read_tool_output" => "Expand".to_string(),
        "grep" => "Grep".to_string(),
        "glob" => "Glob".to_string(),
        "write_file" => "Write".to_string(),
        "apply_patch" => "Patch".to_string(),
        "plan_patch" => "Plan".to_string(),
        "repo_map" => "Repo map".to_string(),
        "diff_context" => "Diff".to_string(),
        "decl_search" => "Declarations".to_string(),
        "definition_search" => "Definition".to_string(),
        "reference_search" => "References".to_string(),
        "symbol_context" => "Symbol".to_string(),
        "hierarchy" => "Hierarchy".to_string(),
        "upstream_flow" => "Upstream".to_string(),
        "downstream_flow" => "Downstream".to_string(),
        "webfetch" => "Fetch".to_string(),
        "websearch" => "Search".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        }
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
            lines.extend(render_diff_patch_full_lines(patch, file.path.as_str()));
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

fn render_diff_patch_full_lines(patch: &str, path: &str) -> Vec<Line<'static>> {
    render::diff::render_patch_full_lines(patch, render::diff::language_hint_from_path(path))
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

pub(crate) fn tool_result_status_text(result: &ToolResult) -> String {
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
pub(crate) enum TurnVisualState {
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
    // At idle the coin is a steady AMBER ●. Animating it forced a real
    // cell change every 320 ms, which kept terminal-emulator per-tab
    // activity indicators buzzing forever even though the agent was
    // doing nothing.
    if app.turn_visual == TurnVisualState::Idle {
        return Span::styled("●", Style::default().fg(AMBER));
    }
    let color = if (prompt_elapsed_ms(app) / 800).is_multiple_of(2) {
        GOLD
    } else {
        AMBER
    };
    Span::styled(prompt_coin_frame(app), Style::default().fg(color))
}

fn prompt_coin_frame(app: &TuiApp) -> &'static str {
    const FRAMES: [&str; 8] = ["●", "◕", "◐", "◔", "○", "◔", "◑", "◕"];
    if app.turn_visual == TurnVisualState::Idle {
        return FRAMES[0];
    }
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

pub(crate) fn compact_text(text: &str, limit: usize) -> String {
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
    let overlay_lines = app
        .overlay
        .as_ref()
        .map(|o| o.render_lines().len())
        .unwrap_or(0);
    let mention_lines = app
        .mention_popup
        .as_ref()
        .map(|p| p.matches.len().min(5))
        .unwrap_or(0);
    let suggestion_lines = if overlay_lines == 0 && mention_lines == 0 {
        slash_suggestions(&app.input).len()
    } else {
        0
    };
    let popup_height = overlay_lines + mention_lines;
    let max_height = (PROMPT_MAX_HEIGHT as usize).max(popup_height + PROMPT_MIN_HEIGHT as usize);
    prompt_visual_line_count(&app.input, width)
        .saturating_add(2)
        .saturating_add(overlay_lines)
        .saturating_add(mention_lines)
        .saturating_add(suggestion_lines)
        .clamp(PROMPT_MIN_HEIGHT as usize, max_height) as u16
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
    // Slash highlight only applies to the first line of input — slash
    // commands are always at the start of a prompt, never embedded later.
    let slash_len = parts
        .first()
        .and_then(|first| input::match_slash_command_prefix(first));
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
            let slash_split = if index == 0 { slash_len } else { None };
            let style_text_at = |abs_offset: usize| -> Style {
                let base = Style::default().bg(PROMPT_BG);
                match slash_split {
                    Some(len) if abs_offset < len => base.fg(AMBER),
                    _ => base.fg(Color::White),
                }
            };
            if cursor >= line_start && cursor <= line_end {
                let split_at = cursor.saturating_sub(line_start).min(line.len());
                let (before, after) = line.split_at(split_at);
                if !before.is_empty() {
                    push_styled_segments(
                        &mut spans,
                        before,
                        line_start,
                        slash_split,
                        style_text_at,
                    );
                }
                spans.push(prompt_cursor_span());
                if !after.is_empty() {
                    let after_start = line_start + split_at;
                    push_styled_segments(
                        &mut spans,
                        after,
                        after_start,
                        slash_split,
                        style_text_at,
                    );
                }
            } else {
                push_styled_segments(&mut spans, line, line_start, slash_split, style_text_at);
            }
            line_start = line_end.saturating_add(1);
            Line::from(spans)
        })
        .collect()
}

/// Push one or two styled spans for `chunk`, splitting on the slash
/// boundary when the chunk straddles it so the amber prefix and the white
/// rest remain visually distinct on the live input row.
fn push_styled_segments(
    spans: &mut Vec<Span<'static>>,
    chunk: &str,
    chunk_start: usize,
    slash_split: Option<usize>,
    style_text_at: impl Fn(usize) -> Style,
) {
    let chunk_end = chunk_start + chunk.len();
    if let Some(split) = slash_split
        && chunk_start < split
        && split < chunk_end
    {
        let local = split - chunk_start;
        let (head, tail) = chunk.split_at(local);
        if !head.is_empty() {
            spans.push(Span::styled(head.to_string(), style_text_at(chunk_start)));
        }
        if !tail.is_empty() {
            spans.push(Span::styled(tail.to_string(), style_text_at(split)));
        }
        return;
    }
    spans.push(Span::styled(chunk.to_string(), style_text_at(chunk_start)));
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
    let task_active = turn_in_progress(app);
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
            let dimmed = command.is_dimmed(task_active);
            let marker = if selected { "› " } else { "  " };
            let command_padding =
                " ".repeat(command_width.saturating_sub(command.name.chars().count()) + 2);
            let name_color = if dimmed {
                QUIET
            } else if selected {
                GOLD
            } else {
                AMBER
            };
            let mut name_style = Style::default().fg(name_color);
            if dimmed {
                name_style = name_style.add_modifier(Modifier::DIM);
            }
            let mut description_style = Style::default().fg(QUIET);
            if dimmed {
                description_style = description_style.add_modifier(Modifier::DIM);
            }
            let mut spans = vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected { GOLD } else { QUIET }),
                ),
                Span::styled(command.name, name_style),
                Span::styled(command_padding, Style::default().fg(QUIET)),
                Span::styled(command.description, description_style),
            ];
            if let Some(hint) = command.parameter_hint {
                let hint_text = format!(" {hint}");
                spans.push(Span::styled(
                    hint_text,
                    Style::default()
                        .fg(QUIET)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC),
                ));
            }
            let badges = command.capability_badges();
            if !badges.is_empty() {
                spans.push(Span::styled(
                    format!("  [{}]", badges.join("|")),
                    Style::default()
                        .fg(if dimmed { QUIET } else { AMBER })
                        .add_modifier(if dimmed {
                            Modifier::DIM | Modifier::ITALIC
                        } else {
                            Modifier::ITALIC
                        }),
                ));
            }
            if dimmed {
                spans.push(Span::styled(
                    "  (unavailable during turn)",
                    Style::default()
                        .fg(QUIET)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC),
                ));
            }
            Line::from(spans)
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
    let overlay_lines = overlay_picker_lines(app);
    let mention_lines = if overlay_lines.is_empty() {
        mention_popup_lines(app)
    } else {
        Vec::new()
    };
    let suggestion_lines = if overlay_lines.is_empty() && mention_lines.is_empty() {
        slash_suggestion_lines(app)
    } else {
        Vec::new()
    };
    let overlay_height = overlay_lines.len() + mention_lines.len() + suggestion_lines.len();
    let prompt_height = area.height.saturating_sub(overlay_height as u16);
    let mut lines = prompt_input_lines(app, prompt_height);
    lines.extend(overlay_lines);
    lines.extend(mention_lines);
    lines.extend(suggestion_lines);
    let scroll = lines.len().saturating_sub(area.height as usize) as u16;
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(Color::White).bg(PROMPT_BG))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn overlay_picker_lines(app: &TuiApp) -> Vec<Line<'static>> {
    app.overlay
        .as_ref()
        .map(|o| o.render_lines())
        .unwrap_or_default()
}

fn mention_popup_lines(app: &TuiApp) -> Vec<Line<'static>> {
    let Some(popup) = app.mention_popup.as_ref() else {
        return Vec::new();
    };
    if popup.is_empty() {
        return Vec::new();
    }
    popup
        .matches
        .iter()
        .take(5)
        .enumerate()
        .map(|(index, path)| {
            let selected = index == popup.selected;
            let marker = if selected { "› " } else { "  " };
            let display = path.display().to_string();
            let style = if selected {
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected { GOLD } else { QUIET }),
                ),
                Span::styled(display, style),
            ])
        })
        .collect()
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
    let mut base = format!("dir {} · git {}", app.directory, branch);
    if let Some(segment) = turn_progress_segment(app) {
        base.push_str(" · ");
        base.push_str(&segment);
    }
    base
}

/// Compact mid-turn progress segment for the status bar — replaces the
/// per-tool "running this turn: ... tokens · $X" and per-second "shell
/// still running (Ns)" lines that used to be appended to the transcript
/// log.
fn turn_progress_segment(app: &TuiApp) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(tool) = app.active_tool.as_deref()
        && let Some(elapsed_ms) = app.active_tool_elapsed_ms
    {
        parts.push(format!("⟳ {tool} {:.0}s", elapsed_ms as f64 / 1000.0));
    }
    if let Some(progress) = app.turn_progress {
        parts.push(format!(
            "{} tools · {} in · ${:.4}",
            progress.tool_count,
            compact_token_count(progress.input_tokens),
            progress.micro_usd as f64 / 1_000_000.0,
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn compact_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn mode_status_text(app: &TuiApp) -> String {
    let base = format!("{} mode (Shift+Tab to cycle)", title_case_mode(app.mode));
    let Some(plan_id) = app.current_plan_id.as_deref() else {
        return base;
    };
    // Truncate the hex tail so the status bar stays compact on narrow
    // windows. Step count is derived from the on-disk file body so it
    // reflects the *current* shape of the active plan, including any
    // in-place refinements via apply_patch (PR-C).
    let short_id = short_plan_id(plan_id);
    let sid = app.plan_session_id();
    let plan_path = proposed_plan::plan_file_for(&app.workspace_root, sid, plan_id);
    let step_count = proposed_plan::read_plan_body(&plan_path)
        .map(|body| count_plan_steps(&body))
        .unwrap_or(0);
    if step_count == 0 {
        format!("{base} · {short_id}")
    } else {
        format!("{base} · {short_id} ({step_count} steps)")
    }
}

/// Render a plan id as `plan-<first-6-hex>` for the status bar. Falls
/// back to the full id when the input does not match the expected
/// `plan-<hex>` shape.
fn short_plan_id(plan_id: &str) -> String {
    let Some(hex) = plan_id.strip_prefix("plan-") else {
        return plan_id.to_string();
    };
    let head: String = hex.chars().take(6).collect();
    if head.is_empty() {
        plan_id.to_string()
    } else {
        format!("plan-{head}")
    }
}

/// Count top-level numbered list items (e.g. `1.`, `12)`) in a plan
/// body. Heading lines (`#`, `##`) and nested indented items are
/// ignored. Matches what the styled card renders as the step list.
pub(crate) fn count_plan_steps(body: &str) -> usize {
    body.lines()
        .filter(|line| {
            // Top-level only: no leading whitespace.
            if line.starts_with(|c: char| c.is_whitespace()) {
                return false;
            }
            let mut chars = line.chars().peekable();
            let mut saw_digit = false;
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() {
                    saw_digit = true;
                    chars.next();
                } else {
                    break;
                }
            }
            if !saw_digit {
                return false;
            }
            matches!(chars.next(), Some('.') | Some(')'))
                && matches!(chars.next(), Some(' ') | Some('\t'))
        })
        .count()
}

pub(crate) fn title_case_mode(mode: SessionMode) -> &'static str {
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
    let paragraph = Paragraph::new(format_status_lines(app, area.width));
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
    let hints_span = Span::styled(format_status_hints(app), Style::default().fg(QUIET));
    let detail = configured_status_line_items(app).and_then(|items| {
        status::render_status_detail_line(app, &items, app.status_line_use_colors)
    });
    // When the user has configured `[tui].status_line`, the detail items
    // take the place of `dir … · git …` on row 1 (otherwise both rows
    // duplicate the same data). Mode label stays right-aligned. Without a
    // configured list, fall back to the historical overview layout.
    let top = match detail {
        Some(detail_line) => compose_status_overview_with_detail(detail_line, app, width),
        None => format_status_overview_line(app, width),
    };
    let bottom = if detail_was_present(app) {
        Line::from(hints_span)
    } else if app.status_verbosity == StatusVerbosity::Verbose {
        Line::from(Span::styled(
            format!(
                "{} · {}",
                format_status_details(app),
                format_status_hints(app)
            ),
            Style::default().fg(QUIET),
        ))
    } else {
        Line::from(hints_span)
    };
    vec![top, bottom]
}

/// Right-align the mode label on a status row whose left side is the
/// codex-style detail line. Mirrors [`format_status_overview_line`]'s
/// alignment math but preserves the detail line's styled spans.
fn compose_status_overview_with_detail(
    mut detail: Line<'static>,
    app: &TuiApp,
    width: u16,
) -> Line<'static> {
    let right = mode_status_text(app);
    let right_width = right.chars().count();
    let detail_width: usize = detail
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    let padding_width = (width as usize)
        .saturating_sub(detail_width.saturating_add(right_width))
        .max(1);
    detail.spans.push(Span::raw(" ".repeat(padding_width)));
    detail.spans.push(Span::styled(
        right,
        Style::default().fg(mode_status_color(app.mode)),
    ));
    detail
}

fn detail_was_present(app: &TuiApp) -> bool {
    configured_status_line_items(app).is_some()
}

/// User-configured `[tui].status_line`. `None` when the TOML key is unset
/// (so the renderer falls back to the historical hints-only second row)
/// or when the user deliberately set an empty list (also "no detail row").
fn configured_status_line_items(app: &TuiApp) -> Option<Vec<status::StatusLineItem>> {
    match &app.status_line_items {
        Some(list) if list.is_empty() => None,
        Some(list) => Some(list.clone()),
        None => None,
    }
}

/// Parse the TOML-side `[tui].status_line` list into typed items, dropping
/// unknown identifiers. Returns `None` when the TOML key was unset.
fn parse_status_line_items(raw: Option<&[String]>) -> Option<Vec<status::StatusLineItem>> {
    let raw = raw?;
    Some(
        raw.iter()
            .filter_map(|s| s.parse::<status::StatusLineItem>().ok())
            .collect(),
    )
}

#[cfg(test)]
fn format_status_context(app: &TuiApp) -> String {
    format!("{}  {}", status_left_text(app), mode_status_text(app))
}

fn format_status_details(app: &TuiApp) -> String {
    status::render_status_details(app)
}

pub(crate) fn context_window_pct(used: u64, threshold: u64) -> u64 {
    if threshold == 0 {
        return 0;
    }
    let ratio = (used as f64 / threshold as f64) * 100.0;
    // Saturate at 999 so the field stays compact even past the limit.
    ratio.clamp(0.0, 999.0).round() as u64
}

const CONTEXT_BUDGET_HINT_PCT: u64 = 85;
/// Percent of `context_compaction_threshold` at which we surface the
/// compaction nudge. The threshold is itself a fraction of the full
/// context window (default 80% of the model max), so firing the nudge
/// at 70% of the threshold gives users a runway to `/pin` or `/compact`
/// deliberately before auto-compaction kicks in at 100%.
pub(crate) const CONTEXT_NUDGE_THRESHOLD_RATIO_PCT: u64 = 70;

fn format_status_hints(app: &TuiApp) -> String {
    if let Some(pending) = app.pending_request_user_input.as_ref() {
        if pending.request.choices.is_empty() && pending.request.allow_freeform {
            return "type your answer · Enter send · Esc cancel".to_string();
        }
        if pending.request.allow_freeform {
            return "Up/Down choose · type for free-form · Enter send · Esc cancel".to_string();
        }
        return "Up/Down choose · Enter select · Esc cancel".to_string();
    }
    if app.pending_mcp_elicitation.is_some() {
        if app
            .pending_mcp_elicitation
            .as_ref()
            .is_some_and(|pending| pending.request.kind == McpElicitationKind::Form)
        {
            return "type JSON object · Enter accept · N decline · Esc cancel".to_string();
        }
        return "Enter accept · N decline · Esc cancel".to_string();
    } else if app.pending_approval.is_some() {
        return "Up/Down choose · Enter select · Y approve · A always approve repo · N deny · Esc cancel"
            .to_string();
    } else if app.cancel.is_some() {
        return "Ctrl-C/Esc interrupt · Ctrl+J newline · Ctrl-P task · Ctrl-E expand · Ctrl-T transcript · Ctrl-Y copy · /help"
            .to_string();
    } else if app.exit_confirm_armed {
        return "Ctrl+C or Y to exit · any other key to cancel".to_string();
    }
    if app.cancelled_prompt.is_some() && app.turn_rx.is_none() && app.input.is_empty() {
        // We're idle right after a cancelled/failed turn — surface the
        // recovery affordance before the regular hint set.
        return "Ctrl-R restore last prompt · Enter send · Ctrl+J newline · /help".to_string();
    }
    let mut base = if app.alternate_scroll_enabled {
        "Enter send · !cmd shell · Wheel/PgUp/PgDn scroll · Up/Down menu · Alt+Up/Down history · Ctrl+J newline · Ctrl-E expand · Ctrl-T transcript · /help"
            .to_string()
    } else {
        "Enter send · !cmd shell · Up/Down menu/history · Ctrl+J newline · Ctrl-E expand · Ctrl-T transcript · /help"
            .to_string()
    };
    if app.context_compaction_threshold > 0
        && context_window_pct(
            app.context_estimate.estimated_tokens,
            app.context_compaction_threshold,
        ) >= CONTEXT_BUDGET_HINT_PCT
    {
        base.push_str(" · /pin to keep important context · /compact to summarize");
    }
    base
}

pub(crate) fn format_mcp_status(app: &TuiApp) -> String {
    app.mcp_status
        .as_ref()
        .map(format_mcp_status_snapshot)
        .unwrap_or_else(|| "none".to_string())
}

pub(crate) fn format_mcp_status_snapshot(snapshot: &McpStatusSnapshot) -> String {
    let total = snapshot.per_server.len();
    if total == 0 {
        return "none".to_string();
    }
    let mut ready = 0usize;
    let mut cached = 0usize;
    let mut failed = 0usize;
    let mut cancelled = 0usize;
    let mut tools = 0usize;
    for status in snapshot.per_server.values() {
        match status {
            McpServerStatus::Ready {
                tools_count,
                cached: is_cached,
            } => {
                ready += 1;
                tools += *tools_count;
                if *is_cached {
                    cached += 1;
                }
            }
            McpServerStatus::Failed { .. } => failed += 1,
            McpServerStatus::Cancelled => cancelled += 1,
            McpServerStatus::Starting => {}
        }
    }
    let mut parts = vec![format!("{ready}/{total} ready"), format!("{tools} tools")];
    if cached > 0 {
        parts.push(format!("{cached} cached"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if cancelled > 0 {
        parts.push(format!("{cancelled} cancelled"));
    }
    parts.join(" ")
}

pub(crate) fn reasoning_status_fragment(app: &TuiApp) -> String {
    if !app.show_reasoning_usage {
        return String::new();
    }
    app.cost
        .reasoning_output_tokens
        .map(|tokens| format!(" reasoning={tokens}"))
        .unwrap_or_default()
}

pub(crate) fn format_error_status(error: &SqueezyError) -> String {
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
    /// Open pull request number for the current branch, when `gh` reports
    /// one. Populated at startup via [`probe_pull_request`]; `None` when
    /// `gh` is missing, no PR exists, or the probe fails.
    pull_request: Option<u64>,
    /// `(added, removed)` line counts of the current branch relative to
    /// the repository's default branch. Populated via `git diff --shortstat`.
    branch_changes: Option<(u32, u32)>,
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
        let branch = snapshot
            .vcs
            .branch
            .or_else(|| snapshot.vcs.head.map(|head| short_commit(&head)));
        let pull_request = branch
            .as_deref()
            .and_then(|b| probe_pull_request(&config.workspace_root, b));
        let branch_changes = probe_branch_changes(&config.workspace_root);
        Self {
            branch,
            changed_files: snapshot.summary.files_changed,
            operation: snapshot.vcs.operation_state,
            available: true,
            pull_request,
            branch_changes,
        }
    }

    fn none() -> Self {
        Self {
            branch: None,
            changed_files: 0,
            operation: None,
            available: false,
            pull_request: None,
            branch_changes: None,
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

/// Look up the open pull-request number for `branch` via the `gh` CLI.
/// Returns `None` if `gh` is missing, unauthenticated, or no open PR exists.
/// Runs at startup; users see the result update across TUI restarts.
fn probe_pull_request(workspace_root: &std::path::Path, branch: &str) -> Option<u64> {
    use std::process::Command;
    let output = Command::new("gh")
        .arg("pr")
        .arg("view")
        .arg(branch)
        .arg("--json")
        .arg("number")
        .arg("-q")
        .arg(".number")
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.trim().parse::<u64>().ok()
}

/// Parse `git diff --shortstat <default>...HEAD` into `(added, removed)`.
/// Returns `None` if not in a git repo, the default branch can't be
/// determined, or git fails for any reason.
fn probe_branch_changes(workspace_root: &std::path::Path) -> Option<(u32, u32)> {
    use std::process::Command;
    let default_branch = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(workspace_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let output = Command::new("git")
        .args(["diff", "--shortstat"])
        .arg(format!("{default_branch}...HEAD"))
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_shortstat(&stdout)
}

/// Parse a `git diff --shortstat` line like
/// ` 3 files changed, 47 insertions(+), 9 deletions(-)`.
fn parse_shortstat(text: &str) -> Option<(u32, u32)> {
    let mut added = 0u32;
    let mut removed = 0u32;
    for chunk in text.split(',') {
        let trimmed = chunk.trim();
        if let Some(rest) = trimmed.strip_suffix(" insertions(+)") {
            added = rest.parse().unwrap_or(0);
        } else if let Some(rest) = trimmed.strip_suffix(" insertion(+)") {
            added = rest.parse().unwrap_or(0);
        } else if let Some(rest) = trimmed.strip_suffix(" deletions(-)") {
            removed = rest.parse().unwrap_or(0);
        } else if let Some(rest) = trimmed.strip_suffix(" deletion(-)") {
            removed = rest.parse().unwrap_or(0);
        }
    }
    if added == 0 && removed == 0 && !text.contains("changed") {
        None
    } else {
        Some((added, removed))
    }
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

pub(crate) struct TuiApp {
    pub(crate) provider_name: &'static str,
    pub(crate) version: &'static str,
    pub(crate) model: String,
    pub(crate) directory: String,
    pub(crate) language_summary: String,
    pub(crate) mode: SessionMode,
    pub(crate) config_sources: String,
    pub(crate) status_verbosity: StatusVerbosity,
    pub(crate) response_verbosity: ResponseVerbosity,
    pub(crate) tool_output_verbosity: ToolOutputVerbosity,
    pub(crate) transcript_default: TranscriptDefault,
    pub(crate) show_reasoning_usage: bool,
    pub(crate) repo: RepoStatus,
    pub(crate) permissions: PermissionStatus,
    pub(crate) telemetry: TelemetryStatus,
    pub(crate) input: String,
    pub(crate) input_cursor: usize,
    pub(crate) input_history: Vec<String>,
    pub(crate) input_history_index: Option<usize>,
    pub(crate) input_history_draft: String,
    pub(crate) slash_menu_index: usize,
    pub(crate) mention_popup: Option<mention::MentionPopup>,
    pub(crate) workspace_file_cache: Option<mention::WorkspaceFileCache>,
    pub(crate) overlay: Option<overlay::Overlay>,
    /// Full-screen transcript overlay (Ctrl+T) that renders every entry
    /// in its uncapped form. `None` = closed; `Some(state)` = open with
    /// a scroll offset. Mirrors codex's `open_transcript_overlay` as the
    /// escape hatch from the new aggressive default truncation.
    pub(crate) transcript_overlay: Option<TranscriptOverlayState>,
    pub(crate) alternate_scroll_enabled: bool,
    pub(crate) attachments: Vec<ContextAttachment>,
    pub(crate) context_compaction: ContextCompactionState,
    /// Token threshold above which auto-compaction triggers. Captured at
    /// startup from `config.context_compaction.estimated_tokens` so the
    /// status line can express usage as a percentage of the local cap
    /// (Squeezy's cost thesis cares about the configured budget, not the
    /// raw model window).
    pub(crate) context_compaction_threshold: u64,
    /// Whether the "compaction imminent" advisory has been pushed for the
    /// current pre-compaction window. Reset when compaction lands so the
    /// nudge can fire again on the next approach to the threshold.
    pub(crate) context_compaction_nudge_shown: bool,
    pub(crate) context_estimate: ContextEstimate,
    pub(crate) transcript: Vec<TranscriptEntry>,
    pub(crate) selected_entry: Option<usize>,
    pub(crate) next_entry_id: u64,
    pub(crate) transcript_scroll_from_bottom: u16,
    pub(crate) pending_assistant: streaming::StreamingController,
    /// Streaming buffer for reasoning/thinking deltas emitted during the
    /// current turn. Rendered as a grey transient block above the
    /// assistant text; cleared at turn completion.
    pub(crate) pending_reasoning: String,
    pub(crate) proposed_plan: proposed_plan::ProposedPlanExtractor,
    pub(crate) workspace_root: PathBuf,
    /// Session id assigned by the agent. Plan-mode IO (persist, prune,
    /// resolve-active) is scoped under
    /// `<workspace_root>/.squeezy/plans/<session_id>/` so concurrent
    /// sessions cannot see each other's plans. `None` until the agent
    /// hands one back; in that window plan IO falls back to
    /// [`proposed_plan::FALLBACK_SESSION_ID`].
    pub(crate) session_id: Option<String>,
    /// Plan id of the most recent `<proposed_plan>` block persisted under
    /// `.squeezy/plans/`. Used by Build-mode handoff and refinement turns
    /// to identify which plan file is active without scanning the dir.
    pub(crate) current_plan_id: Option<String>,
    /// Path to a plan file that should be re-attached to upcoming Build-mode
    /// turns. Set when the user switches Plan→Build while a plan is active.
    /// Cleared on Build→Plan switch, on plan discard, and on the first
    /// successful apply_patch / write_file in Build mode (the plan is "in
    /// motion" — re-attaching it from there is just noise).
    pub(crate) pending_plan_handoff: Option<PathBuf>,
    /// Number of Build-mode turns since the current `pending_plan_handoff`
    /// was queued. Turn 0 receives the full plan body as a prefix; turns
    /// 1+ receive a lighter `[plan still in effect — <path>]` marker so
    /// the model is reminded the plan applies without re-paying the
    /// body's tokens on every turn (issue 16).
    pub(crate) plan_handoff_turns_seen: u32,
    /// Interactive Execute/Refine/Discard/View prompt rendered right after a
    /// `<proposed_plan>` block lands. Set once on persist; cleared by an
    /// explicit user choice. Blocks other input while present.
    pub(crate) pending_plan_choice: Option<PendingPlanChoice>,
    /// Pause/resume state for Build-mode plan execution (PR-G item 6).
    /// Set when the user presses Shift+Tab while a Build turn is in
    /// flight and an active plan exists: the turn is cancelled, the
    /// captured plan id rides through Plan-mode, and the next Plan→
    /// Build crossing surfaces a resume marker telling the model
    /// whether the plan was refined while paused.
    pub(crate) plan_pause: Option<PlanPauseState>,
    /// One-shot resume marker queued during a Plan→Build crossing that
    /// resumes from a pause. Consumed by [`take_pending_plan_prefix`]
    /// on the first Build turn after the crossing so the marker rides
    /// alongside the plan body.
    pub(crate) plan_resume_marker: Option<String>,
    pub(crate) task_state: Option<TaskStateSnapshot>,
    pub(crate) mcp_status: Option<McpStatusSnapshot>,
    pub(crate) task_panel_collapsed: bool,
    pub(crate) active_tool: Option<String>,
    pub(crate) status: String,
    pub(crate) turn_visual: TurnVisualState,
    pub(crate) turn_started_at: Option<Instant>,
    pub(crate) last_turn_duration: Option<Duration>,
    /// Set when a terminal resize event arrives so the next draw can wipe
    /// the inline viewport before ratatui's autoresize scrolls stale frame
    /// content up into the scrollback above the new viewport.
    pub(crate) pending_resize: bool,
    pub(crate) terminal_title_state: TerminalTitleState,
    /// Last OSC title we wrote, so that repeated identical writes are
    /// suppressed and emitter logic stays idempotent across redraws.
    pub(crate) last_terminal_title: Option<String>,
    pub(crate) animation_tick: u64,
    pub(crate) animation_tick_rate: Duration,
    /// Set by any state mutator that requires the next frame to repaint.
    /// Cleared by the main loop immediately after `draw_app`. Without
    /// this gate the loop redraws every ~50 ms and idle terminals show
    /// continuous activity in their per-tab indicators.
    pub(crate) needs_redraw: bool,
    pub(crate) exit_confirm_armed: bool,
    pub(crate) active_tool_calls: BTreeMap<String, ToolCall>,
    /// Elapsed time on the currently-running tool, sourced from the
    /// 1Hz `AgentEvent::ToolProgress` heartbeat. Cleared when the tool
    /// finishes or the turn ends.
    pub(crate) active_tool_elapsed_ms: Option<u64>,
    /// Latest mid-turn cost snapshot, surfaced in the status bar instead
    /// of being appended to the transcript log on every stride.
    pub(crate) turn_progress: Option<TurnProgress>,
    /// File paths whose most recent edit failed within the current turn,
    /// mapped to the transcript entry id of that failure. When a later
    /// edit on the same file succeeds, the failure row is removed so a
    /// noisy `unified-diff fallback could not apply cleanly` that the
    /// agent immediately recovered from doesn't end up in scrollback.
    pub(crate) recent_edit_failures: HashMap<PathBuf, u64>,
    pub(crate) cost: squeezy_core::CostSnapshot,
    /// Session-level cap in USD micros, sourced from
    /// `AppConfig.max_session_cost_usd_micros`. `None` (or a zero cap)
    /// means the status bar renders the legacy `cost $X` segment
    /// unchanged.
    pub(crate) cost_cap_usd_micros: Option<u64>,
    pub(crate) metrics: squeezy_core::TurnMetrics,
    pub(crate) turn_rx: Option<mpsc::Receiver<AgentEvent>>,
    pub(crate) job_rx: Option<broadcast::Receiver<JobEvent>>,
    pub(crate) jobs: BTreeMap<JobId, JobSnapshot>,
    pub(crate) notifications: VecDeque<JobNotification>,
    pub(crate) cancel: Option<CancellationToken>,
    pub(crate) pending_approval: Option<PendingApproval>,
    pub(crate) approval_selection_index: usize,
    pub(crate) pending_mcp_elicitation: Option<PendingMcpElicitation>,
    pub(crate) pending_request_user_input: Option<PendingRequestUserInput>,
    /// Prompt that was in flight when the most recent turn was cancelled
    /// or failed. Surfaced via Ctrl-R so the user can recover from a
    /// typo without retyping. Cleared on successful completion.
    pub(crate) cancelled_prompt: Option<String>,
    /// True when the in-flight turn has already produced a successful
    /// edit-capable tool call (apply_patch / write_file). Used to surface
    /// `/diff` and `/undo` hints at end-of-turn (success or failure).
    pub(crate) last_turn_had_edits: bool,
    pub(crate) mcp_elicitation_selection_index: usize,
    pub(crate) pending_feedback: Option<PreparedFeedback>,
    pub(crate) pending_report: Option<BugReportBundle>,
    pub(crate) clipboard: Box<dyn Clipboard>,
    pub(crate) app_notifications: NotificationQueue,
    /// Corner toast stack — short-lived overlays for fire-and-forget
    /// status events (telemetry flush, MCP connect, index ready). Kept
    /// separate from `app_notifications` because that surface is an
    /// inline rotating banner; toasts overlay the top-right and stack up
    /// to three at a time.
    pub(crate) toasts: ToastQueue,
    /// Opt-in OSC 9 / BEL emitter for off-tab attention. Disabled by
    /// default; flips on via `[tui].desktop_notifications`.
    pub(crate) desktop_notifier: DesktopNotifier,
    pub(crate) config_screen: Option<config_screen::ConfigScreenState>,
    /// Configured status-bar items (`[tui].status_line`). `None` means
    /// "use the built-in default list"; `Some(empty)` means the user
    /// disabled the detail line entirely.
    pub(crate) status_line_items: Option<Vec<status::StatusLineItem>>,
    /// Whether status-bar items render with accent colors. Defaults to
    /// `true`.
    pub(crate) status_line_use_colors: bool,
    /// Interactive `/statusline` overlay. `None` = closed.
    pub(crate) status_line_setup: Option<status_line_setup::StatusLineSetupState>,
    /// Latest mid-turn task-progress text surfaced by the agent.
    /// Drives the `task-progress` status item.
    pub(crate) latest_plan_progress: Option<String>,
    /// Resolved key bindings (defaults + user overrides from
    /// `[tui.keymap]`). Built once at startup; `/keymap` reads from
    /// here and `handle_key` consults it before dispatching to the
    /// legacy hardcoded handlers.
    pub(crate) keymap: keymap::KeymapResolver,
}

impl TuiApp {
    /// Session id to use for plan IO. Falls back to
    /// [`proposed_plan::FALLBACK_SESSION_ID`] when the agent has not yet
    /// handed one back so plan-mode IO can still proceed during the
    /// pre-first-turn window.
    pub(crate) fn plan_session_id(&self) -> &str {
        self.session_id
            .as_deref()
            .unwrap_or(proposed_plan::FALLBACK_SESSION_ID)
    }

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
                skip_resume_picker: false,
                update_banner: None,
                resume_session_id: None,
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
            repo: RepoStatus::detect(config),
            permissions: PermissionStatus::from_policy(&config.permissions),
            telemetry: TelemetryStatus::from_config(&config.telemetry),
            input: String::new(),
            input_cursor: 0,
            input_history: Vec::new(),
            input_history_index: None,
            input_history_draft: String::new(),
            slash_menu_index: 0,
            mention_popup: None,
            workspace_file_cache: None,
            overlay: None,
            transcript_overlay: None,
            alternate_scroll_enabled: TerminalMode::from(config.tui.alternate_screen)
                == TerminalMode::AlternateScreen,
            attachments: Vec::new(),
            context_compaction: ContextCompactionState::default(),
            context_compaction_threshold: config.context_compaction.estimated_tokens,
            context_compaction_nudge_shown: false,
            context_estimate: ContextEstimate::default(),
            transcript,
            selected_entry: None,
            next_entry_id,
            transcript_scroll_from_bottom: 0,
            pending_assistant: streaming::StreamingController::new(),
            pending_reasoning: String::new(),
            proposed_plan: proposed_plan::ProposedPlanExtractor::new(),
            workspace_root: config.workspace_root.clone(),
            session_id: None,
            current_plan_id: None,
            pending_plan_handoff: None,
            plan_handoff_turns_seen: 0,
            pending_plan_choice: None,
            plan_pause: None,
            plan_resume_marker: None,
            task_state: None,
            mcp_status: None,
            task_panel_collapsed: false,
            active_tool: None,
            status,
            turn_visual: TurnVisualState::Idle,
            turn_started_at: None,
            last_turn_duration: None,
            pending_resize: false,
            terminal_title_state: TerminalTitleState::Cleared,
            last_terminal_title: None,
            animation_tick: 0,
            animation_tick_rate: config.tick_rate,
            // Start dirty so the first iteration of the main loop paints
            // the initial frame.
            needs_redraw: true,
            exit_confirm_armed: false,
            active_tool_calls: BTreeMap::new(),
            active_tool_elapsed_ms: None,
            turn_progress: None,
            recent_edit_failures: HashMap::new(),
            cost: squeezy_core::CostSnapshot::default(),
            cost_cap_usd_micros: config.max_session_cost_usd_micros.filter(|cap| *cap > 0),
            metrics: squeezy_core::TurnMetrics::default(),
            turn_rx: None,
            job_rx: None,
            jobs: BTreeMap::new(),
            notifications: VecDeque::new(),
            cancel: None,
            pending_approval: None,
            approval_selection_index: 0,
            pending_mcp_elicitation: None,
            pending_request_user_input: None,
            cancelled_prompt: None,
            last_turn_had_edits: false,
            mcp_elicitation_selection_index: 0,
            pending_feedback: None,
            pending_report: None,
            clipboard,
            app_notifications: NotificationQueue::new(),
            toasts: ToastQueue::new(),
            desktop_notifier: DesktopNotifier::new(config.tui.desktop_notifications),
            config_screen: None,
            status_line_items: parse_status_line_items(config.tui.status_line.as_deref()),
            status_line_use_colors: config.tui.status_line_use_colors,
            status_line_setup: None,
            latest_plan_progress: None,
            keymap: keymap::KeymapResolver::from_overrides(&config.tui.keymap),
        }
    }

    pub(crate) fn note_turn_started(&mut self) {
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(Instant::now());
        }
        self.last_turn_duration = None;
        self.terminal_title_state = TerminalTitleState::Working;
        self.turn_progress = None;
        self.active_tool_elapsed_ms = None;
        self.recent_edit_failures.clear();
        self.needs_redraw = true;
    }

    /// Record the latest mid-turn cost/token snapshot for the status
    /// bar. Repeated identical resends (the broker fires on a tool-count
    /// stride, not a token delta) are dropped so the status bar doesn't
    /// flicker.
    pub(crate) fn update_turn_progress(
        &mut self,
        tool_count: u64,
        input_tokens: u64,
        micro_usd: u64,
    ) {
        let snapshot = TurnProgress {
            tool_count,
            input_tokens,
            micro_usd,
        };
        if self.turn_progress == Some(snapshot) {
            return;
        }
        self.turn_progress = Some(snapshot);
    }

    /// Update the active-tool elapsed clock. The tool name itself is
    /// already tracked by [`Self::remember_active_tool_call`]; we only
    /// need the elapsed-ms refresh from the 1Hz heartbeat.
    pub(crate) fn note_active_tool_progress(&mut self, _tool_name: &str, elapsed_ms: u64) {
        self.active_tool_elapsed_ms = Some(elapsed_ms);
    }

    pub(crate) fn note_turn_finished(&mut self) {
        if let Some(started_at) = self.turn_started_at.take() {
            self.last_turn_duration = Some(started_at.elapsed());
        }
        self.terminal_title_state = TerminalTitleState::Notification;
        // Status-bar progress snapshots only describe a live turn. The
        // end-of-turn footer prints final totals separately.
        self.turn_progress = None;
        self.active_tool_elapsed_ms = None;
        // Best-effort off-tab attention surface; the in-terminal toast and
        // title glyph already cover the on-screen case. We ignore any IO
        // error here because failing to notify is strictly less important
        // than continuing the turn-finish bookkeeping.
        let _ = self.desktop_notifier.notify("squeezy turn complete");
        self.needs_redraw = true;
    }

    /// Fire the desktop-notification surface for an approval-pending event.
    /// Public to `events.rs` so the approval-request handler can call it
    /// without poking the field directly.
    pub(crate) fn notify_approval_pending(&self, tool_name: &str) {
        let message = format!("squeezy needs approval for {tool_name}");
        let _ = self.desktop_notifier.notify(&message);
    }

    /// Whether the next frame would visibly differ from the current one
    /// purely because some animation is in motion. Decoupled from
    /// `needs_redraw`: state mutations set the dirty flag; this predicate
    /// catches the case where nothing has mutated but the on-screen
    /// content is still moving (working spinner, title spinner, etc.).
    /// When neither is true the main loop skips `draw_app` entirely.
    pub(crate) fn has_active_animation(&self) -> bool {
        matches!(self.turn_visual, TurnVisualState::Running)
            || self.terminal_title_state == TerminalTitleState::Working
    }

    pub(crate) fn push_transcript_item(&mut self, item: TranscriptItem) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::message(id, item, self.transcript_default));
    }

    #[cfg(test)]
    fn push_tool_result(&mut self, result: ToolResult) {
        self.push_tool_result_with_call(result, None);
    }

    pub(crate) fn push_tool_result_with_call(
        &mut self,
        result: ToolResult,
        call: Option<ToolCall>,
    ) {
        if tool_result_hidden_by_default(&result) {
            return;
        }
        // Edit-family results that fail and then succeed on the same path
        // within a turn are usually the apply_patch fallback dance, not a
        // user-actionable error. Drop the prior failure row when the new
        // row is a success; record the new failure row when it fails.
        let edit_paths = edit_target_paths(&result, call.as_ref());
        if !edit_paths.is_empty() {
            match result.status {
                ToolStatus::Success => {
                    let removed: Vec<u64> = edit_paths
                        .iter()
                        .filter_map(|path| self.recent_edit_failures.remove(path))
                        .collect();
                    if !removed.is_empty() {
                        self.transcript.retain(|entry| !removed.contains(&entry.id));
                    }
                }
                ToolStatus::Error => {
                    // Keep the entry id; we don't know it yet, so record
                    // after pushing.
                }
                _ => {}
            }
        }
        let id = self.next_id();
        let entry = TranscriptEntry::tool_result(id, result, call, self.transcript_default);
        if let Some(last) = self.transcript.last_mut()
            && coalesce_tool_transcript_entry(last, &entry)
        {
            return;
        }
        let is_edit_failure = matches!(
            entry.kind,
            TranscriptEntryKind::ToolResult(ref t)
                if matches!(t.result.tool_name.as_str(), "apply_patch" | "write_file")
                    && t.result.status == ToolStatus::Error
        );
        self.push_entry(entry);
        if is_edit_failure {
            for path in edit_paths {
                self.recent_edit_failures.insert(path, id);
            }
        }
    }

    pub(crate) fn push_log(&mut self, message: String) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::log(id, message, self.transcript_default));
    }

    pub(crate) fn push_plan_card(&mut self, data: render::plan_card::PlanCardData) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::plan_card(
            id,
            data,
            self.transcript_default,
        ));
    }

    pub(crate) fn push_diff_card(&mut self, data: DiffCardData) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::diff_card(id, data));
    }

    pub(crate) fn push_reasoning_segment(&mut self, snapshot: squeezy_core::ReasoningSnapshot) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::reasoning(
            id,
            snapshot,
            self.transcript_default,
        ));
    }

    fn push_entry(&mut self, entry: TranscriptEntry) {
        self.transcript.push(entry);
    }

    pub(crate) fn remember_active_tool_call(&mut self, call: ToolCall) {
        if is_control_tool_name(&call.name) {
            return;
        }
        self.active_tool = Some(call.name.clone());
        self.active_tool_calls.insert(call.call_id.clone(), call);
    }

    pub(crate) fn refresh_active_tool_name(&mut self) {
        self.active_tool = self
            .active_tool_calls
            .values()
            .find(|call| !is_control_tool_name(&call.name))
            .map(|call| call.name.clone());
    }

    pub(crate) fn clear_active_tools(&mut self) {
        self.active_tool = None;
        self.active_tool_calls.clear();
        self.active_tool_elapsed_ms = None;
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
        _transcript_default: TranscriptDefault,
    ) -> Self {
        // Tool results are now uniformly collapsed-by-default: the new
        // codex-style head-tail preview caps each card at ~5 lines (50
        // for direct `!`-shell). `TranscriptDefault::Normal|Compact`
        // still gates messages and logs but no longer the tool card,
        // because the cap itself is the whole point of the preview.
        Self {
            id,
            kind: TranscriptEntryKind::ToolResult(Box::new(ToolTranscript {
                call,
                result,
                repeat_count: 1,
            })),
            collapsed: true,
        }
    }

    fn log(id: u64, message: String, transcript_default: TranscriptDefault) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::Log(message),
            collapsed: transcript_default == TranscriptDefault::Compact,
        }
    }

    fn plan_card(
        id: u64,
        data: render::plan_card::PlanCardData,
        _transcript_default: TranscriptDefault,
    ) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::PlanCard(Box::new(data)),
            // Plan cards default to fully expanded even in Compact mode
            // — the whole point of the card is to show the plan body.
            collapsed: false,
        }
    }

    fn diff_card(id: u64, data: DiffCardData) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::Diff(Box::new(data)),
            // Mirror codex's `/diff`: never truncated by default. The
            // user can still Ctrl-E to fold the body if it's huge.
            collapsed: false,
        }
    }

    fn reasoning(
        id: u64,
        snapshot: squeezy_core::ReasoningSnapshot,
        transcript_default: TranscriptDefault,
    ) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::Reasoning(Box::new(snapshot)),
            // Default expanded so reasoning the user just watched stream in
            // stays visible after the segment lands; auto-collapsing the
            // full body to a one-line summary makes the text look like it
            // vanished. Users on `transcript_default = "compact"` keep the
            // collapsed shape, and Ctrl-E still toggles it either way.
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
                TranscriptEntryKind::PlanCard(_) => true,
                _ => false,
            },
            TranscriptCategory::Diffs => match &self.kind {
                TranscriptEntryKind::ToolResult(tool) => tool.result.tool_name.contains("diff"),
                TranscriptEntryKind::Diff(_) => true,
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
            TranscriptEntryKind::PlanCard(data) => {
                let body = proposed_plan::read_plan_body(&data.path).unwrap_or_default();
                vec![format!("plan {}\n{body}", data.plan_id)]
            }
            TranscriptEntryKind::Diff(data) => {
                vec![format!("diff ({})\n{}", data.summary, data.plain)]
            }
            TranscriptEntryKind::Reasoning(snapshot) => {
                vec![format!("reasoning: {}", snapshot.display_text)]
            }
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
        match &self.kind {
            TranscriptEntryKind::Message(item) => {
                item.role != Role::User && text_has_collapsible_content(&item.content)
            }
            TranscriptEntryKind::ToolResult(_) => true,
            TranscriptEntryKind::Log(message) => text_has_collapsible_content(message),
            TranscriptEntryKind::PlanCard(_) => true,
            TranscriptEntryKind::Diff(_) => true,
            TranscriptEntryKind::Reasoning(_) => true,
        }
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
            TranscriptEntryKind::PlanCard(data) => (
                format!("plan {}", data.plan_id),
                proposed_plan::read_plan_body(&data.path).unwrap_or_default(),
                format!("transcript:{}", self.id),
            ),
            TranscriptEntryKind::Diff(data) => (
                format!("diff ({})", data.summary),
                data.plain.clone(),
                format!("transcript:{}", self.id),
            ),
            TranscriptEntryKind::Reasoning(snapshot) => (
                "reasoning".to_string(),
                snapshot.display_text.clone(),
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
    /// Plan-mode v3 (PR-F): a styled card pointing at the persisted
    /// plan file. The body is loaded from disk at render time so the
    /// cell tracks in-place refinements and survives compaction.
    PlanCard(Box<render::plan_card::PlanCardData>),
    /// Output of the `/diff` slash command — a snapshot of uncommitted
    /// changes (tracked + untracked), captured at invocation time and
    /// rendered as pre-styled lines using `render::diff`. Stored
    /// pre-rendered so per-frame work is constant.
    Diff(Box<DiffCardData>),
    /// A finalized reasoning segment from the model. Stored separately
    /// so each reasoning block becomes its own grey collapsible entry
    /// instead of being pinned to the next assistant message.
    Reasoning(Box<squeezy_core::ReasoningSnapshot>),
}

/// Frozen snapshot of `/diff` output. Lines are pre-rendered with the
/// existing diff styling so re-rendering a frame is constant-time.
#[derive(Debug, Clone)]
pub(crate) struct DiffCardData {
    pub(crate) summary: String,
    pub(crate) plain: String,
    pub(crate) lines: Vec<Line<'static>>,
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
    // Include `path` so two apply_patch failures on different files with the
    // same boilerplate error (e.g. "search text not found") don't coalesce
    // into a single transcript entry.
    let path = tool
        .result
        .content
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    Some(format!(
        "{}:{}:{}",
        tool.result.tool_name,
        path,
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

/// Mid-turn cost/token snapshot surfaced in the status bar so the user
/// can watch a turn's spend grow without log spam in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnProgress {
    pub(crate) tool_count: u64,
    pub(crate) input_tokens: u64,
    pub(crate) micro_usd: u64,
}

pub(crate) struct PendingApproval {
    pub(crate) request: ToolApprovalRequest,
    pub(crate) decision_tx: oneshot::Sender<ToolApprovalDecision>,
}

pub(crate) struct PendingMcpElicitation {
    pub(crate) request: McpElicitationRequest,
    pub(crate) response_tx: oneshot::Sender<McpElicitationResponse>,
}

pub(crate) struct PendingRequestUserInput {
    pub(crate) request: RequestUserInputRequest,
    pub(crate) response_tx: oneshot::Sender<RequestUserInputResponse>,
    pub(crate) selection_index: usize,
}

/// Interactive prompt that appears after a `<proposed_plan>` block lands
/// and persists. Lets the user execute, refine, discard, or view the
/// plan file without typing a slash command.
#[derive(Debug, Clone)]
pub(crate) struct PendingPlanChoice {
    pub(crate) plan_id: String,
    pub(crate) plan_path: PathBuf,
    pub(crate) selection_index: usize,
}

/// Captured plan-execution state at the moment of a Shift+Tab pause
/// (PR-G item 6). `plan_id` is compared against `current_plan_id` on
/// the next Plan→Build crossing so the resume marker can tell the
/// model whether the plan body was refined while paused.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanPauseState {
    plan_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlanChoiceAction {
    Execute,
    ExecuteClean,
    Refine,
    Discard,
    View,
}

struct PlanChoiceOption {
    action: PlanChoiceAction,
    label: &'static str,
    hint: &'static str,
    shortcut: char,
}

const PLAN_CHOICES: &[PlanChoiceOption] = &[
    PlanChoiceOption {
        action: PlanChoiceAction::Execute,
        label: "Execute",
        hint: "switch to Build; keep history; run the plan",
        shortcut: 'e',
    },
    PlanChoiceOption {
        action: PlanChoiceAction::ExecuteClean,
        label: "Execute (clean)",
        hint: "compact prior chat to a summary, then run the plan",
        shortcut: 'c',
    },
    PlanChoiceOption {
        action: PlanChoiceAction::Refine,
        label: "Refine",
        hint: "stay in Plan; describe what to change",
        shortcut: 'r',
    },
    PlanChoiceOption {
        action: PlanChoiceAction::Discard,
        label: "Discard",
        hint: "delete the plan file and dismiss this prompt",
        shortcut: 'd',
    },
    PlanChoiceOption {
        action: PlanChoiceAction::View,
        label: "View",
        hint: "log the plan file path so you can open it externally",
        shortcut: 'v',
    },
];

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
        let _ = execute!(
            stdout,
            DisableModifyOtherKeys,
            PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
        );
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

    fn draw_app(&mut self, app: &mut TuiApp) -> Result<()> {
        if app.pending_resize {
            app.pending_resize = false;
            self.wipe_inline_viewport_for_resize()?;
        }
        self.apply_terminal_title(app)?;
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

    fn apply_terminal_title(&mut self, app: &mut TuiApp) -> Result<()> {
        let elapsed_ms = prompt_elapsed_ms(app);
        let desired = terminal_title_for(app.terminal_title_state, &app.directory, elapsed_ms);
        if desired == app.last_terminal_title {
            return Ok(());
        }
        let backend = self.terminal.backend_mut();
        match &desired {
            Some(title) => write!(backend, "\x1b]0;{title}\x07"),
            None => write!(backend, "\x1b]0;\x07"),
        }
        .and_then(|_| backend.flush())
        .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        app.last_terminal_title = desired;
        Ok(())
    }

    /// Crossterm's autoresize for inline viewports calls `append_lines` to
    /// shift the previous frame upward before redrawing — leaving the old
    /// frame's contents (e.g. the "Worked for Ns" divider) stranded as
    /// scrollback above the new viewport. Wiping from the current viewport
    /// top down before the next draw means there is nothing stale for the
    /// scroll to pull up. Alternate-screen mode handles resize cleanly via
    /// the terminal itself, so the work is inline-only.
    fn wipe_inline_viewport_for_resize(&mut self) -> Result<()> {
        if self.mode != TerminalMode::Inline {
            return Ok(());
        }
        let viewport_top = self.terminal.get_frame().area().y;
        execute!(
            self.terminal.backend_mut(),
            MoveTo(0, viewport_top),
            Clear(ClearType::FromCursorDown)
        )
        .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        self.terminal
            .clear()
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
                    PopKeyboardEnhancementFlags,
                    Print(RESET_KEYBOARD_ENHANCEMENT_FLAGS),
                    DisableModifyOtherKeys,
                    DisableBracketedPaste,
                    DisableAlternateScroll,
                    Print(DISABLE_MOUSE_MODES),
                    Print("\x1b]0;\x07"),
                    Print(CLEAR_SCROLLBACK_AND_VISIBLE)
                );
            }
            TerminalMode::AlternateScreen => {
                let _ = execute!(
                    self.terminal.backend_mut(),
                    PopKeyboardEnhancementFlags,
                    Print(RESET_KEYBOARD_ENHANCEMENT_FLAGS),
                    DisableModifyOtherKeys,
                    DisableBracketedPaste,
                    DisableAlternateScroll,
                    Print(DISABLE_MOUSE_MODES),
                    Print("\x1b]0;\x07"),
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
            app.show_reasoning_usage,
        ));
    }
    lines
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
