use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    env, fmt,
    hash::{Hash, Hasher},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::{
    Command,
    cursor::MoveTo,
    event::{
        self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size as terminal_size,
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
use serde::Deserialize;
#[cfg(test)]
use squeezy_agent::RequestUserInputChoice;
use squeezy_agent::{
    Agent, AgentEvent, DispatchCommand, DispatchCommandParseError, JobEvent, JobId,
    JobNotification, JobSnapshot, MAX_JOB_NOTIFICATIONS, PendingConfigSwap,
    RequestUserInputRequest, RequestUserInputResponse, SubagentId, ToolApprovalDecision,
    ToolApprovalRequest,
};
use squeezy_core::{
    AppConfig, ConfigWarning, ContextAttachment, ContextAttachmentKind, ContextCompactionRecord,
    ContextCompactionState, ContextEstimate, DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES,
    PermissionCapability, PermissionPolicy, ResponseVerbosity, Result, Role, SessionMode,
    ShellDiffInline, SqueezyError, StatusVerbosity, TaskStateSnapshot, TelemetryConfig,
    ToolOutputVerbosity, TranscriptDefault, TranscriptItem, TuiAlternateScreen,
    TuiSynchronizedOutput, TurnMetrics, context_attachment_storage_text,
    detect_context_attachment_kind, detect_image_mime,
};
use squeezy_llm::{LlmInputItem, LlmProvider};
use squeezy_skills::PromptTemplateCatalog;
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
mod commands_style;
mod config_screen;
mod events;
mod fuzzy;
mod history;
mod input;
mod keymap;
mod keymap_config;
mod mention;
mod notification;
mod overlay;
mod prompt_history;
mod prompt_queue;
mod proposed_plan;
mod render;
mod resume_picker;
mod settings_watcher;
mod startup_model_picker;
mod status;
mod status_line_setup;
mod streaming;
mod streaming_patch;
mod terminal_writer;
mod toast;
pub use render::markdown::render_markdown;
pub use startup_model_picker::{
    StartupModelPickerModel, StartupModelPickerProvider, StartupModelPickerResult,
    StartupModelPickerSelection, StartupProviderCredential, StartupThemeAction, StartupThemeChoice,
};
pub use streaming_patch::{
    JsonPatchPreviewParser, PatchPartial, PatchPreviewEvent, render_streaming_preview,
};

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
pub(crate) use events::apply_mcp_status_update;
pub(crate) use events::{
    drain_agent_events, drain_job_events, drain_pending_diff, drain_pending_mention_walk,
    drain_plan_housekeeping, drain_repo_status,
};
#[cfg(test)]
pub(crate) use input::set_input;
pub(crate) use input::{HistoryDirection, SLASH_COMMANDS, SelectionDirection, SlashCommand};
use input::{
    clear_input, complete_selected_slash_command, delete_at_cursor, delete_before_cursor,
    delete_next_word, delete_previous_word, delete_to_line_end, delete_to_line_start,
    handle_mention_popup_key, handle_overlay_key, handle_request_user_input_key, input_cursor,
    insert_input_char, insert_input_text, move_input_cursor_down, move_input_cursor_left,
    move_input_cursor_line_end, move_input_cursor_line_start, move_input_cursor_right,
    move_input_cursor_up, move_input_cursor_word_left, move_input_cursor_word_right,
    move_slash_menu_selection, push_input_history, recall_prompt_history,
    reject_unknown_slash_command,
};

use notification::{DesktopNotifier, NotificationQueue, Severity as NotifySeverity};
use render::palette::{self, blend_color};
use terminal_writer::TerminalWriter;
use toast::ToastQueue;

const LARGE_PASTE_CHAR_THRESHOLD: usize = 1_000;
const LONG_ASSISTANT_CHARS: usize = 1_200;
const TOOL_PREVIEW_COMPACT_BYTES: usize = 300;
const TOOL_PREVIEW_NORMAL_BYTES: usize = 1_200;
const TOOL_PREVIEW_VERBOSE_BYTES: usize = 4_000;
/// Default tool-card cap for model-initiated tool calls. Aggressive on
/// purpose — the structured detail is one keystroke (Ctrl-E / Ctrl-T)
/// away, and a 5-line preview keeps the transcript readable even when
/// the model fires off long commands.
const TOOL_CALL_MAX_LINES: usize = 5;
/// Larger cap for `!`-shell calls the user typed directly (those carry
/// `direct_user_shell: true` in their arguments, set by
/// `local_shell_command_call` in the agent). 50 lines fits a typical
/// command's full output without truncation.
const USER_SHELL_TOOL_CALL_MAX_LINES: usize = 50;
const PROMPT_MIN_HEIGHT: u16 = 4;
const PROMPT_MAX_HEIGHT: u16 = 30;
const INLINE_VIEWPORT_HEIGHT: u16 = 18;
// The slash-command roster grew well past 30 entries, so a 5-row
// window forced users to scroll for almost any non-top-5 command.
// 10 fits comfortably in a standard 24-row terminal alongside the
// prompt + status row and matches the picker height used by /config
// search and the model picker.
const SLASH_MENU_MAX_ITEMS: usize = 10;

/// Process-wide override for `tui.shell_diff_inline`, pinned by the TuiApp
/// at startup and re-applied on settings hot-reload. Encoded as `0 = Full
/// (default)`, `1 = Folded`. A static lets the deeply-nested render path
/// consult the setting without threading it through every formatter, the
/// same pattern the palette uses for tone/accent overrides.
static SHELL_DIFF_INLINE_OVERRIDE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

fn shell_diff_inline_setting() -> ShellDiffInline {
    match SHELL_DIFF_INLINE_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => ShellDiffInline::Folded,
        _ => ShellDiffInline::Full,
    }
}

fn set_shell_diff_inline(setting: ShellDiffInline) {
    let encoded = match setting {
        ShellDiffInline::Full => 0,
        ShellDiffInline::Folded => 1,
    };
    SHELL_DIFF_INLINE_OVERRIDE.store(encoded, std::sync::atomic::Ordering::Relaxed);
}

/// True when this tool is `shell` (or `verify`, the structured-shell sibling)
/// and its stdout looks like unified-diff output. Used by the preview-cap
/// bypass and the BG-color renderer to treat `git diff` cards as first-class
/// diff content the way `apply_patch` already is.
fn shell_output_is_unified_diff(tool: &ToolTranscript) -> bool {
    if !matches!(tool.result.tool_name.as_str(), "shell" | "verify") {
        return false;
    }
    let stdout = tool.result.content["stdout"].as_str().unwrap_or("");
    shell_text_looks_like_diff(stdout)
}

const DISABLE_MOUSE_MODES: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";
/// Enable basic button-press/release reporting (1000) with SGR
/// coordinate encoding (1006). Required for the clickable queue
/// indicator strip to receive `MouseEventKind::Down(Left)` events.
/// Note: while this is enabled, native text selection in the terminal
/// requires holding `Shift` on most emulators — the standard tradeoff
/// when a TUI takes over mouse input.
const ENABLE_MOUSE_CLICK_CAPTURE: &str = "\x1b[?1000h\x1b[?1006h";
/// Enable button-motion reporting (1002) in addition to basic click
/// reporting. This is only used while the transcript overlay is in its
/// explicit scrollbar-drag mode; native unmodified text selection needs
/// mouse reporting disabled, so drag capture cannot be the overlay default.
const ENABLE_MOUSE_DRAG_CAPTURE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
// The matching disable sequence (1000l, 1006l) is already part of
// `DISABLE_MOUSE_MODES`, so the Drop tear-down covers undoing this
// without needing a dedicated constant.
const CLEAR_SCROLLBACK_AND_VISIBLE: &str = "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H";
const RESET_KEYBOARD_ENHANCEMENT_FLAGS: &str = "\x1b[<u";
/// DEC private mode 2026 — Begin Synchronized Update. Capable terminals
/// buffer subsequent output and flip the cell grid atomically when they
/// see the matching End Synchronized Update sequence. Terminals that do
/// not implement the mode silently ignore both sequences, so emitting
/// them is safe across the ecosystem.
const BEGIN_SYNCHRONIZED_UPDATE: &str = "\x1b[?2026h";
/// DEC private mode 2026 — End Synchronized Update. Pairs with
/// [`BEGIN_SYNCHRONIZED_UPDATE`]; capable terminals commit the buffered
/// frame atomically on receipt.
const END_SYNCHRONIZED_UPDATE: &str = "\x1b[?2026l";
const TITLE_SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TITLE_SPINNER_INTERVAL_MS: u64 = 100;
const TITLE_NOTIFICATION_GLYPH: &str = "●";
const MAX_INPUT_EVENTS_PER_POLL: usize = 128;
const MAX_TRANSCRIPT_DRAG_INPUT_EVENTS_PER_POLL: usize = 4096;

fn enter_transcript_overlay_screen<W: Write>(writer: &mut W) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        Print(DISABLE_MOUSE_MODES),
        EnableAlternateScroll,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )
}

fn leave_transcript_overlay_screen<W: Write>(
    writer: &mut W,
    restore_mouse_capture: bool,
) -> io::Result<()> {
    execute!(
        writer,
        DisableAlternateScroll,
        Print(DISABLE_MOUSE_MODES),
        LeaveAlternateScreen
    )?;
    if restore_mouse_capture {
        execute!(writer, Print(ENABLE_MOUSE_CLICK_CAPTURE))?;
    }
    Ok(())
}

fn set_transcript_overlay_mouse_mode<W: Write>(
    writer: &mut W,
    scrollbar_drag: bool,
    restore_main_mouse_capture: bool,
) -> io::Result<()> {
    execute!(writer, Print(DISABLE_MOUSE_MODES))?;
    if scrollbar_drag {
        execute!(writer, Print(ENABLE_MOUSE_DRAG_CAPTURE))?;
    } else if restore_main_mouse_capture {
        execute!(writer, Print(ENABLE_MOUSE_CLICK_CAPTURE))?;
    }
    Ok(())
}

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

/// Resolve the user's synchronized-output policy into a runtime flag.
/// `Auto` consults the environment for known capable terminals; the
/// sequences themselves are spec'd as silent no-ops on terminals that
/// do not understand them, so a false negative here only forfeits the
/// optimisation — it never corrupts output.
fn resolve_synchronized_output(policy: TuiSynchronizedOutput) -> bool {
    match policy {
        TuiSynchronizedOutput::Always => true,
        TuiSynchronizedOutput::Never => false,
        // Wrap `env::var_os` in a closure so the HRTB on the resolver
        // (`for<'a> Fn(&'a str) -> _`) is satisfied — passing the bare
        // monomorphised function pointer here trips lifetime inference.
        TuiSynchronizedOutput::Auto => {
            detect_synchronized_output_support_from_env(|key: &str| env::var_os(key))
        }
    }
}

/// Pure capability heuristic for DEC mode 2026 support based on
/// environment-variable signals exposed by the host terminal.
/// Factored out for testability — production calls thread
/// [`std::env::var_os`] in; tests pass a closure backed by a fixture
/// map so the resolver is exercised without mutating real process env.
fn detect_synchronized_output_support_from_env<F>(env_get: F) -> bool
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    if env_get("KITTY_WINDOW_ID").is_some()
        || env_get("WEZTERM_PANE").is_some()
        || env_get("WEZTERM_EXECUTABLE").is_some()
        || env_get("GHOSTTY_RESOURCES_DIR").is_some()
        || env_get("ALACRITTY_LOG").is_some()
        || env_get("ALACRITTY_WINDOW_ID").is_some()
        || env_get("ITERM_SESSION_ID").is_some()
    {
        return true;
    }
    if let Some(prog) = env_get("TERM_PROGRAM") {
        let prog = prog.to_string_lossy().to_ascii_lowercase();
        if matches!(
            prog.as_str(),
            "iterm.app" | "iterm2" | "wezterm" | "ghostty" | "kitty" | "vscode"
        ) {
            return true;
        }
    }
    if let Some(term) = env_get("TERM") {
        let term = term.to_string_lossy().to_ascii_lowercase();
        if term.contains("kitty")
            || term.contains("wezterm")
            || term.contains("ghostty")
            || term.contains("alacritty")
            || term.contains("foot")
            || term.contains("contour")
        {
            return true;
        }
    }
    false
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
    /// Number of first-run setup questions already completed before the
    /// resume picker. When present, the resume picker keeps the same
    /// question-flow chrome and allows the user to go back to setup.
    pub setup_question_count: Option<usize>,
    pub open_config_section: Option<squeezy_core::config_schema::SectionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupRunOutcome {
    Finished,
    BackToSetup,
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
    run_inner(config, provider, None, StartupProfile::default())
        .await
        .map(|_| ())
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
            setup_question_count: None,
            open_config_section: None,
        },
    )
    .await
    .map(|_| ())
}

pub async fn run_with_startup_profile(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    startup: StartupProfile,
) -> Result<StartupRunOutcome> {
    // Resume target carried inside the profile (`--continue` /
    // `--session`) wins over the picker; surface it to `run_inner` as
    // the canonical resume id so the rest of the boot path is identical
    // to `squeezy_tui::resume`.
    let resume = startup.resume_session_id.clone();
    run_inner(config, provider, resume, startup).await
}

pub struct StartupTerminal {
    guard: TerminalGuard,
}

pub struct StartupRunResult {
    pub outcome: StartupRunOutcome,
    pub terminal: Option<StartupTerminal>,
}

pub fn enter_startup_terminal(config: &AppConfig) -> Result<StartupTerminal> {
    apply_theme_overrides(config);
    let guard = TerminalGuard::enter(config.tui.alternate_screen, config.tui.synchronized_output)?;
    Ok(StartupTerminal { guard })
}

pub fn pick_startup_model_selection(
    config: &AppConfig,
    settings_path: &Path,
    choices: Vec<StartupModelPickerProvider>,
    trailing_question_count: usize,
) -> Result<Option<StartupModelPickerResult>> {
    let mut terminal = enter_startup_terminal(config)?;
    pick_startup_model_selection_in_terminal(
        &mut terminal,
        config,
        settings_path,
        choices,
        trailing_question_count,
    )
}

pub fn pick_startup_model_selection_in_terminal(
    terminal: &mut StartupTerminal,
    config: &AppConfig,
    settings_path: &Path,
    choices: Vec<StartupModelPickerProvider>,
    trailing_question_count: usize,
) -> Result<Option<StartupModelPickerResult>> {
    apply_theme_overrides(config);
    let mut themes = render::theme::available_theme_names(config)
        .into_iter()
        .map(|name| StartupThemeChoice {
            label: name.clone(),
            name,
            action: StartupThemeAction::Select,
        })
        .collect::<Vec<_>>();
    themes.push(StartupThemeChoice {
        name: String::new(),
        label: "Custom theme in /config".to_string(),
        action: StartupThemeAction::ConfigureInConfig,
    });
    startup_model_picker::run_picker(
        terminal.guard.term(),
        config,
        settings_path,
        themes,
        choices,
        trailing_question_count,
    )
    .map_err(|err| SqueezyError::Terminal(err.to_string()))
}

pub async fn run_with_startup_profile_in_terminal(
    terminal: StartupTerminal,
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    startup: StartupProfile,
) -> Result<StartupRunResult> {
    let resume = startup.resume_session_id.clone();
    let (outcome, terminal) =
        run_inner_with_terminal(config, provider, resume, startup, terminal.guard).await?;
    Ok(StartupRunResult {
        outcome,
        terminal: terminal.map(|guard| StartupTerminal { guard }),
    })
}

pub fn startup_resume_question_available(config: &AppConfig) -> bool {
    // Only a yes/no answer is needed here, so use the summary-only loader:
    // skip the per-candidate event-log reads that `load_candidates` does for
    // branch detection (the picker itself still does them when it renders).
    let candidates = resume_picker::load_candidate_summaries(config);
    resume_picker::has_scoped_candidates(&candidates, &config.workspace_root)
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
    .map(|_| ())
}

async fn run_inner(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    resume_session_id: Option<String>,
    startup: StartupProfile,
) -> Result<StartupRunOutcome> {
    apply_theme_overrides(&config);
    let terminal =
        TerminalGuard::enter(config.tui.alternate_screen, config.tui.synchronized_output)?;
    let (outcome, _) =
        run_inner_with_terminal(config, provider, resume_session_id, startup, terminal).await?;
    Ok(outcome)
}

async fn run_inner_with_terminal(
    mut config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    resume_session_id: Option<String>,
    startup: StartupProfile,
    mut terminal: TerminalGuard,
) -> Result<(StartupRunOutcome, Option<TerminalGuard>)> {
    // Apply the persisted theme preference before the first render so the
    // initial paint already reflects the user's choice — without this the
    // first frame uses the auto-detected tone and pops to the override on
    // the next redraw.
    apply_theme_overrides(&config);
    let resume_session_id =
        match maybe_pick_resume_session(&mut terminal, &config, resume_session_id, &startup)? {
            ResumeStartup::Use(id) => Some(id),
            ResumeStartup::UseInDir {
                session_id,
                workspace_root,
            } => {
                // Re-root everything (session store, tools, graph, git) at the
                // session's own directory so it opens exactly as if launched
                // there. `set_current_dir` best-effort matches subprocess cwd;
                // `workspace_root` is the authoritative knob the agent reads.
                let _ = std::env::set_current_dir(&workspace_root);
                config.workspace_root = workspace_root;
                apply_theme_overrides(&config);
                Some(session_id)
            }
            ResumeStartup::Fresh => None,
            ResumeStartup::BackToSetup => {
                return Ok((StartupRunOutcome::BackToSetup, Some(terminal)));
            }
            ResumeStartup::Quit => return Ok((StartupRunOutcome::Finished, None)),
        };
    // Cover the gap between the picker exiting and the main loop's
    // first `draw_app`: `Agent::resume`/`Agent::build` walk the
    // workspace, initialise tree-sitter, open redb, and replay
    // `events.jsonl`. The placeholder gives the user immediate feedback
    // instead of a blank viewport for that window.
    let startup_message = if resume_session_id.is_some() {
        "Resuming session…"
    } else {
        "Starting session…"
    };
    squeezy_core::startup_trace::mark("tui_placeholder_drawn");
    let _ = terminal.draw_startup_placeholder(startup_message);
    let (mut agent, initial_transcript) = if let Some(session_id) = resume_session_id {
        Agent::resume(config.clone(), provider, &session_id)?
    } else {
        (Agent::new(config.clone(), provider), Vec::new())
    };
    squeezy_core::startup_trace::mark("agent_built");
    // Plan housekeeping (legacy migration + git-referenced protection +
    // retention pruning) is best-effort maintenance: the 30-day `git
    // log` shell-out and the plan-dir fs walks add tens-to-hundreds of
    // milliseconds and nothing on the input path depends on the result.
    // Run it on the blocking pool so the first frame paints immediately;
    // the result lands as log lines once the main loop drains the
    // channel.
    let session_id_for_plans = agent.session_id();
    let plans_session_owned = session_id_for_plans
        .clone()
        .unwrap_or_else(|| proposed_plan::FALLBACK_SESSION_ID.to_string());
    let (plan_housekeeping_tx, plan_housekeeping_rx) = oneshot::channel::<Vec<String>>();
    let plan_workspace_root = config.workspace_root.clone();
    let plan_session_for_task = plans_session_owned.clone();
    tokio::task::spawn_blocking(move || {
        let mut logs: Vec<String> = Vec::new();
        // One-shot migration of pre-v3 flat-layout plan files into a
        // legacy subdir; safe to run unconditionally.
        let migrated = proposed_plan::migrate_legacy_plans(&plan_workspace_root);
        if migrated > 0 {
            logs.push(format!(
                "migrated {migrated} legacy plan file(s) to {}/{}",
                proposed_plan::PLAN_DIR,
                proposed_plan::LEGACY_PLAN_DIR
            ));
        }
        // Plan ids referenced in the last 30 days of git history survive
        // retention pruning even when older than the cap. Best-effort:
        // no git repo → empty protected set → mtime behaviour.
        let protected_plan_ids = proposed_plan::git_referenced_plan_ids(&plan_workspace_root, 30);
        let pruned = proposed_plan::prune_plan_dir(
            &plan_workspace_root,
            &plan_session_for_task,
            &protected_plan_ids,
        );
        if pruned > 0 {
            logs.push(format!(
                "pruned {pruned} stale plan file(s) from {}/{} (kept {} newest)",
                proposed_plan::PLAN_DIR,
                plan_session_for_task,
                proposed_plan::PLAN_RETENTION_LIMIT
            ));
        }
        let _ = plan_housekeeping_tx.send(logs);
    });
    // `StartupProfile` is moved into `TuiApp::new`, so capture the banner
    // (the only field needed below) before that hand-off.
    let update_banner = startup.update_banner.clone();
    let mut app = TuiApp::new(
        agent.provider_name(),
        &config,
        agent.session_mode(),
        startup,
    );
    squeezy_core::startup_trace::mark("tuiapp_new");
    app.session_id = session_id_for_plans;
    app.plan_housekeeping_rx = Some(plan_housekeeping_rx);
    // Probe repo status (git worktree snapshot + `gh pr view` + branch
    // diff) on the blocking pool instead of in `TuiApp::new`. Those
    // subprocesses — the `gh` network call especially — were the single
    // largest contributor to time-to-interactive; nothing on the input
    // path depends on the result, so the status bar fills in once it lands.
    let (repo_status_tx, repo_status_rx) = oneshot::channel::<RepoStatus>();
    let repo_status_root = config.workspace_root.clone();
    tokio::task::spawn_blocking(move || {
        let _ = repo_status_tx.send(RepoStatus::detect_at(&repo_status_root));
    });
    app.repo_status_rx = Some(repo_status_rx);
    if let Some(banner) = update_banner.filter(|s| !s.trim().is_empty()) {
        app.push_log(banner);
    }
    for item in initial_transcript {
        hydrate_transcript_item(&mut app, item);
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

    squeezy_core::startup_trace::mark("snapshots_done");
    let mut settings_watcher = settings_watcher::SettingsWatcher::new();
    // Poll mtimes roughly once per second; tick_rate defaults to 50ms.
    let settings_poll_every = (1000 / config.tick_rate.as_millis().max(1) as u64).max(1);
    let mut frame_limiter = FrameRateLimiter::default();
    let mut interactive_marked = false;

    loop {
        // Drain producers first so the next draw reflects everything that
        // has landed since the previous iteration. A flurry of events
        // therefore coalesces into a single frame.
        drain_plan_housekeeping(&mut app);
        drain_repo_status(&mut app);
        drain_job_events(&mut app);
        drain_agent_events(&mut app).await;
        if app.auto_drain_queue {
            app.auto_drain_queue = false;
            if app.turn_rx.is_none()
                && let Some(next) = app.prompt_queue.pop_front()
            {
                let remaining = app.prompt_queue.len();
                app.status = if remaining == 0 {
                    "running queued prompt".to_string()
                } else {
                    format!("running queued prompt ({remaining} more queued)")
                };
                start_user_turn(&mut app, &mut agent, next);
            }
        }
        drain_pending_diff(&mut app);
        drain_pending_mention_walk(&mut app);

        // Skip the animation-tick driver while the host terminal is
        // unfocused. Freezing the counter pins the spinner glyph and
        // the title clock, which removes the per-frame draw work that
        // would otherwise keep a background window's GPU and emulator
        // pipeline busy.
        if should_advance_animation_tick(&app) {
            app.animation_tick = app.animation_tick.wrapping_add(1);
        }
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
        // Refresh the language-summary status item from the graph at
        // the same cadence as the settings poll. The agent call is
        // cheap when the file watcher hasn't queued changes (graph
        // refresh is throttled by `idle_refresh_interval` = 15s) and
        // only triggers a redraw when the rendered string actually
        // changes, so idle workspaces pay no draw cost.
        if app.animation_tick.is_multiple_of(settings_poll_every)
            && let Some(report) = agent.current_language_report()
        {
            let next = format_language_report(&report);
            if next != app.language_summary {
                app.language_summary = next;
                app.needs_redraw = true;
            }
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
            if !interactive_marked {
                interactive_marked = true;
                squeezy_core::startup_trace::mark("interactive_ready");
            }
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

    Ok((StartupRunOutcome::Finished, None))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeStartup {
    Use(String),
    /// Resume a session that belongs to a different directory: re-root the
    /// workspace at `workspace_root` so the agent, tools, and graph all open
    /// there, exactly as if `squeezy` had been launched from that directory.
    UseInDir {
        session_id: String,
        workspace_root: PathBuf,
    },
    Fresh,
    BackToSetup,
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
    let setup_progress = startup
        .setup_question_count
        .map(|count| (count.saturating_add(1), count.saturating_add(1)));
    let choice = resume_picker::run_picker(
        terminal.term(),
        candidates,
        config.workspace_root.clone(),
        setup_progress,
    )
    .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
    match choice {
        resume_picker::ResumeChoice::StartFresh => Ok(ResumeStartup::Fresh),
        // `branch_tip` is captured by the picker but not yet wired through
        // the resume flow — the agent restarts at the most recent event in
        // the session log. Branch-aware resume is a follow-up; the
        // schema/picker landing first lets future producers populate
        // `parent_event_sequence` without churn here.
        resume_picker::ResumeChoice::Resume {
            session_id,
            branch_tip: _,
        } => Ok(ResumeStartup::Use(session_id)),
        resume_picker::ResumeChoice::CrossProject {
            session_id,
            target_cwd,
        } => {
            // The user explicitly picked this session, so open it in place by
            // re-rooting at its directory. If that directory no longer exists
            // (repo moved or deleted) fall back to the exit hint rather than
            // resuming against a missing tree.
            let workspace_root = PathBuf::from(&target_cwd);
            if workspace_root.is_dir() {
                Ok(ResumeStartup::UseInDir {
                    session_id,
                    workspace_root,
                })
            } else {
                terminal.set_exit_hint(Some(cross_project_resume_hint(&session_id, &target_cwd)));
                Ok(ResumeStartup::Quit)
            }
        }
        resume_picker::ResumeChoice::Back => Ok(ResumeStartup::BackToSetup),
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

async fn handle_feedback_prompt_key(app: &mut TuiApp, agent: &Agent, key: KeyEvent) -> bool {
    if app.pending_feedback.is_none() {
        return false;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return true;
    }
    match key.code {
        KeyCode::Enter
        | KeyCode::Char('y')
        | KeyCode::Char('Y')
        | KeyCode::Char('s')
        | KeyCode::Char('S') => {
            submit_pending_feedback(app, agent).await;
            true
        }
        KeyCode::Esc
        | KeyCode::Char('n')
        | KeyCode::Char('N')
        | KeyCode::Char('d')
        | KeyCode::Char('D') => {
            discard_pending_feedback(app);
            true
        }
        _ => true,
    }
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
                Ok(Some(report)) => {
                    app.context_compaction.last = Some(report.record.clone());
                    app.context_compaction.generation = report.record.generation;
                    app.context_compaction.summary = Some(report.summary.clone());
                    app.context_compaction.history.push(report.record.clone());
                    app.context_estimate = report.record.after.clone();
                    app.context_compaction_nudge_shown = false;
                    app.push_status(format!(
                        "compacted prior context before executing plan {}",
                        pending.plan_id
                    ));
                }
                Ok(None) => {
                    // Nothing to compact — common on a fresh session.
                    app.push_log(format!(
                        "execute-clean: nothing to compact for plan {}; running",
                        pending.plan_id
                    ));
                }
                Err(err) => {
                    app.push_log(format!(
                        "execute-clean: compaction failed ({err}); running plan"
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
    apply_theme_overrides(&new_cfg);
    // Same pattern for the shell-diff inline preference: TuiApp keeps the
    // canonical value, but the deep render path reads the static.
    app.shell_diff_inline = new_cfg.tui.shell_diff_inline;
    set_shell_diff_inline(new_cfg.tui.shell_diff_inline);
    let old_provider = agent.provider_name();
    let new_provider = squeezy_llm::provider_name(&new_cfg.provider);
    app.provider_name = new_provider;
    app.apply_config_change(&new_cfg);
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
        || app.pending_feedback.is_some()
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
                hydrate_transcript_item(app, item);
            }
            app.attachments = agent.context_attachments_snapshot().await;
            app.pending_assistant.clear();
            app.task_state = None;
            app.task_panel_collapsed = false;
            app.turn_rx = None;
            app.cancel = None;
            // Subagent records are live state for the session we are leaving:
            // they are never persisted or rehydrated, so without this reset the
            // pane keeps the prior session's rows and an active subagent view
            // would hijack the resumed transcript. Keep `next_synthetic_id`
            // monotonic across the switch.
            app.subagent_pane = SubagentPaneState {
                next_synthetic_id: app.subagent_pane.next_synthetic_id,
                ..SubagentPaneState::default()
            };
            app.status = format!("resumed session {session_id}");
        }
        Err(error) => app.status = format!("resume failed: {error}"),
    }
}

/// Apply one `HydratedTranscriptItem` from the resume state to the
/// live `TuiApp`, routing each variant to the matching `push_*`
/// helper so the rebuilt transcript renders the same shape a fresh
/// turn would. Tool-result cards need a roundtrip from
/// `serde_json::Value` back to the typed `squeezy_tools::ToolResult`
/// — done here so `squeezy-store` can stay independent of
/// `squeezy-tools`. Malformed entries log a transcript warning and
/// otherwise no-op rather than abort the whole hydration.
fn hydrate_transcript_item(app: &mut TuiApp, item: squeezy_store::HydratedTranscriptItem) {
    match item {
        squeezy_store::HydratedTranscriptItem::Message { item } => {
            app.push_transcript_item(item);
        }
        squeezy_store::HydratedTranscriptItem::ToolResult { call, result } => {
            let mut result = result;
            if result.get("tool_name").is_none()
                && let Some(tool) = call.as_ref().map(|call| call.tool.as_str())
                && let Some(object) = result.as_object_mut()
            {
                object.insert(
                    "tool_name".to_string(),
                    serde_json::Value::String(tool.to_string()),
                );
            }
            let parsed_result: ToolResult = match serde_json::from_value(result) {
                Ok(result) => result,
                Err(err) => {
                    app.push_warn(format!(
                        "resume: dropped a malformed tool-result card ({err})"
                    ));
                    return;
                }
            };
            let parsed_call = call.map(|call| ToolCall {
                call_id: call.call_id,
                name: call.tool,
                arguments: call.arguments,
            });
            app.push_tool_result_with_call(parsed_result, parsed_call);
        }
    }
}

async fn poll_input(app: &mut TuiApp, agent: &mut Agent, tick_rate: Duration) -> Result<bool> {
    if !event::poll(tick_rate).map_err(|err| SqueezyError::Terminal(err.to_string()))? {
        return Ok(false);
    }

    let mut events = Vec::with_capacity(8);
    events.push(event::read().map_err(|err| SqueezyError::Terminal(err.to_string()))?);
    let max_events = input_events_per_poll_limit(app);
    while events.len() < max_events
        && event::poll(Duration::ZERO).map_err(|err| SqueezyError::Terminal(err.to_string()))?
    {
        events.push(event::read().map_err(|err| SqueezyError::Terminal(err.to_string()))?);
    }
    dispatch_input_events(app, agent, events).await
}

/// Dispatch every event read in a single poll window. Each event already
/// pulled out of crossterm's queue must be handled here — returning after
/// the first key would silently drop the already-read tail (held-key
/// autorepeat, fast typing, paste bursts, and the `Ctrl+X` → `Q` chord
/// follow-up). The main loop coalesces the resulting state into one redraw.
async fn dispatch_input_events(
    app: &mut TuiApp,
    agent: &mut Agent,
    mut events: Vec<Event>,
) -> Result<bool> {
    // While the transcript overlay holds mouse-drag capture, handle a key
    // ahead of the drag repaint flood, then fall through to drain the rest
    // of the batch (the key is removed from `events`, so it is not replayed).
    if let Some(key_event) = take_priority_key_event_for_dispatch(app, &mut events)
        && handle_input_event(app, agent, key_event).await?
    {
        return Ok(true);
    }
    let events = coalesce_input_events_for_dispatch(app, events);
    for input_event in events {
        if handle_input_event(app, agent, input_event).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn take_priority_key_event_for_dispatch(app: &TuiApp, events: &mut Vec<Event>) -> Option<Event> {
    if !transcript_overlay_drag_mode_active(app) {
        return None;
    }
    let key_index = events
        .iter()
        .position(|event| matches!(event, Event::Key(_)))?;
    Some(events.remove(key_index))
}

fn input_events_per_poll_limit(app: &TuiApp) -> usize {
    if transcript_overlay_drag_mode_active(app) {
        MAX_TRANSCRIPT_DRAG_INPUT_EVENTS_PER_POLL
    } else {
        MAX_INPUT_EVENTS_PER_POLL
    }
}

fn coalesce_input_events_for_dispatch(app: &TuiApp, events: Vec<Event>) -> Vec<Event> {
    let mut coalesced = Vec::with_capacity(events.len());
    let mut pending_drag: Option<crossterm::event::MouseEvent> = None;
    for input_event in events {
        match input_event {
            Event::Mouse(mouse) if should_coalesce_transcript_overlay_drag(app, &mouse) => {
                pending_drag = Some(mouse);
            }
            other => {
                if let Some(mouse) = pending_drag.take() {
                    coalesced.push(Event::Mouse(mouse));
                }
                coalesced.push(other);
            }
        }
    }
    if let Some(mouse) = pending_drag {
        coalesced.push(Event::Mouse(mouse));
    }
    coalesced
}

fn should_coalesce_transcript_overlay_drag(
    app: &TuiApp,
    mouse: &crossterm::event::MouseEvent,
) -> bool {
    transcript_overlay_drag_mode_active(app)
        && matches!(
            mouse.kind,
            MouseEventKind::Drag(crossterm::event::MouseButton::Left)
        )
}

fn transcript_overlay_drag_mode_active(app: &TuiApp) -> bool {
    app.transcript_overlay
        .as_ref()
        .is_some_and(|state| state.mode.mouse_capture())
}

async fn handle_input_event(
    app: &mut TuiApp,
    agent: &mut Agent,
    input_event: Event,
) -> Result<bool> {
    // Preserve a deferred redraw if the frame limiter held one back from
    // the previous loop iteration. Individual event arms below add to this
    // instead of blindly repainting for mouse-wheel momentum at scroll
    // boundaries.
    let was_dirty = app.needs_redraw;
    match input_event {
        Event::Key(key) => {
            let before_overlay = app.transcript_overlay;
            let overlay_scroll_key = before_overlay.is_some()
                && is_transcript_overlay_scroll_key(key.code, key.modifiers);
            let quit = handle_key(app, agent, key).await?;
            let unchanged_overlay_scroll =
                overlay_scroll_key && app.transcript_overlay == before_overlay;
            app.needs_redraw = was_dirty || app.needs_redraw || !unchanged_overlay_scroll;
            Ok(quit)
        }
        Event::Mouse(mouse) => {
            if handle_mouse(app, mouse) {
                app.needs_redraw = true;
            } else {
                app.needs_redraw = was_dirty || app.needs_redraw;
            }
            Ok(false)
        }
        Event::Paste(text) => {
            handle_paste(app, agent, text).await?;
            app.needs_redraw = true;
            Ok(false)
        }
        Event::Resize(_, _) => {
            app.pending_resize = true;
            app.needs_redraw = true;
            Ok(false)
        }
        // Crossterm only emits these after `EnableFocusChange` succeeded
        // on a focus-aware terminal. Terminals that ignore the enable
        // sequence (older xterm / Apple Terminal / SSH targets without
        // focus reporting) simply never reach this arm, so `focused`
        // stays `true` and animations run as before. We force a redraw
        // on both transitions: focus-regain wants the spinner to catch
        // up to wall-clock state, and focus-loss wants the final
        // "paused" frame to land before the animation tick driver
        // freezes so the spinner is not stuck mid-cycle when the user
        // tabs back.
        Event::FocusGained => {
            app.focused = true;
            app.needs_redraw = true;
            Ok(false)
        }
        Event::FocusLost => {
            app.focused = false;
            app.needs_redraw = true;
            Ok(false)
        }
    }
}

fn handle_mouse(app: &mut TuiApp, mouse: crossterm::event::MouseEvent) -> bool {
    if let Some(changed) = handle_transcript_overlay_mouse(app, mouse) {
        return changed;
    }

    // Left-click is dispatched via the per-frame click registry so any
    // render path can add new buttons by pushing a `Clickable` — no
    // edits to this fn are needed when new buttons land.
    if let MouseEventKind::Down(crossterm::event::MouseButton::Left) = mouse.kind
        && let Some(action) = app.click_target_at(mouse.column, mouse.row)
    {
        dispatch_click_action(app, action);
        return true;
    }
    // Wheel scroll always scrolls the transcript. The previous
    // `alternate_scroll_enabled` gate dropped wheel events in
    // inline-viewport mode (where the terminal's native
    // wheel-to-arrow translation is disabled), which left the user
    // with no way to scroll at all once mouse capture was on.
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            let before = app.transcript_scroll_from_bottom;
            scroll_transcript_up(app, 3);
            app.transcript_scroll_from_bottom != before
        }
        MouseEventKind::ScrollDown => {
            let before = app.transcript_scroll_from_bottom;
            scroll_transcript_down(app, 3);
            app.transcript_scroll_from_bottom != before
        }
        _ => false,
    }
}

fn handle_transcript_overlay_mouse(
    app: &mut TuiApp,
    mouse: crossterm::event::MouseEvent,
) -> Option<bool> {
    app.transcript_overlay.as_ref()?;
    let changed = match mouse.kind {
        MouseEventKind::ScrollUp => {
            adjust_transcript_overlay_scroll(app, |scroll| scroll.saturating_sub(3))
        }
        MouseEventKind::ScrollDown => {
            adjust_transcript_overlay_scroll(app, |scroll| scroll.saturating_add(3))
        }
        MouseEventKind::Down(crossterm::event::MouseButton::Left)
        | MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
            if app
                .transcript_overlay
                .as_ref()
                .is_some_and(|state| state.mode.mouse_capture())
                && let Some((scroll, max_scroll)) =
                    transcript_overlay_scroll_from_mouse(app, mouse.column, mouse.row)
            {
                set_transcript_overlay_scroll_from_cached_geometry(app, scroll, max_scroll)
            } else {
                false
            }
        }
        _ => false,
    };
    Some(changed)
}

fn adjust_transcript_overlay_scroll(app: &mut TuiApp, f: impl FnOnce(usize) -> usize) -> bool {
    let scroll = f(resolved_transcript_overlay_scroll(app));
    set_transcript_overlay_scroll(app, scroll)
}

fn set_transcript_overlay_scroll(app: &mut TuiApp, scroll: usize) -> bool {
    let scroll = clamp_transcript_overlay_scroll(app, scroll);
    set_transcript_overlay_scroll_known_clamped(app, scroll)
}

fn set_transcript_overlay_scroll_known_clamped(app: &mut TuiApp, scroll: usize) -> bool {
    let scroll = transcript_overlay_max_scroll(app)
        .map(|max_scroll| {
            if scroll >= max_scroll {
                TRANSCRIPT_OVERLAY_SCROLL_BOTTOM
            } else {
                scroll
            }
        })
        .unwrap_or(scroll);
    if let Some(state) = app.transcript_overlay.as_mut() {
        if state.scroll == scroll {
            return false;
        }
        state.scroll = scroll;
        true
    } else {
        false
    }
}

fn set_transcript_overlay_scroll_from_cached_geometry(
    app: &mut TuiApp,
    scroll: usize,
    max_scroll: usize,
) -> bool {
    let scroll = if scroll >= max_scroll {
        TRANSCRIPT_OVERLAY_SCROLL_BOTTOM
    } else {
        scroll
    };
    if let Some(state) = app.transcript_overlay.as_mut() {
        if state.scroll == scroll {
            return false;
        }
        state.scroll = scroll;
        true
    } else {
        false
    }
}

fn resolved_transcript_overlay_scroll(app: &TuiApp) -> usize {
    let Some(state) = app.transcript_overlay else {
        return 0;
    };
    transcript_overlay_max_scroll(app)
        .map(|max_scroll| {
            if state.scroll == TRANSCRIPT_OVERLAY_SCROLL_BOTTOM {
                max_scroll
            } else {
                state.scroll.min(max_scroll)
            }
        })
        .unwrap_or(if state.scroll == TRANSCRIPT_OVERLAY_SCROLL_BOTTOM {
            0
        } else {
            state.scroll
        })
}

fn clamp_transcript_overlay_scroll(app: &TuiApp, scroll: usize) -> usize {
    transcript_overlay_max_scroll(app)
        .map(|max_scroll| scroll.min(max_scroll))
        .unwrap_or(scroll)
}

fn is_transcript_overlay_scroll_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::META)
        && matches!(
            code,
            KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
        )
}

fn transcript_overlay_max_scroll(app: &TuiApp) -> Option<usize> {
    let (width, height) = terminal_size().ok()?;
    let full_area = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let (area, _) = transcript_overlay_content_and_status_areas(full_area);
    let inner = transcript_overlay_inner(area);
    let (text_area, _) = transcript_overlay_text_and_scrollbar_areas(inner)?;
    Some(with_transcript_overlay_rows(app, text_area.width, |rows| {
        transcript_overlay_max_scroll_for_content(rows.len(), text_area.height)
    }))
}

fn transcript_overlay_scroll_from_mouse(
    app: &TuiApp,
    column: u16,
    row: u16,
) -> Option<(usize, usize)> {
    let cached = app.transcript_overlay_scrollbar_cache.get()?;
    let scrollbar_area = cached.scrollbar_area;
    if column != scrollbar_area.x
        || row < scrollbar_area.y
        || row >= scrollbar_area.y.saturating_add(scrollbar_area.height)
    {
        return None;
    }
    Some((
        transcript_overlay_scroll_for_cached_scrollbar_row(row, cached),
        cached.geometry.max_scroll,
    ))
}

/// Canonicalise a `KeyEvent` so every downstream dispatcher sees a
/// single shape regardless of which terminal-protocol level emitted it.
///
/// Three normalisations live here, in order:
///
/// 1. **`META` modifier → `ALT`.** Some terminal protocols carry
///    `Option`/`Alt` as `KeyModifiers::META` rather than `ALT`. We
///    flatten so keymap lookups don't have to know.
///
/// 2. **Raw ASCII control byte → `Char(letter) + CONTROL`.** Terminals
///    that don't honour kitty's `DISAMBIGUATE_ESCAPE_CODES` deliver
///    `Ctrl+E` as the literal byte `\x05` with empty modifiers. We
///    map `\x01..=\x1A` (skipping `\x08`/`\x09`/`\x0A`/`\x0D`/`\x1B`
///    which have dedicated `KeyCode` variants).
///
/// 3. **Lowercase the letter + drop SHIFT in Ctrl/Alt+letter combos.**
///    Kitty's `REPORT_ALTERNATE_KEYS` can deliver `Ctrl+E` as
///    `Char('E') + CONTROL` (the base-layout key), and a user holding
///    Shift can leak an extra `SHIFT` bit. Either would miss the
///    keymap entry stored as `Char('e') + CONTROL`.
fn normalise_control_byte(mut key: KeyEvent) -> KeyEvent {
    if key.modifiers.contains(KeyModifiers::META) {
        key.modifiers.remove(KeyModifiers::META);
        key.modifiers.insert(KeyModifiers::ALT);
    }
    if !key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(ch) = key.code
    {
        let byte = ch as u32;
        let is_remappable = matches!(
            byte,
            0x01..=0x07 | 0x0B..=0x0C | 0x0E..=0x1A,
        );
        if is_remappable {
            let letter = char::from_u32(byte + 0x60).expect("0x01..0x1A maps into 'a'..'z'");
            key.code = KeyCode::Char(letter);
            key.modifiers |= KeyModifiers::CONTROL;
        }
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && let KeyCode::Char(ch) = key.code
        && ch.is_ascii_alphabetic()
    {
        key.code = KeyCode::Char(ch.to_ascii_lowercase());
        key.modifiers.remove(KeyModifiers::SHIFT);
    }
    debug_log_key_event(&key);
    key
}

/// Append a one-line summary of `key` to the path named by the
/// `SQUEEZY_DEBUG_KEYS` env var. Silent no-op when unset or when the
/// file can't be opened — diagnostics must never break the TUI.
fn debug_log_key_event(key: &KeyEvent) {
    use std::io::Write;
    let Some(path) = std::env::var_os("SQUEEZY_DEBUG_KEYS") else {
        return;
    };
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(
        f,
        "{:?} mods={:?} kind={:?}",
        key.code, key.modifiers, key.kind
    );
}

/// Switch the shown conversation to the currently highlighted pane row and
/// reset its scroll, so moving the selection previews the conversation live
/// instead of only committing on Enter.
fn select_subagent_row(app: &mut TuiApp) {
    app.subagent_pane.active = if app.subagent_pane.selected == 0 {
        ConversationSource::Main
    } else {
        app.subagent_pane
            .records
            .get(app.subagent_pane.selected - 1)
            .map(|record| ConversationSource::Subagent(record.id))
            .unwrap_or(ConversationSource::Main)
    };
    set_active_transcript_scroll_from_bottom(app, 0);
    app.status = match app.subagent_pane.active {
        ConversationSource::Main => "main conversation".to_string(),
        ConversationSource::Subagent(_) => "subagent conversation".to_string(),
    };
}

fn handle_subagent_pane_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    if app.subagent_pane.records.is_empty() || !key.modifiers.is_empty() {
        return false;
    }
    let row_count = 1 + app.subagent_pane.records.len();
    app.subagent_pane.selected = app.subagent_pane.selected.min(row_count.saturating_sub(1));
    match key.code {
        KeyCode::Down if app.subagent_pane.focused => {
            app.subagent_pane.selected = (app.subagent_pane.selected + 1).min(row_count - 1);
            select_subagent_row(app);
            true
        }
        KeyCode::Down if app.input.is_empty() => {
            app.subagent_pane.focused = true;
            if row_count > 1 && app.subagent_pane.selected == 0 {
                app.subagent_pane.selected = 1;
            }
            select_subagent_row(app);
            true
        }
        KeyCode::Up if app.subagent_pane.focused && app.subagent_pane.selected > 0 => {
            app.subagent_pane.selected -= 1;
            select_subagent_row(app);
            true
        }
        KeyCode::Up if app.subagent_pane.focused => {
            app.subagent_pane.focused = false;
            app.status = "subagent pane closed".to_string();
            true
        }
        KeyCode::Enter if app.subagent_pane.focused => {
            // Selection already previews live (see `select_subagent_row`);
            // Enter just releases pane focus so the arrow keys scroll the
            // now-active transcript instead of moving the selector.
            app.subagent_pane.focused = false;
            app.status = match app.subagent_pane.active {
                ConversationSource::Main => "main conversation selected".to_string(),
                ConversationSource::Subagent(_) => "subagent conversation selected".to_string(),
            };
            true
        }
        KeyCode::Esc
            if app.subagent_pane.focused
                || !matches!(app.subagent_pane.active, ConversationSource::Main) =>
        {
            app.subagent_pane.focused = false;
            app.subagent_pane.active = ConversationSource::Main;
            app.subagent_pane.selected = 0;
            set_active_transcript_scroll_from_bottom(app, 0);
            app.status = "main conversation selected".to_string();
            true
        }
        KeyCode::Delete | KeyCode::Backspace if app.subagent_pane.focused => {
            app.clear_finished_subagents();
            true
        }
        // Any other key releases pane focus and falls through to its normal
        // handler (the composer, slash commands, …) so the prompt is never
        // trapped behind the pane.
        _ if app.subagent_pane.focused => {
            app.subagent_pane.focused = false;
            false
        }
        _ => false,
    }
}

pub(crate) async fn handle_key(app: &mut TuiApp, agent: &mut Agent, key: KeyEvent) -> Result<bool> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(false);
    }

    // Normalise raw ASCII control bytes into their canonical
    // `Char(<lowercase letter>) + CONTROL` form before any dispatcher
    // sees them. Terminals that do not fully honour kitty's
    // DISAMBIGUATE_ESCAPE_CODES (Apple Terminal, older tmux versions,
    // many SSH targets) emit e.g. `Ctrl+E` as `KeyCode::Char('\x05')`
    // with empty modifiers, which silently misses every keymap arm
    // that looks for `Char('e') + CONTROL`. This is the source of the
    // "Ctrl+E does nothing", "Ctrl+X Q types Q" class of bugs.
    let key = normalise_control_byte(key);

    // Any keypress while a turn-done notification is up counts as the
    // user acknowledging it — drop the title back to cleared so the
    // emulator's tab/window stops showing the bulb glyph.
    if app.terminal_title_state == TerminalTitleState::Notification {
        app.terminal_title_state = TerminalTitleState::Cleared;
    }

    // Chord follow-ups run BEFORE any single-key dispatch so the second
    // stroke of a chord never accidentally fires a normal keybinding.
    if app.transcript_overlay.is_some() {
        app.pending_chord = None;
    } else if let Some(prefix) = app.pending_chord.take() {
        // Permissive guard: `Q` may arrive as `Char('q')` or `Char('Q')`
        // and may carry stray SHIFT / CAPS_LOCK / KEYPAD bits depending
        // on the kitty keyboard protocol level the terminal advertises.
        // Only `CONTROL` and `ALT` should disqualify the chord (e.g.
        // `Ctrl+X` then `Ctrl+Q` is a different key combo).
        let blocking = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let is_chord_q = matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
            && !key.modifiers.intersects(blocking);
        if prefix == ChordPrefix::CtrlX && is_chord_q {
            toggle_prompt_queue_overlay(app);
            return Ok(false);
        }
        if key.code == KeyCode::Esc {
            app.status = "chord cancelled".to_string();
            return Ok(false);
        }
        // Anything else: the chord is already cleared via `.take()`; the
        // keystroke falls through to its normal handler.
        app.status.clear();
    }

    // `Ctrl+X` starts the queue-overlay chord. `normalise_control_byte`
    // above already canonicalised any raw `\x18` byte to
    // `Char('x') + CONTROL`, so a single check is enough here.
    if matches!(key.code, KeyCode::Char('x') | KeyCode::Char('X'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && app.transcript_overlay.is_none()
    {
        app.pending_chord = Some(ChordPrefix::CtrlX);
        app.status = "Ctrl+X… (Q opens queue · Esc cancels)".to_string();
        return Ok(false);
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
    if app.transcript_overlay.is_none()
        && app.input.is_empty()
        && !app.app_notifications.is_empty()
        && key.modifiers.is_empty()
    {
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

    // The transcript-overlay open/close action is dispatched up top
    // by `dispatch_keymap_action`. While the overlay is open we still
    // need to forward navigation keys to its own handler before the
    // composer takes over.
    if app.transcript_overlay.is_some() && handle_transcript_overlay_key(app, key) {
        return Ok(false);
    }

    // Prompt-queue reorder overlay: same pattern. Routed before the
    // Esc-cancel-turn shortcut so Esc-in-overlay closes the overlay
    // rather than interrupting the running turn.
    if app.prompt_queue_overlay.is_some() && handle_prompt_queue_overlay_key(app, key) {
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

    if handle_feedback_prompt_key(app, agent, key).await {
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

    // (Old `Ctrl+E` readline line-end is gone — that key now toggles
    // expand-all unconditionally via the keymap. Use `End` /
    // `Cmd+Right` for cursor-to-line-end.)

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

    // A focused subagent pane owns Esc (to close itself); don't let Esc
    // cancel an in-flight turn out from under the user while they're
    // navigating the pane.
    if key.code == KeyCode::Esc
        && !app.subagent_pane.focused
        && matches!(app.subagent_pane.active, ConversationSource::Main)
        && request_turn_interrupt(app)
    {
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

    if should_route_plain_arrow_to_scroll_before_subagent_pane(app, key) {
        match key.code {
            KeyCode::Up => scroll_transcript_up(app, 3),
            KeyCode::Down => scroll_transcript_down(app, 3),
            _ => {}
        }
        return Ok(false);
    }

    if handle_subagent_pane_key(app, key) {
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
            } else if move_input_cursor_up(app) {
                // Multi-line input: step the cursor up one line. Falls
                // through to history/scroll when the cursor is already on
                // the first line so the single-line behaviour is intact.
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
            } else if move_input_cursor_down(app) {
                // Same as Up — when there's a next line in the composer
                // step into it; only when on the last line do we fall
                // through to history/scroll.
            } else if should_route_plain_arrow_to_scroll(app) {
                scroll_transcript_down(app, 3);
            } else {
                recall_prompt_history(app, HistoryDirection::Next);
            }
            Ok(false)
        }
        KeyCode::Enter => {
            if complete_selected_slash_command(app) {
                return Ok(false);
            }
            if input_cursor(app) == app.input.len() && app.input.ends_with('\\') {
                delete_before_cursor(app);
                insert_input_char(app, '\n');
                return Ok(false);
            }
            let raw_input = app.input.clone();
            let input = raw_input.trim().to_string();
            if input.is_empty() {
                app.status = "enter a prompt first".to_string();
                return Ok(false);
            }
            // Slash commands always execute immediately — they're UI
            // actions, not turn-equivalent prompts, so they shouldn't
            // queue behind a running turn.
            let before_command_input = app.input.clone();
            if handle_slash_command(app, agent, &input).await {
                let preserve_input =
                    app.preserve_input_after_slash_command || app.input != before_command_input;
                app.preserve_input_after_slash_command = false;
                if !preserve_input {
                    clear_input(app);
                }
                app.input_history_index = None;
                app.input_history_draft.clear();
                app.slash_menu_index = 0;
                return Ok(false);
            }
            if handle_inline_slash_command(app, agent, &raw_input).await {
                return Ok(false);
            }
            if reject_unknown_slash_command(app, &input) {
                return Ok(false);
            }
            if app.turn_rx.is_some() {
                app.prune_prompt_attachments();
                if app
                    .prompt_attachments
                    .iter()
                    .any(|attachment| input.contains(&attachment.placeholder))
                {
                    app.status = "prompt attachments cannot be queued; wait for the current turn"
                        .to_string();
                    return Ok(false);
                }
                app.prompt_queue.push_back(input.clone());
                clear_input(app);
                push_input_history(app, input);
                app.status = format!("queued ({})", app.prompt_queue.len());
                return Ok(false);
            }
            // Stash the typed prompt before clearing so that a Ctrl-C/Esc
            // during the turn can restore it via Ctrl-R. Completion clears
            // this field; only Cancelled/Failed leave it set.
            app.cancelled_prompt = Some(input.clone());
            push_input_history(app, input.clone());
            start_user_turn(app, agent, input);
            clear_input(app);
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

async fn handle_paste(app: &mut TuiApp, _agent: &mut Agent, text: String) -> Result<()> {
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
    if app.pending_approval.is_some() || app.pending_mcp_elicitation.is_some() {
        app.status = "paste unavailable while a modal prompt is open".to_string();
        return Ok(());
    }

    if let Some(status) = insert_pasted_image_path_token(app, &normalized) {
        app.status = status;
        return Ok(());
    }

    if is_large_prompt_paste(&normalized) {
        let chars = normalized.chars().count();
        let placeholder =
            insert_prompt_text_token(app, format!("[Pasted Content {chars} chars]"), normalized);
        app.status = format!("inserted {placeholder}");
    } else {
        insert_input_text(app, &normalized);
    }
    Ok(())
}

fn insert_pasted_image_path_token(app: &mut TuiApp, text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let candidate = trimmed.trim_matches('"');
    let resolved = resolve_workspace_path(&app.workspace_root, candidate);
    if !resolved.is_file() {
        return None;
    }
    let bytes = match std::fs::read(&resolved) {
        Ok(bytes) => bytes,
        Err(error) => return Some(format!("image paste failed: {error}")),
    };
    let media_type = detect_image_mime(&bytes)?;
    let label = file_label(&resolved);
    let placeholder = insert_prompt_image_token(
        app,
        format!("[Image {label}]"),
        media_type.to_string(),
        bytes,
    );
    Some(format!("inserted {placeholder}"))
}

fn insert_file_prompt_attachment(app: &mut TuiApp, path: &str) -> Result<String> {
    let resolved = resolve_workspace_path(&app.workspace_root, path);
    let bytes = std::fs::read(&resolved)?;
    let label = file_label(&resolved);
    let display_path = display_workspace_path(&app.workspace_root, &resolved);
    let text = std::str::from_utf8(&bytes).ok();
    let kind = detect_context_attachment_kind(Some(&label), &bytes, text);
    if kind == ContextAttachmentKind::Image {
        let media_type = detect_image_mime(&bytes)
            .map(str::to_string)
            .unwrap_or_else(|| "image/png".to_string());
        let placeholder =
            insert_prompt_image_token(app, format!("[Image {label}]"), media_type, bytes);
        return Ok(format!("inserted {placeholder}"));
    }
    if !kind.is_supported_text() {
        return Err(SqueezyError::Agent(format!(
            "unsupported file kind={}",
            kind.as_str()
        )));
    }
    let text = text.unwrap_or_default();
    let (bounded_text, truncated) =
        context_attachment_storage_text(text, DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES);
    let mut replacement = format!("Attached file {display_path}:\n{bounded_text}");
    if truncated {
        replacement.push_str("\n[truncated]");
    }
    let placeholder =
        insert_prompt_text_token(app, format!("[Attached file {label}]"), replacement);
    Ok(format!("inserted {placeholder}"))
}

fn resolve_workspace_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file")
        .to_string()
}

fn display_workspace_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn insert_prompt_text_token(
    app: &mut TuiApp,
    base_placeholder: String,
    replacement: String,
) -> String {
    let placeholder = unique_prompt_placeholder(app, &base_placeholder);
    insert_input_text(app, &placeholder);
    app.prompt_attachments.push(PromptAttachment {
        placeholder: placeholder.clone(),
        payload: PromptAttachmentPayload::Text { replacement },
    });
    placeholder
}

fn insert_prompt_image_token(
    app: &mut TuiApp,
    base_placeholder: String,
    media_type: String,
    bytes: Vec<u8>,
) -> String {
    let placeholder = unique_prompt_placeholder(app, &base_placeholder);
    insert_input_text(app, &placeholder);
    app.prompt_attachments.push(PromptAttachment {
        placeholder: placeholder.clone(),
        payload: PromptAttachmentPayload::Image {
            media_type,
            bytes: Arc::from(bytes.into_boxed_slice()),
        },
    });
    placeholder
}

fn unique_prompt_placeholder(app: &TuiApp, base: &str) -> String {
    if !prompt_placeholder_in_use(app, base) {
        return base.to_string();
    }
    let stem = base.strip_suffix(']').unwrap_or(base);
    for index in 2.. {
        let candidate = format!("{stem} #{index}]");
        if !prompt_placeholder_in_use(app, &candidate) {
            return candidate;
        }
    }
    unreachable!("unbounded placeholder suffix search must return")
}

fn prompt_placeholder_in_use(app: &TuiApp, placeholder: &str) -> bool {
    app.input.contains(placeholder)
        || app
            .prompt_attachments
            .iter()
            .any(|attachment| attachment.placeholder == placeholder)
}

fn normalize_pasted_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn is_large_prompt_paste(text: &str) -> bool {
    text.chars().count() > LARGE_PASTE_CHAR_THRESHOLD
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
    if app.transcript_overlay.is_some() && action != keymap::Action::ToggleTranscriptOverlay {
        return false;
    }
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
            app.transcript_overlay_scrollbar_cache.set(None);
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
            copy_last_assistant_to_clipboard(app);
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
                set_active_transcript_scroll_from_bottom(app, u16::MAX);
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
                set_active_transcript_scroll_from_bottom(app, 0);
                true
            } else {
                false
            }
        }
    }
}

/// Toggle the prompt-queue reorder overlay. Hard-coded as a chord
/// (`Ctrl+X` then `Q`) rather than a rebindable keymap action because
/// single-Ctrl-letter defaults collide with terminal flow control
/// (`Ctrl+Q` = `XON`, `Ctrl+S` = `XOFF`) and with macOS shortcuts.
fn toggle_prompt_queue_overlay(app: &mut TuiApp) {
    if app.config_screen.is_some()
        || app.status_line_setup.is_some()
        || app.transcript_overlay.is_some()
    {
        return;
    }
    app.prompt_queue_overlay = if app.prompt_queue_overlay.is_some() {
        None
    } else {
        Some(prompt_queue::PromptQueueState::new())
    };
    app.status = if app.prompt_queue_overlay.is_some() {
        format!("prompt queue ({} queued)", app.prompt_queue.len())
    } else {
        "prompt queue closed".to_string()
    };
}

/// Dispatch a click on a registered `Clickable`. Single source of truth
/// for what each `ClickAction` variant does. Adding a new button means
/// adding a variant to `ClickAction` and one arm here.
fn dispatch_click_action(app: &mut TuiApp, action: ClickAction) {
    match action {
        ClickAction::ToggleQueueOverlay => toggle_prompt_queue_overlay(app),
    }
}

fn scroll_transcript_up(app: &mut TuiApp, lines: u16) {
    let scroll = active_transcript_scroll_from_bottom(app).saturating_add(lines);
    set_active_transcript_scroll_from_bottom(app, scroll);
}

fn scroll_transcript_down(app: &mut TuiApp, lines: u16) {
    let scroll = active_transcript_scroll_from_bottom(app).saturating_sub(lines);
    set_active_transcript_scroll_from_bottom(app, scroll);
}

fn should_route_plain_arrow_to_scroll(app: &TuiApp) -> bool {
    app.alternate_scroll_enabled
        && app.input_history_index.is_none()
        && !active_transcript_entries(app).is_empty()
}

fn should_route_plain_arrow_to_scroll_before_subagent_pane(app: &TuiApp, key: KeyEvent) -> bool {
    if app.subagent_pane.focused
        || app.input_history_index.is_some()
        || !app.input.is_empty()
        || !key.modifiers.is_empty()
        || !app.alternate_scroll_enabled
        || active_transcript_entries(app).is_empty()
    {
        return false;
    }
    match key.code {
        KeyCode::Up => true,
        KeyCode::Down => active_transcript_scroll_from_bottom(app) > 0,
        _ => false,
    }
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

/// Apply the resolved theme table in one shot so all token lookups and
/// render caches observe the same generation.
pub(crate) fn apply_theme_overrides(config: &AppConfig) {
    render::theme::set_active_theme(config);
    render::spinner::set_active_spinner(config);
}

/// Apply a `/theme` switch: flip the runtime palette override, mirror the
/// new value into the agent's in-memory config, and persist to the user-
/// scope settings file so the choice survives a restart. Persistence failures
/// surface in the status line but the live switch still takes effect — the
/// user can re-run later to retry the save.
fn apply_theme_change(app: &mut TuiApp, agent: &mut Agent, theme: String) {
    use squeezy_core::settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits};

    let mut next = agent.config_snapshot();
    next.tui.theme = theme.clone();
    apply_theme_overrides(&next);
    agent.replace_config(next);

    let target_path = app.user_settings_path();
    let scope_target = SettingsScope::user(&target_path);
    let edits = [SettingsEdit {
        path: &["tui", "theme"],
        op: EditOp::SetString(theme.clone()),
    }];
    match apply_edits(&scope_target, &edits) {
        Ok(_) => {
            app.app_notifications
                .push(format!("theme → {theme}"), NotifySeverity::Success);
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

    let target_path = app.user_settings_path();
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

/// Whether the transcript should show a styled banner for this slash
/// command's invocation. Commands that open their own UI overlay
/// (`/config`, `/statusline`, …) are silenced — the overlay is the
/// affordance. Commands that route through `start_user_turn` and
/// already produce a user-message bubble (`/help`) are also silenced
/// to avoid duplication. `/verbosity` and `/tool-verbosity` open a UI
/// when called bare but silently apply a value when given an arg —
/// echo only the second form. Unrecognized commands are not echoed:
/// they fall through to be sent as regular user prompts.
fn should_echo_slash_command(command: &str, rest: &str) -> bool {
    if !SLASH_COMMANDS.iter().any(|spec| spec.name == command) {
        return false;
    }
    match command {
        "/config" | "/options" | "/statusline" | "/model" | "/permissions" | "/help" => false,
        "/verbosity" | "/tool-verbosity" => !rest.trim().is_empty(),
        _ => true,
    }
}

pub(crate) async fn handle_slash_command(app: &mut TuiApp, agent: &mut Agent, input: &str) -> bool {
    let cmd = match DispatchCommand::parse(input) {
        Ok(cmd) => cmd,
        // Unknown heads fall through to the user-authored prompt
        // template catalog. A match expands the template body and
        // routes it through the regular user-turn machinery so the
        // model sees the rendered prompt, not the literal slash text.
        // No match keeps the legacy behaviour: the input is treated as
        // a normal user prompt (echoed as a slash echo upstream by
        // `reject_unknown_slash_command`).
        Err(DispatchCommandParseError::Unknown { .. }) => {
            return expand_prompt_template_or_fallthrough(app, agent, input);
        }
        Err(DispatchCommandParseError::NotASlashCommand)
        | Err(DispatchCommandParseError::Empty) => return false,
        // Required-arg failures preserve the pre-refactor `usage:`
        // strings so the visible affordance is unchanged.
        Err(DispatchCommandParseError::Usage { hint, .. }) => {
            set_status_with_notice(app, hint.clone(), hint);
            return true;
        }
    };

    let raw_head = input.split_whitespace().next().unwrap_or_default();
    let slash = cmd.slash_name();
    if let Some(spec) = SLASH_COMMANDS.iter().find(|spec| spec.name == slash)
        && !spec.available_during_task
        && turn_in_progress(app)
    {
        app.status = format!("{slash} unavailable during turn");
        return true;
    }

    let rest = input
        .strip_prefix(raw_head)
        .map(str::trim)
        .unwrap_or_default();
    if should_echo_slash_command(raw_head, rest) {
        app.push_slash_command_echo(input);
    }

    apply_dispatch_command(app, agent, cmd).await;
    true
}

fn set_status_notice(app: &mut TuiApp, message: impl Into<String>) {
    let message = message.into();
    app.status = message.clone();
    app.push_transcript_item(TranscriptItem::system(message));
}

fn set_status_with_notice(app: &mut TuiApp, status: impl Into<String>, notice: impl Into<String>) {
    app.status = status.into();
    app.push_transcript_item(TranscriptItem::system(notice.into()));
}

async fn handle_inline_slash_command(app: &mut TuiApp, agent: &mut Agent, input: &str) -> bool {
    let Some(occurrence) = input::find_inline_slash_dispatch_command(input) else {
        return false;
    };
    let slash = occurrence.command.name;
    if !occurrence.command.available_during_task && turn_in_progress(app) {
        app.status = format!("{slash} unavailable during turn");
        return true;
    }
    match slash {
        "/attach" => handle_inline_attach_command(app, input, occurrence.start, occurrence.end),
        "/help" | "/plan" | "/build" => {
            let command_input =
                inline_prompt_command_input(input, occurrence.start, occurrence.end, slash);
            let before_command_input = app.input.clone();
            if handle_slash_command(app, agent, &command_input).await {
                let preserve_input =
                    app.preserve_input_after_slash_command || app.input != before_command_input;
                app.preserve_input_after_slash_command = false;
                if !preserve_input {
                    clear_input(app);
                }
                app.input_history_index = None;
                app.input_history_draft.clear();
                app.slash_menu_index = 0;
                return true;
            }
            false
        }
        _ => false,
    }
}

fn inline_prompt_command_input(input: &str, start: usize, end: usize, slash: &str) -> String {
    let before = input[..start].trim();
    let after = input[end..].trim();
    let prompt = if slash == "/help" {
        after.to_string()
    } else {
        [before, after]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    if prompt.is_empty() {
        slash.to_string()
    } else {
        format!("{slash} {prompt}")
    }
}

fn handle_inline_attach_command(
    app: &mut TuiApp,
    input: &str,
    command_start: usize,
    command_end: usize,
) -> bool {
    let Some((path, path_end)) = inline_attach_path(input, command_end) else {
        set_status_notice(app, "usage: /attach <path>");
        return true;
    };
    let original_input = app.input.clone();
    let original_cursor = app.input_cursor;
    app.input.replace_range(command_start..path_end, "");
    app.input_cursor = command_start;
    match insert_file_prompt_attachment(app, &path) {
        Ok(status) => app.status = status,
        Err(error) => {
            app.input = original_input;
            app.input_cursor = original_cursor;
            app.preserve_input_after_slash_command = true;
            app.status = format!("attach failed: {error}");
        }
    }
    true
}

fn inline_attach_path(input: &str, command_end: usize) -> Option<(String, usize)> {
    let (relative_start, first) = input[command_end..]
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())?;
    let path_start = command_end + relative_start;
    if first == '"' || first == '\'' {
        let value_start = path_start + first.len_utf8();
        let close = input[value_start..]
            .char_indices()
            .find(|(_, ch)| *ch == first)
            .map(|(index, _)| value_start + index)?;
        let path = input[value_start..close].to_string();
        let path_end = close + first.len_utf8();
        return (!path.is_empty()).then_some((path, path_end));
    }
    let path_end = input[path_start..]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, _)| path_start + index)
        .unwrap_or(input.len());
    let path = input[path_start..path_end].to_string();
    (!path.is_empty()).then_some((path, path_end))
}

/// Attempt to resolve an unknown `/foo …` head against the user-
/// authored prompt-template catalog. On a hit the rendered body is
/// routed through the normal user-turn flow (echo + history + queue
/// or start) so the model sees the expanded prompt; on a miss this
/// returns `false` so the legacy "unknown command" path runs and the
/// input becomes a regular user prompt.
///
/// Templates take precedence only over the *unknown* slot — built-in
/// `DispatchCommand` heads (e.g. `/help`, `/diff`) cannot be shadowed
/// by a same-named template file so muscle memory keeps working.
fn expand_prompt_template_or_fallthrough(app: &mut TuiApp, agent: &mut Agent, input: &str) -> bool {
    let Some(expanded) = app.prompt_templates.expand(input) else {
        return false;
    };
    app.push_slash_command_echo(input);
    if app.turn_rx.is_some() {
        app.prompt_queue.push_back(expanded);
        app.status = format!("queued ({})", app.prompt_queue.len());
        return true;
    }
    app.cancelled_prompt = Some(expanded.clone());
    push_input_history(app, expanded.clone());
    start_user_turn(app, agent, expanded);
    true
}

/// Run the typed slash-command on the TUI. The parsing is done by
/// [`DispatchCommand::parse`] in [`handle_slash_command`]; this
/// function only routes to the existing helpers that own the TUI
/// state. Agent-only dispatch lives on [`Agent::dispatch_command`] and
/// is invoked by non-TUI drivers (eval, RPC).
async fn apply_dispatch_command(app: &mut TuiApp, agent: &mut Agent, cmd: DispatchCommand) {
    match cmd {
        DispatchCommand::Config { section } => {
            let slug = section.as_deref();
            let id = slug.and_then(squeezy_core::config_schema::section_from_slug);
            // `/config <slug>` used to silently fall back to the Models
            // section when the slug was a valid `SectionId` variant that
            // had no `ConfigSectionMeta` entry (Skills, Tools, Providers,
            // Context, McpServers, ShellSandbox, PermissionRules) or was
            // simply unrecognised. Make the fallback explicit so the user
            // knows their argument was ignored.
            if let Some(raw) = slug
                && !raw.is_empty()
                && id.is_none()
            {
                app.app_notifications.push(
                    format!(
                        "/config: '{raw}' is not a navigable section — opening the default view. \
                         Press / to search field labels."
                    ),
                    NotifySeverity::Warn,
                );
            }
            toggle_config_screen(app, agent, id);
        }
        DispatchCommand::Statusline => toggle_status_line_setup(app),
        DispatchCommand::Plan { prompt } => {
            switch_mode(app, agent, Some(SessionMode::Plan), "tui_command");
            if let Some(prompt) = prompt {
                app.cancelled_prompt = Some(prompt.clone());
                clear_input(app);
                push_input_history(app, prompt.clone());
                start_user_turn(app, agent, prompt);
            }
        }
        DispatchCommand::Build { prompt } => {
            switch_mode(app, agent, Some(SessionMode::Build), "tui_command");
            if let Some(prompt) = prompt {
                app.cancelled_prompt = Some(prompt.clone());
                clear_input(app);
                push_input_history(app, prompt.clone());
                start_user_turn(app, agent, prompt);
            }
        }
        DispatchCommand::Plans { args } => handle_plans_command(app, &args),
        DispatchCommand::Cost => {
            let snapshot = agent.session_accounting_snapshot().await;
            app.status = "cost snapshot".to_string();
            app.push_transcript_item(TranscriptItem::system(commands::format_cost_command(
                &snapshot,
            )));
        }
        DispatchCommand::Context => {
            let snapshot = agent.session_accounting_snapshot().await;
            app.status = "context snapshot".to_string();
            app.push_transcript_item(TranscriptItem::system(commands::format_context_command(
                &snapshot,
            )));
        }
        DispatchCommand::Reviewer => {
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
        }
        DispatchCommand::Help { topic } => {
            handle_help_command(app, agent, topic.as_deref().unwrap_or(""));
        }
        DispatchCommand::Model => {
            toggle_config_screen(
                app,
                agent,
                Some(squeezy_core::config_schema::SectionId::Models),
            );
        }
        DispatchCommand::Permissions => {
            toggle_config_screen(
                app,
                agent,
                Some(squeezy_core::config_schema::SectionId::Permissions),
            );
        }
        DispatchCommand::Feedback { args } => {
            handle_feedback_command(app, agent, &args).await;
        }
        DispatchCommand::Report { args } => {
            handle_report_command(app, agent, &args).await;
        }
        DispatchCommand::Attach { path } => {
            let original_input = app.input.clone();
            let original_cursor = app.input_cursor;
            clear_input(app);
            match insert_file_prompt_attachment(app, &path) {
                Ok(status) => app.status = status,
                Err(error) => {
                    app.input = original_input;
                    app.input_cursor = original_cursor;
                    app.preserve_input_after_slash_command = true;
                    app.status = format!("attach failed: {error}");
                }
            }
        }
        DispatchCommand::Attachments => {
            app.attachments = agent.context_attachments_snapshot().await;
            if app.attachments.is_empty() {
                set_status_with_notice(
                    app,
                    "no attached context",
                    "No attached context yet. Use `/attach <path>` to add a file or directory token to your next prompt.",
                );
            } else {
                app.status = format!("{} attached context item(s)", app.attachments.len());
                app.push_transcript_item(TranscriptItem::system(format_attachment_list(
                    &app.attachments,
                )));
            }
        }
        DispatchCommand::Detach { id } => match agent.detach_context_attachment(&id).await {
            Ok(attachment) => {
                app.attachments = agent.context_attachments_snapshot().await;
                app.status = format!("detached {}", attachment.id);
                app.push_log(format!("detached context attachment {}", attachment.id));
            }
            Err(error) => {
                set_status_with_notice(
                    app,
                    format!("detach failed: {error}"),
                    format!(
                        "Detach failed: {error}\nRun `/attachments` to see current attachment ids."
                    ),
                );
            }
        },
        DispatchCommand::Compact { undo } => {
            if undo {
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
                        set_status_with_notice(
                            app,
                            "no compaction checkpoint to undo",
                            "No context compaction checkpoint is available to undo.",
                        );
                    }
                    Err(error) => {
                        set_status_notice(app, format!("compact undo failed: {error}"));
                    }
                }
            } else {
                match agent.compact_context_manual().await {
                    Ok(Some(report)) => {
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
                    Ok(None) => {
                        set_status_with_notice(
                            app,
                            "nothing to compact yet",
                            "Nothing to compact yet; the current context is still below the compaction threshold.",
                        );
                    }
                    Err(error) => {
                        set_status_notice(app, format!("compact failed: {error}"));
                    }
                }
            }
        }
        DispatchCommand::Pins => {
            app.context_compaction = agent.context_compaction_snapshot().await;
            if app.context_compaction.pinned.is_empty() {
                set_status_with_notice(
                    app,
                    "no pinned context",
                    "No pinned context yet. Use `/pin selected` or `/pin last` to keep an important transcript item through compaction.",
                );
            } else {
                app.status = format!(
                    "{} pinned context item(s)",
                    app.context_compaction.pinned.len()
                );
                app.push_transcript_item(TranscriptItem::system(format_pin_list(
                    &app.context_compaction,
                )));
            }
        }
        DispatchCommand::Pin { target } => {
            let target_str = target.as_deref().unwrap_or("selected");
            match pin_source(app, target_str) {
                PinSourceResult::Found(label, summary, source) => {
                    match agent.pin_context_entry(label, summary, source).await {
                        Ok(pin) => {
                            app.context_compaction = agent.context_compaction_snapshot().await;
                            app.status = format!("pinned {}", pin.id);
                            app.push_log(format!("pinned context {}", pin.id));
                        }
                        Err(error) => {
                            set_status_notice(app, format!("pin failed: {error}"));
                        }
                    }
                }
                PinSourceResult::NoEntry => {
                    set_status_with_notice(
                        app,
                        "no transcript entry to pin",
                        "No transcript entry is available to pin yet. Select a transcript row first, or run `/pin last` after there is something in the transcript.",
                    );
                }
                PinSourceResult::UnknownTarget => {
                    set_status_notice(app, "usage: /pin selected|last");
                }
            }
        }
        DispatchCommand::Unpin { id } => match agent.unpin_context_entry(&id).await {
            Ok(pin) => {
                app.context_compaction = agent.context_compaction_snapshot().await;
                app.status = format!("unpinned {}", pin.id);
                app.push_log(format!("unpinned context {}", pin.id));
            }
            Err(error) => {
                set_status_with_notice(
                    app,
                    format!("unpin failed: {error}"),
                    format!("Unpin failed: {error}\nRun `/pins` to see current pin ids."),
                );
            }
        },
        DispatchCommand::Diff => handle_slash_diff(app),
        DispatchCommand::Cheap => {
            agent.request_routing_force_cheap();
            app.push_transcript_item(TranscriptItem::system(
                "next turn forced to the cheap model (one-shot)".to_string(),
            ));
            app.status = "routing: forced cheap next turn".to_string();
        }
        DispatchCommand::Parent => {
            agent.request_routing_force_parent();
            app.push_transcript_item(TranscriptItem::system(
                "next turn forced to the parent model (one-shot)".to_string(),
            ));
            app.status = "routing: forced parent next turn".to_string();
        }
        DispatchCommand::Router { value } => handle_slash_router(app, agent, value.as_deref()),
        DispatchCommand::Effort { value } => handle_slash_effort(app, agent, value.as_deref()),
        DispatchCommand::Verbosity { value } => {
            handle_slash_verbosity(app, agent, value.as_deref());
        }
        DispatchCommand::ToolVerbosity { value } => {
            handle_slash_tool_verbosity(app, agent, value.as_deref());
        }
        DispatchCommand::Theme { theme: None } => {
            toggle_config_screen(
                app,
                agent,
                Some(squeezy_core::config_schema::SectionId::Themes),
            );
        }
        DispatchCommand::Theme { theme: Some(theme) } => {
            let Some(parsed) = squeezy_core::normalize_tui_theme_name(&theme) else {
                set_status_with_notice(
                    app,
                    format!("unknown theme {theme:?}; expected a theme slug"),
                    format!(
                        "Unknown theme {theme:?}. Run `/theme` to open theme settings, or use one of: {}.",
                        render::theme::available_theme_names(&agent.config_snapshot()).join(", ")
                    ),
                );
                return;
            };
            if !render::theme::theme_exists(&agent.config_snapshot(), &parsed) {
                let available =
                    render::theme::available_theme_names(&agent.config_snapshot()).join(", ");
                set_status_with_notice(
                    app,
                    format!("unknown theme {theme:?}; available: {available}"),
                    format!("Unknown theme {theme:?}. Available themes: {available}."),
                );
                return;
            }
            apply_theme_change(app, agent, parsed);
        }
        DispatchCommand::Keymap => {
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
        }
        DispatchCommand::Tasks => {
            sync_jobs_from_agent(app, agent);
            let body = format_tasks_list(app, agent);
            app.status = if app.jobs.is_empty() {
                "no background tasks".to_string()
            } else {
                format!("{} tasks", app.jobs.len())
            };
            app.push_transcript_item(TranscriptItem::system(body));
        }
        DispatchCommand::Task { id } => {
            apply_task_detail(app, agent, &id);
        }
        DispatchCommand::TaskCancel { id } => {
            apply_task_cancel(app, agent, &id);
        }
        DispatchCommand::Sessions => match agent.list_sessions(&SessionQuery::default()) {
            Ok(sessions) => {
                app.status = format!("{} sessions", sessions.len());
                let body = if sessions.is_empty() {
                    "No saved sessions found yet.".to_string()
                } else {
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
                        .join("\n")
                };
                app.push_transcript_item(TranscriptItem::system(body));
            }
            Err(error) => set_status_notice(app, format!("session list failed: {error}")),
        },
        DispatchCommand::Session { id } => match agent.show_session(&id) {
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
            Err(error) => {
                set_status_with_notice(
                    app,
                    format!("session show failed: {error}"),
                    format!(
                        "Session lookup failed: {error}\nRun `/sessions` to see recent session ids."
                    ),
                );
            }
        },
        DispatchCommand::SessionRename { name } => {
            let parameter = if name.trim().is_empty() {
                None
            } else {
                Some(name.clone())
            };
            match agent.set_session_display_name(parameter) {
                Ok(metadata) => match metadata.display_name {
                    Some(display) => {
                        app.status = format!("renamed session → {display}");
                        app.push_transcript_item(TranscriptItem::system(format!(
                            "session {} renamed to {display}",
                            metadata.session_id
                        )));
                    }
                    None => {
                        app.status = format!("cleared session name ({})", metadata.session_id);
                        app.push_transcript_item(TranscriptItem::system(format!(
                            "session {} display_name cleared",
                            metadata.session_id
                        )));
                    }
                },
                Err(error) => set_status_notice(app, format!("rename failed: {error}")),
            }
        }
        DispatchCommand::SessionLabel { name } => match agent.add_session_label(name.clone()) {
            Ok((metadata, added)) => {
                let label_list = if metadata.labels.is_empty() {
                    "(none)".to_string()
                } else {
                    metadata.labels.join(", ")
                };
                if added {
                    app.status = format!("labelled session #{name}");
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "session {} labels: {label_list}",
                        metadata.session_id
                    )));
                } else {
                    set_status_notice(app, format!("label #{name} already on session"));
                }
            }
            Err(error) => set_status_notice(app, format!("label failed: {error}")),
        },
        DispatchCommand::Fork => match agent.fork_current().await {
            Ok(new_id) => {
                app.status = format!("forked session → {new_id}");
                app.push_transcript_item(TranscriptItem::system(format!(
                    "/fork started session {new_id}; the original session is saved and \
                     remains resumable via /resume."
                )));
            }
            Err(error) => app.status = format!("fork failed: {error}"),
        },
        DispatchCommand::Clear => match agent.clear_conversation().await {
            Ok(new_session) => {
                // Drop the visible transcript and any in-flight render
                // state so the screen matches the now-empty conversation.
                // `/clear` is refused mid-turn, so the streaming/cancel
                // fields are already idle; clearing them is defensive.
                app.transcript.clear();
                app.selected_entry = None;
                app.next_entry_id = 0;
                app.attachments = agent.context_attachments_snapshot().await;
                app.pending_assistant.clear();
                app.pending_reasoning.clear();
                app.task_state = None;
                app.task_panel_collapsed = false;
                app.turn_rx = None;
                app.cancel = None;
                app.subagent_pane = SubagentPaneState {
                    next_synthetic_id: app.subagent_pane.next_synthetic_id,
                    ..SubagentPaneState::default()
                };
                app.toasts.clear();
                let note = match new_session {
                    Some(new_id) => format!(
                        "Conversation cleared. The previous conversation is saved and remains \
                         resumable via /resume; this is now session {new_id}."
                    ),
                    None => "Conversation cleared.".to_string(),
                };
                app.status = "conversation cleared".to_string();
                app.push_transcript_item(TranscriptItem::system(note));
            }
            Err(error) => app.status = format!("clear failed: {error}"),
        },
        DispatchCommand::Resume { id } => switch_to_session(app, agent, &id).await,
        DispatchCommand::SessionExport { id } => match agent.export_session(&id) {
            Ok(value) => {
                let bytes = serde_json::to_string(&value).map_or(0, |text| text.len());
                app.status = format!("session export {} bytes", bytes);
                app.push_transcript_item(TranscriptItem::system(format!(
                    "Session export for `{id}` is ready ({bytes} bytes)."
                )));
            }
            Err(error) => set_status_notice(app, format!("session export failed: {error}")),
        },
        DispatchCommand::SessionExportHtml { id, path } => {
            let target = path
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("squeezy-session-{id}.html")));
            match agent.show_session(&id).and_then(|record| {
                squeezy_agent::export_session_to_html(
                    &record,
                    &squeezy_agent::ExportOpts::default(),
                )
                .map_err(|err| {
                    squeezy_core::SqueezyError::Tool(format!("failed to render html: {err}"))
                })
                .and_then(|html| {
                    std::fs::write(&target, &html).map_err(squeezy_core::SqueezyError::from)?;
                    Ok(html.len())
                })
            }) {
                Ok(len) => {
                    app.status = format!("wrote {} ({} bytes)", target.display(), len);
                    app.push_transcript_item(TranscriptItem::system(format!(
                        "Wrote session export HTML to {} ({} bytes).",
                        target.display(),
                        len
                    )));
                }
                Err(error) => {
                    set_status_notice(app, format!("session export html failed: {error}"))
                }
            }
        }
        DispatchCommand::Checkpoints => {
            start_local_checkpoint_job(app, agent, "checkpoint_list", serde_json::json!({}))
        }
        DispatchCommand::Undo => {
            start_local_checkpoint_job(app, agent, "checkpoint_undo", serde_json::json!({}))
        }
        DispatchCommand::Checkpoint { id } => start_local_checkpoint_job(
            app,
            agent,
            "checkpoint_show",
            serde_json::json!({ "checkpoint_id": id }),
        ),
        DispatchCommand::RevertTurn { group_id } => start_local_checkpoint_job(
            app,
            agent,
            "checkpoint_revert",
            serde_json::json!({ "group_id": group_id }),
        ),
    }
}

fn apply_task_detail(app: &mut TuiApp, agent: &Agent, raw_id: &str) {
    let Some(id) = parse_job_id(raw_id) else {
        set_status_notice(app, "task id must be a number");
        return;
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
        None => set_status_with_notice(
            app,
            format!("task {id} not found"),
            format!("Task {id} was not found. Run `/tasks` to list background tasks."),
        ),
    }
}

fn apply_task_cancel(app: &mut TuiApp, agent: &Agent, raw_id: &str) {
    let Some(id) = parse_job_id(raw_id) else {
        set_status_notice(app, "task id must be a number");
        return;
    };
    if agent.cancel_job(id) {
        app.status = format!("cancelling task {id}");
        app.push_log(format!("cancelling task {id}"));
        sync_jobs_from_agent(app, agent);
    } else {
        set_status_with_notice(
            app,
            format!("task {id} not active"),
            format!(
                "Task {id} is not active or does not exist. Run `/tasks` to list background tasks."
            ),
        );
    }
}

fn start_local_checkpoint_job(
    app: &mut TuiApp,
    agent: &Agent,
    name: &'static str,
    arguments: serde_json::Value,
) {
    let job = agent.start_local_tool_job(ToolCall {
        call_id: format!("tui-{name}"),
        name: name.to_string(),
        arguments,
    });
    app.jobs.insert(job.id, job.clone());
    app.status = format!("started job {} {}", job.id, job.title);
    app.push_log(format!("started job {} {}", job.id, job.title));
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
        set_status_with_notice(
            app,
            "usage: /plans <subcommand> <id-or-prefix>",
            format!(
                "Missing plan id.\n{}\n\nRun `/plans` to list saved plans in this session.",
                plans_usage()
            ),
        );
        return None;
    };
    match proposed_plan::resolve_plan_prefix(&app.workspace_root, sid, needle) {
        Ok(plan_id) => Some(plan_id),
        Err(proposed_plan::PlanLookupError::NotFound) => {
            let entries = proposed_plan::list_plans(&app.workspace_root, sid);
            let notice = if entries.is_empty() {
                format!(
                    "No plan matches `{needle}` because this session has no saved plans yet.\n\
                     Plans are saved when Plan mode produces a completed `<proposed_plan>` block."
                )
            } else {
                format!(
                    "No plan matches `{needle}` in this session.\nRun `/plans` to list available plan ids."
                )
            };
            set_status_with_notice(
                app,
                format!("no plan matches `{needle}` in this session"),
                notice,
            );
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
        set_status_with_notice(
            app,
            "no plans persisted in this session",
            "No plans saved in this session yet.\nPlans are saved when Plan mode produces a completed `<proposed_plan>` block.",
        );
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
        Err(err) => set_status_notice(app, format!("plans show failed: {err}")),
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
        Err(err) => set_status_notice(app, format!("plans delete failed: {err}")),
    }
}

fn plans_set_active(app: &mut TuiApp, sid: &str, plan_id: &str) {
    match proposed_plan::set_active_plan(&app.workspace_root, sid, plan_id) {
        Ok(()) => {
            app.current_plan_id = Some(plan_id.to_string());
            app.status = format!("active plan → {plan_id}");
            app.push_log(format!("set active plan: {plan_id}"));
        }
        Err(err) => set_status_notice(app, format!("plans set-active failed: {err}")),
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
            submit_pending_feedback(app, agent).await;
        }
        "cancel" => {
            discard_pending_feedback(app);
        }
        "" => {
            set_status_with_notice(
                app,
                "usage: /feedback <what happened>",
                "Usage: `/feedback <what happened>` previews maintainer feedback before sending.",
            );
        }
        message => match agent.prepare_feedback(message) {
            Ok(feedback) => {
                let preview = format!(
                    "feedback preview\nfeedback_id={}\nbytes={} redactions={}\n\n{}\n\nPress Enter to send or Esc to discard.",
                    feedback.feedback_id,
                    feedback.message_bytes,
                    feedback.redactions,
                    feedback.message
                );
                app.pending_feedback = Some(feedback);
                app.status = "feedback ready: Enter send · Esc discard".to_string();
                app.push_transcript_item(TranscriptItem::system(preview));
            }
            Err(error) => set_status_notice(app, format!("feedback preview failed: {error}")),
        },
    }
}

async fn submit_pending_feedback(app: &mut TuiApp, agent: &Agent) {
    let Some(feedback) = app.pending_feedback.take() else {
        set_status_with_notice(
            app,
            "no feedback pending",
            "No feedback preview is pending. Run `/feedback <what happened>` first.",
        );
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
            set_status_notice(app, format!("feedback send failed: {error}"));
        }
    }
}

fn discard_pending_feedback(app: &mut TuiApp) {
    app.pending_feedback = None;
    app.status = "feedback discarded".to_string();
}

async fn handle_report_command(app: &mut TuiApp, agent: &Agent, rest: &str) {
    match rest {
        "send" => {
            let Some(report) = app.pending_report.take() else {
                set_status_with_notice(
                    app,
                    "no report pending",
                    "No bug report preview is pending. Run `/report` first.",
                );
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
                    set_status_notice(app, format!("report send failed: {error}"));
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
                        set_status_notice(app, error);
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
                Err(error) => set_status_notice(app, format!("report preview failed: {error}")),
            }
        }
    }
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
            .or_else(|| last_pinnable_entry(app)),
        "last" => last_pinnable_entry(app),
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

fn last_pinnable_entry(app: &TuiApp) -> Option<&TranscriptEntry> {
    app.transcript
        .iter()
        .rev()
        .find(|entry| !matches!(entry.kind, TranscriptEntryKind::SlashEcho(_)))
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

fn copy_last_assistant_to_clipboard(app: &mut TuiApp) {
    let Some(text) = last_assistant_clipboard_text(app) else {
        app.status = "nothing to copy yet".to_string();
        return;
    };
    match app.clipboard.copy_text(&text) {
        Ok(()) => {
            app.status = format!("copied assistant message ({} chars)", text.chars().count());
        }
        Err(error) => {
            app.status = format!("copy failed: {error}");
        }
    }
}

fn last_assistant_clipboard_text(app: &TuiApp) -> Option<String> {
    if !app.pending_assistant.trim_is_empty() {
        return Some(app.pending_assistant.text());
    }
    app.transcript
        .iter()
        .rev()
        .find_map(TranscriptEntry::assistant_content)
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
                set_status_notice(
                    app,
                    format!("unknown effort {raw:?}; expected low, medium, high, xhigh, or auto"),
                );
                return;
            }
        },
    };
    let mut next = agent.config_snapshot();
    next.reasoning_effort = next_effort;
    agent.replace_config(next);
    app.reasoning_effort = next_effort;
    let label = next_effort.map_or_else(
        || "auto (model default)".to_string(),
        |e| e.as_str().to_string(),
    );
    // squeezy-a19z (audit U1): status line only; the previous
    // app_notification push duplicated feedback and diverged from
    // /verbosity / /tool-verbosity, which only used notifications.
    // Unify on the status line — it's the immediate-feedback surface
    // for these session-scoped switches.
    app.status = format!("reasoning effort → {label}");
    if std::env::var("SQUEEZY_REASONING_EFFORT").is_ok() {
        app.app_notifications.push(
            "SQUEEZY_REASONING_EFFORT overrides this on next load".to_string(),
            NotifySeverity::Warn,
        );
    }
}

/// `/router [on|off]`. Bare prints the current state and usage hint.
/// `on` re-enables session-wide auto-routing to the cheap tier; `off`
/// disables it (explicit `/cheap` still works). The toggle is one-shot
/// at the override level — the user's persisted `[routing].auto_cheap`
/// config is not touched.
fn handle_slash_router(app: &mut TuiApp, agent: &mut Agent, value: Option<&str>) {
    let snapshot = agent.config_snapshot();
    let config_default_on = snapshot.routing.auto_cheap;
    let Some(raw) = value else {
        let state = if config_default_on {
            "enabled"
        } else {
            "disabled (config default)"
        };
        app.status = format!("routing: {state}");
        app.push_transcript_item(TranscriptItem::system(format!(
            "routing = {state}\nusage: /router [on|off]"
        )));
        return;
    };
    let disabled = match raw.trim().to_ascii_lowercase().as_str() {
        "on" | "enable" | "enabled" | "true" | "1" => false,
        "off" | "disable" | "disabled" | "false" | "0" => true,
        _ => {
            set_status_notice(
                app,
                format!("unknown router state {raw:?}; expected on or off"),
            );
            return;
        }
    };
    agent.set_routing_session_disabled(disabled);
    app.status = format!(
        "routing → {}",
        if disabled { "disabled" } else { "enabled" }
    );
}

/// `/verbosity [concise|normal|verbose]`. Bare prints the current
/// value and usage hint into the transcript (matches `/effort`'s
/// surface); with an explicit value, sets `tui.response_verbosity`
/// and reports via the status line. Previously the bare form
/// short-circuited to the `/config` config_screen — surprising
/// mode-switch on argument presence (squeezy-3ys0 / audit U2).
fn handle_slash_verbosity(app: &mut TuiApp, agent: &mut Agent, value: Option<&str>) {
    let Some(raw) = value else {
        let current = agent.config_snapshot().tui.response_verbosity;
        app.status = format!("response verbosity: {}", current.as_str());
        app.push_transcript_item(TranscriptItem::system(format!(
            "response verbosity = {}\nusage: /verbosity [concise|normal|verbose]",
            current.as_str()
        )));
        return;
    };
    let Some(verbosity) = parse_response_verbosity(raw) else {
        set_status_notice(
            app,
            format!("unknown response verbosity {raw:?}; expected concise, normal, or verbose"),
        );
        return;
    };
    app.response_verbosity = verbosity;
    let mut next = agent.config_snapshot();
    next.tui.response_verbosity = verbosity;
    agent.replace_config(next);
    app.status = format!("response verbosity → {}", verbosity.as_str());
}

/// `/tool-verbosity [compact|normal|verbose]`. Same shape as
/// [`handle_slash_verbosity`] — bare prints + usage hint, with-arg
/// sets and reports via the status line. squeezy-a19z + squeezy-3ys0.
fn handle_slash_tool_verbosity(app: &mut TuiApp, agent: &mut Agent, value: Option<&str>) {
    let Some(raw) = value else {
        let current = agent.config_snapshot().tui.tool_output_verbosity;
        app.status = format!("tool output verbosity: {}", current.as_str());
        app.push_transcript_item(TranscriptItem::system(format!(
            "tool output verbosity = {}\nusage: /tool-verbosity [compact|normal|verbose]",
            current.as_str()
        )));
        return;
    };
    let Some(verbosity) = parse_tool_output_verbosity(raw) else {
        set_status_notice(
            app,
            format!("unknown tool output verbosity {raw:?}; expected compact, normal, or verbose"),
        );
        return;
    };
    app.tool_output_verbosity = verbosity;
    let mut next = agent.config_snapshot();
    next.tui.tool_output_verbosity = verbosity;
    agent.replace_config(next);
    app.status = format!("tool output verbosity → {}", verbosity.as_str());
}

#[cfg(test)]
fn set_all_transcript_collapsed(app: &mut TuiApp, collapsed: bool) -> usize {
    let mut changed = 0;
    for entry in &mut app.transcript {
        if entry.collapsed != collapsed {
            entry.collapsed = collapsed;
            entry.bump_revision();
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
    const PAGE: usize = 10;
    match key.code {
        KeyCode::Esc => {
            app.transcript_overlay_scrollbar_cache.set(None);
            app.transcript_overlay = None;
            app.status = "transcript overlay closed".to_string();
            true
        }
        KeyCode::PageUp => {
            adjust_transcript_overlay_scroll(app, |scroll| scroll.saturating_sub(PAGE));
            true
        }
        KeyCode::PageDown => {
            adjust_transcript_overlay_scroll(app, |scroll| scroll.saturating_add(PAGE));
            true
        }
        KeyCode::Up => {
            adjust_transcript_overlay_scroll(app, |scroll| scroll.saturating_sub(1));
            true
        }
        KeyCode::Down => {
            adjust_transcript_overlay_scroll(app, |scroll| scroll.saturating_add(1));
            true
        }
        KeyCode::Home => {
            set_transcript_overlay_scroll(app, 0);
            true
        }
        KeyCode::End => {
            let end =
                transcript_overlay_max_scroll(app).unwrap_or(TRANSCRIPT_OVERLAY_SCROLL_BOTTOM);
            set_transcript_overlay_scroll(app, end);
            true
        }
        KeyCode::Char('m') | KeyCode::Char('M')
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::META) =>
        {
            toggle_transcript_overlay_mouse_capture(app);
            true
        }
        _ => true, // swallow everything else so the overlay stays modal
    }
}

fn toggle_transcript_overlay_mouse_capture(app: &mut TuiApp) {
    if let Some(state) = app.transcript_overlay.as_mut() {
        state.mode = match state.mode {
            TranscriptOverlayMode::NativeSelection => TranscriptOverlayMode::ScrollbarDrag,
            TranscriptOverlayMode::ScrollbarDrag => TranscriptOverlayMode::NativeSelection,
        };
        app.status = if state.mode.mouse_capture() {
            "transcript scrollbar drag on (Shift-drag selects text)"
        } else {
            "transcript native selection mode"
        }
        .to_string();
    }
}

fn handle_prompt_queue_overlay_key(app: &mut TuiApp, key: KeyEvent) -> bool {
    let Some(state) = app.prompt_queue_overlay.as_mut() else {
        return false;
    };
    match state.dispatch(&mut app.prompt_queue, key) {
        prompt_queue::QueueDispatch::Handled => true,
        prompt_queue::QueueDispatch::Close => {
            app.prompt_queue_overlay = None;
            app.status = "prompt queue closed".to_string();
            true
        }
        prompt_queue::QueueDispatch::Ignored => true, // stay modal
    }
}

/// Kick off a user-driven turn. Drains any pending config swap, consumes a
/// queued plan handoff (prepending the plan body to `input`), and hands
/// the resulting prompt to the agent. Used by the Enter key handler and
/// by the post-plan Execute action so both paths share the same plan
/// prefix and turn-state bookkeeping.
pub(crate) fn start_user_turn(app: &mut TuiApp, agent: &mut Agent, input: String) {
    let prompt = prepare_prompt_turn_input(app, input);
    start_user_turn_prepared(app, agent, prompt);
}

#[derive(Debug, Clone)]
struct PreparedPromptTurn {
    display_input: String,
    model_input: String,
    transient_input_items: Vec<LlmInputItem>,
}

fn prepare_prompt_turn_input(app: &mut TuiApp, input: String) -> PreparedPromptTurn {
    app.prune_prompt_attachments();
    let mut model_input = input.clone();
    let mut transient_input_items = Vec::new();
    for attachment in app.prompt_attachments.clone() {
        if !input.contains(&attachment.placeholder) {
            continue;
        }
        match attachment.payload {
            PromptAttachmentPayload::Text { replacement } => {
                model_input = model_input.replace(&attachment.placeholder, &replacement);
            }
            PromptAttachmentPayload::Image { media_type, bytes } => {
                transient_input_items.push(LlmInputItem::Image { media_type, bytes });
            }
        }
    }
    PreparedPromptTurn {
        display_input: input,
        model_input,
        transient_input_items,
    }
}

fn start_user_turn_prepared(app: &mut TuiApp, agent: &mut Agent, prompt: PreparedPromptTurn) {
    if let Some(swap) = agent.drain_pending_swap() {
        app.provider_name = swap
            .provider
            .as_ref()
            .map(|provider| provider.name())
            .unwrap_or_else(|| squeezy_llm::provider_name(&swap.config.provider));
        app.apply_config_change(&swap.config);
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
    let (display_input, model_input) = match take_pending_plan_prefix(app) {
        Some(prefix) => (
            format!("{prefix}{}", prompt.display_input),
            format!("{prefix}{}", prompt.model_input),
        ),
        None => (prompt.display_input, prompt.model_input),
    };
    app.turn_rx = Some(agent.start_turn_with_display_input(
        display_input,
        model_input,
        prompt.transient_input_items,
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
/// Strip the plan-handoff prefix that [`take_pending_plan_prefix`] prepended
/// to the user-typed text. The agent still receives the full wrapped body —
/// this only hides the duplicate echo in the rendered transcript, because the
/// Plan card a few entries above already shows the same body. Returns `None`
/// when the message has no plan-handoff prefix.
pub(crate) fn strip_plan_handoff_prefix(content: &str) -> Option<String> {
    let mut rest = content;
    let mut changed = false;

    if let Some(after) = rest.strip_prefix("[resuming from plan ")
        && let Some(end) = after.find("]\n")
    {
        rest = &after[end + 2..];
        changed = true;
    }

    if let Some(after) = rest.strip_prefix("[plan from previous session — ")
        && let Some(header_end) = after.find("]\n")
    {
        let body_start = &after[header_end + 2..];
        if let Some(close) = body_start.find("\n[end plan]\n") {
            let mut tail = &body_start[close + "\n[end plan]\n".len()..];
            tail = tail.strip_prefix('\n').unwrap_or(tail);
            rest = tail;
            changed = true;
        }
    } else if let Some(after) = rest.strip_prefix("[plan still in effect — ")
        && let Some(end) = after.find("]\n")
    {
        let mut tail = &after[end + 2..];
        tail = tail.strip_prefix('\n').unwrap_or(tail);
        rest = tail;
        changed = true;
    }

    if changed {
        Some(rest.to_string())
    } else {
        None
    }
}

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
        app.push_status("plan execution paused (Shift+Tab)".to_string());
    }

    if !is_build_to_plan_pause
        && (app.turn_rx.is_some()
            || app.pending_approval.is_some()
            || app.pending_mcp_elicitation.is_some()
            || app.pending_request_user_input.is_some()
            || app.pending_feedback.is_some())
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
                        app.push_status(format!(
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

/// Build the per-capability allow/deny menu for a pending approval. The
/// menu keeps the obvious one-shot decision first, then offers one
/// project-scoped allow rule for users who want to persist the decision.
fn approval_options_for(request: &ToolApprovalRequest) -> Vec<ApprovalOption> {
    let (project_label, project_hint) = capability_project_label(request);
    let project = ApprovalOption {
        choice: ApprovalChoice::ApproveProject,
        label: project_label,
        hint: project_hint,
        decision: ToolApprovalDecision::AllowRuleProject,
    };
    vec![approval_once(), project, approval_deny()]
}

/// Returns `(project_label, project_hint)` for the persistent allow option.
/// Each label names the capability-specific target (binary, host, MCP
/// server/tool, write root) so the prompt makes the persisted rule shape
/// visible without forcing the user to read the rule-preview line.
fn capability_project_label(
    request: &ToolApprovalRequest,
) -> (
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
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " · {} · {}",
                request.server,
                mcp_elicitation_kind_label(&request.kind)
            ),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ])];
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            compact_text(&request.message, 180),
            Style::default()
                .fg(crate::render::theme::magenta())
                .add_modifier(Modifier::BOLD),
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
                        Style::default().fg(crate::render::theme::quiet()),
                    ),
                ]));
            }
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("response {}", mcp_elicitation_response_preview(input)),
                    Style::default().fg(crate::render::theme::quiet()),
                ),
            ]));
        }
        McpElicitationKind::Url => {
            if let Some(url) = request.url.as_ref() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        compact_text(url, 180),
                        Style::default().fg(crate::render::theme::quiet()),
                    ),
                ]));
            }
        }
    }
    for (index, option) in mcp_elicitation_options().iter().enumerate() {
        let is_selected = index == selected.min(mcp_elicitation_options().len() - 1);
        let marker = if is_selected { "› " } else { "  " };
        let label_style = if is_selected {
            Style::default().fg(crate::render::theme::secondary())
        } else {
            Style::default().fg(palette::muted_fg())
        };
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if is_selected {
                    crate::render::theme::secondary()
                } else {
                    crate::render::theme::quiet()
                }),
            ),
            Span::styled(option.label, label_style),
            Span::styled(
                format!(" · {}", option.hint),
                Style::default().fg(crate::render::theme::quiet()),
            ),
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
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", pending.plan_id),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ])];
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            compact_path(&pending.plan_path),
            Style::default().fg(palette::muted_fg()),
        ),
    ]));
    for (idx, option) in PLAN_CHOICES.iter().enumerate() {
        let is_selected = idx == selected;
        let marker = if is_selected { "› " } else { "  " };
        let label_style = if is_selected {
            Style::default().fg(crate::render::theme::secondary())
        } else {
            Style::default().fg(palette::muted_fg())
        };
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if is_selected {
                    crate::render::theme::secondary()
                } else {
                    crate::render::theme::quiet()
                }),
            ),
            Span::styled(
                format!("[{}] {}", option.shortcut, option.label),
                label_style,
            ),
            Span::styled(
                format!(" · {}", option.hint),
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]));
    }
    lines
}

fn format_feedback_prompt_lines(feedback: &PreparedFeedback) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                "Send feedback?",
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {}", feedback.feedback_id),
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                compact_text(&feedback.message, 160),
                Style::default().fg(palette::muted_fg()),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "› Enter/Y Send",
                Style::default().fg(crate::render::theme::secondary()),
            ),
            Span::styled(" · ", Style::default().fg(crate::render::theme::quiet())),
            Span::styled("Esc/N Discard", Style::default().fg(palette::muted_fg())),
        ]),
    ]
}

fn format_request_user_input_menu_lines(
    request: &RequestUserInputRequest,
    selected: usize,
    input: &str,
) -> Vec<Line<'static>> {
    let mut lines = vec![{
        let mut spans = vec![Span::styled(
            "Plan-mode question",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        )];
        if request.allow_freeform {
            spans.push(Span::styled(
                " · freeform allowed",
                Style::default().fg(crate::render::theme::quiet()),
            ));
        }
        Line::from(spans)
    }];
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            compact_text(&request.question, 240),
            Style::default()
                .fg(crate::render::theme::magenta())
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    for (index, choice) in request.choices.iter().enumerate() {
        let is_selected = index == selected && selected < request.choices.len();
        let marker = if is_selected { "● " } else { "  " };
        let label_style = if is_selected {
            Style::default()
                .fg(palette::footer_fg())
                .add_modifier(Modifier::BOLD)
        } else {
            // Tone-aware muted grey sits below the luminance budget so
            // the amber selection dot carries focus without turning the
            // label itself yellow.
            Style::default().fg(palette::muted_fg())
        };
        let mut spans = vec![
            Span::styled(
                marker,
                Style::default().fg(if is_selected {
                    crate::render::theme::accent()
                } else {
                    crate::render::theme::quiet()
                }),
            ),
            Span::styled(compact_text(&choice.label, 180), label_style),
        ];
        if choice.value != choice.label {
            spans.push(Span::styled(
                format!(" · {}", compact_text(&choice.value, 120)),
                Style::default().fg(crate::render::theme::quiet()),
            ));
        }
        lines.push(Line::from(spans));
    }
    if request.allow_freeform {
        // Dedicated answer-entry box. Lives inside the modal area so the
        // main composer below stays untouched for the user's next prompt.
        // Label + cursor share the `crate::render::theme::magenta()` warm-taupe accent so the
        // whole modal reads as one semantic surface; the typed body uses
        // a dim+bold tone-aware foreground for legibility without
        // overpowering the question line.
        let is_selected = selected >= request.choices.len();
        let marker = if is_selected { "● " } else { "  " };
        let marker_style = Style::default().fg(if is_selected {
            crate::render::theme::accent()
        } else {
            crate::render::theme::quiet()
        });
        let entry_style = Style::default()
            .fg(palette::footer_fg())
            .add_modifier(Modifier::BOLD);
        let label_style = Style::default().fg(crate::render::theme::magenta());
        let cursor_style = Style::default()
            .fg(Color::Black)
            .bg(crate::render::theme::magenta());
        let mut spans = vec![
            Span::styled(marker, marker_style),
            Span::styled("Answer › ", label_style),
        ];
        if input.is_empty() {
            spans.push(Span::styled(
                "(type your answer · Enter sends when selected)",
                Style::default().fg(crate::render::theme::quiet()),
            ));
        } else {
            // Render the answer with an inline cursor block. The cursor
            // sits at `answer_cursor` bytes, which we don't have here —
            // approximate by drawing the whole answer followed by a
            // block. Accurate cursor placement is the caller's job; for
            // now this gives the user a visible "I'm typing in the right
            // box" affordance.
            spans.push(Span::styled(compact_text(input, 200), entry_style));
            spans.push(Span::styled("▌", cursor_style));
        }
        lines.push(Line::from(spans));
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
            Style::default().fg(crate::render::theme::secondary())
        } else {
            Style::default().fg(palette::muted_fg())
        };
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if is_selected {
                    crate::render::theme::secondary()
                } else {
                    crate::render::theme::quiet()
                }),
            ),
            Span::styled(option.label.to_string(), label_style),
            Span::styled(
                format!(" · {}", option.hint),
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]));
    }
    lines
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &TuiApp) {
    app.begin_frame_clickables();
    let area = frame.area();
    if app.transcript_overlay.is_some() {
        render_transcript_overlay_surface(frame, area, app);
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
    let approval_height = approval_menu_height(app, area.width);
    let plan_indicator_height = plan_mode_indicator_height(app);
    let subagent_height = subagent_pane_height(app);
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
        .saturating_add(subagent_height)
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
    if subagent_height > 0 {
        constraints.push(Constraint::Length(subagent_height));
    }
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
    if subagent_height > 0 {
        render_subagent_pane(frame, chunks[index], app);
        index += 1;
    }
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
        Span::styled(
            current.message.as_str(),
            Style::default().fg(palette::muted_fg()),
        ),
    ];
    if let Some(hint) = current.action_hint {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            hint,
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    if app.app_notifications.len() > 1 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("({}+)", app.app_notifications.len() - 1),
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("· {remaining_secs}s"),
        Style::default().fg(crate::render::theme::quiet()),
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

pub(crate) fn render_inline(frame: &mut Frame<'_>, app: &TuiApp) {
    app.begin_frame_clickables();
    let area = frame.area();
    if app.transcript_overlay.is_some() {
        render_transcript_overlay_surface(frame, area, app);
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
    let approval_height = approval_menu_height(app, area.width);
    let plan_indicator_height = plan_mode_indicator_height(app);
    let task_height = should_show_task_panel(app).then_some(task_panel_height(app));
    let status_height = 2;
    let subagent_height = subagent_pane_height(app);
    let live_lines = pending_assistant_lines(app);
    let live_visual_height = visual_line_count(&live_lines, area.width);
    let live_gap = if live_visual_height > 0 { 1 } else { 0 };
    let required_height = task_height
        .unwrap_or(0)
        .saturating_add(input_height)
        .saturating_add(approval_height)
        .saturating_add(plan_indicator_height)
        .saturating_add(status_height)
        .saturating_add(subagent_height)
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
    if subagent_height > 0 {
        constraints.push(Constraint::Length(subagent_height));
    }
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
    index += 1;
    if subagent_height > 0 {
        render_subagent_pane(frame, chunks[index], app);
    }
    render_toast_overlay(frame, area, app);
}

fn transcript_prompt_gap_height(app: &TuiApp) -> u16 {
    if active_transcript_entries(app).is_empty()
        && (active_subagent_record(app).is_some() || app.pending_assistant.is_empty())
    {
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
        .style(Style::default().fg(crate::render::theme::quiet()))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// Detail row rendered below the spinner when there is something
/// actionable to show. Returns `None` when the spinner alone suffices —
/// keeps the working cell single-row in the common case.
fn working_detail_line(app: &TuiApp) -> Option<Line<'static>> {
    // Highest priority: a `/diff` snapshot is running on the blocking
    // pool. The user typed `/diff` and is waiting for git to finish.
    if app.pending_diff.is_some() {
        return Some(Line::from(Span::styled(
            "    ↳ computing diff…",
            Style::default().fg(crate::render::theme::quiet()),
        )));
    }
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
            return Some(Line::from(Span::styled(
                text,
                Style::default().fg(crate::render::theme::quiet()),
            )));
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
        return Some(Line::from(Span::styled(
            text,
            Style::default().fg(crate::render::theme::quiet()),
        )));
    }
    None
}

fn turn_in_progress(app: &TuiApp) -> bool {
    app.turn_rx.is_some()
        || app.cancel.is_some()
        || app.pending_diff.is_some()
        || (app.last_turn_duration.is_none()
            && app
                .task_state
                .as_ref()
                .is_some_and(|snapshot| snapshot.status == squeezy_core::TaskStateStatus::Running))
}

fn should_advance_animation_tick(app: &TuiApp) -> bool {
    app.focused || turn_in_progress(app)
}

fn working_line(app: &TuiApp) -> Line<'static> {
    let interrupting = app.status == "interrupting";
    // The live agent is a cool-silver star (the moon motif is reserved
    // for the header band and the prompt coin); red only when interrupting.
    let activity_color = if interrupting {
        crate::render::theme::red()
    } else {
        crate::render::theme::foreground()
    };
    let spinner_frame = crate::render::spinner::active_style().frame(prompt_elapsed_ms(app));
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            format!("{spinner_frame} "),
            Style::default()
                .fg(activity_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(if interrupting {
        vec![Span::styled(
            "Interrupting",
            Style::default()
                .fg(crate::render::theme::red())
                .add_modifier(Modifier::BOLD),
        )]
    } else {
        working_word_spans(app)
    });
    spans.push(Span::styled(
        format!(
            " ({} • esc to interrupt)",
            format_turn_duration(current_turn_duration(app))
        ),
        Style::default().fg(crate::render::theme::quiet()),
    ));
    if let Some(call) = app
        .active_tool_calls
        .values()
        .find(|call| !is_control_tool_name(&call.name))
    {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
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
        .or(app.pending_diff_started_at)
        .map(|started_at| started_at.elapsed())
        .unwrap_or_default()
}

fn worked_divider_line(duration: Duration, width: u16) -> Line<'static> {
    let label = format!("─ Worked for {} ", format_turn_duration(duration));
    let label_width = label.chars().count();
    let fill_width = (width as usize).saturating_sub(label_width);
    let mut text = label;
    text.push_str(&"─".repeat(fill_width));
    Line::from(Span::styled(
        text,
        Style::default().fg(crate::render::theme::quiet()),
    ))
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
        squeezy_core::TaskStateStatus::Running => ("Working", crate::render::theme::accent()),
        squeezy_core::TaskStateStatus::Blocked => ("Blocked", crate::render::theme::secondary()),
        squeezy_core::TaskStateStatus::Completed => ("Done", crate::render::theme::green()),
        squeezy_core::TaskStateStatus::Cancelled => ("Cancelled", crate::render::theme::red()),
        squeezy_core::TaskStateStatus::Failed => ("Failed", crate::render::theme::red()),
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
        spans.push(Span::styled(
            detail,
            Style::default().fg(crate::render::theme::quiet()),
        ));
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

fn approval_menu_height(app: &TuiApp, width: u16) -> u16 {
    // The modal renders with `Wrap { trim: false }`, so a single logical
    // line can occupy multiple visible rows when its content exceeds the
    // area width. Counting `lines.len()` under-allocates and pushes the
    // freeform answer box (or the lowest choice) off the modal area on
    // long verbose labels — see squeezy-xtvg / wave2-06 Anthropic run.
    // `visual_line_count` mirrors the wrap pass used for the transcript
    // and live regions and is safe (it rounds up per line, never down).
    if let Some(pending) = app.pending_approval.as_ref() {
        visual_line_count(
            &format_approval_menu_lines(&pending.request, app.approval_selection_index),
            width,
        )
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
        visual_line_count(
            &format_request_user_input_menu_lines(
                &pending.request,
                pending.selection_index,
                &pending.answer,
            ),
            width,
        )
    } else if let Some(pending) = app.pending_plan_choice.as_ref() {
        visual_line_count(&format_plan_choice_menu_lines(pending), width)
    } else if let Some(feedback) = app.pending_feedback.as_ref() {
        visual_line_count(&format_feedback_prompt_lines(feedback), width)
    } else {
        0
    }
}

fn render_approval(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph = Paragraph::new(approval_lines(app))
        .style(Style::default().fg(crate::render::theme::quiet()))
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
        format_request_user_input_menu_lines(
            &pending.request,
            pending.selection_index,
            &pending.answer,
        )
    } else if let Some(pending) = app.pending_plan_choice.as_ref() {
        format_plan_choice_menu_lines(pending)
    } else if let Some(feedback) = app.pending_feedback.as_ref() {
        format_feedback_prompt_lines(feedback)
    } else {
        Vec::new()
    }
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, include_startup_card: bool) {
    let lines = transcript_lines_for_render(app, Some(area.width), include_startup_card);
    let scroll = transcript_scroll_offset(
        lines.len(),
        area.height,
        active_transcript_scroll_from_bottom(app),
    );
    let paragraph = Paragraph::new(lines)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

const TRANSCRIPT_OVERLAY_SCROLL_BOTTOM: usize = usize::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptOverlayMode {
    NativeSelection,
    ScrollbarDrag,
}

impl TranscriptOverlayMode {
    fn mouse_capture(self) -> bool {
        matches!(self, Self::ScrollbarDrag)
    }
}

/// State for the full-screen transcript overlay (Ctrl+T). All transcript
/// entries are rendered in their fully-expanded form regardless of each
/// entry's collapsed flag; the user scrolls with PgUp/PgDn/arrows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptOverlayState {
    pub(crate) scroll: usize,
    pub(crate) mode: TranscriptOverlayMode,
}

impl Default for TranscriptOverlayState {
    fn default() -> Self {
        Self {
            scroll: TRANSCRIPT_OVERLAY_SCROLL_BOTTOM,
            mode: TranscriptOverlayMode::NativeSelection,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptOverlayRenderKey {
    width: u16,
    transcript_len: usize,
    transcript_revision_hash: u64,
    selected_entry: Option<usize>,
    pending_hash: u64,
    show_reasoning_usage: bool,
    coalesce_tool_runs: bool,
    animation_tick: u64,
    palette_generation: u64,
}

#[derive(Debug, Default)]
pub(crate) struct TranscriptOverlayRenderCache {
    key: Option<TranscriptOverlayRenderKey>,
    rows: Vec<Line<'static>>,
}

/// Render the full-screen transcript overlay. Replaces the normal
/// transcript + prompt layout while `app.transcript_overlay` is `Some`.
fn render_transcript_overlay_surface(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let (overlay_area, status_area) = transcript_overlay_content_and_status_areas(area);
    if overlay_area.height > 0 {
        render_transcript_overlay(frame, overlay_area, app);
    }
    if status_area.height > 0 {
        render_status(frame, status_area, app);
    }
    render_toast_overlay(frame, area, app);
}

fn transcript_overlay_content_and_status_areas(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2)])
        .split(area);
    (chunks[0], chunks[1])
}

fn render_transcript_overlay(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let state = match app.transcript_overlay {
        Some(state) => state,
        None => return,
    };
    let title = " Transcript — Ctrl-T or Esc to close · PgUp/PgDn or wheel scroll ";
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::render::theme::secondary()))
        .title(Span::styled(
            title,
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = transcript_overlay_inner(area);
    frame.render_widget(block, area);
    let (text_area, scrollbar_area) =
        transcript_overlay_text_and_scrollbar_areas(inner).unwrap_or((
            inner,
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
        ));
    with_transcript_overlay_rows(app, text_area.width, |rows| {
        let row_count = rows.len();
        let scroll =
            resolved_transcript_overlay_scroll_for_state(state, row_count, text_area.height);
        render_transcript_overlay_rows(frame, text_area, rows, scroll);
        if scrollbar_area.width > 0
            && let Some(geometry) =
                transcript_overlay_scrollbar_geometry(row_count, scrollbar_area.height, scroll)
        {
            app.transcript_overlay_scrollbar_cache
                .set(Some(TranscriptOverlayScrollbarCache {
                    scrollbar_area,
                    geometry,
                }));
            render_transcript_overlay_scrollbar(frame, scrollbar_area, geometry);
        } else {
            app.transcript_overlay_scrollbar_cache.set(None);
        }
    });
}

#[cfg(test)]
fn transcript_overlay_rows_for_render(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    with_transcript_overlay_rows(app, width, |rows| rows.to_vec())
}

fn with_transcript_overlay_rows<R>(
    app: &TuiApp,
    width: u16,
    f: impl FnOnce(&[Line<'static>]) -> R,
) -> R {
    let width = width.max(1);
    let key = transcript_overlay_render_key(app, width);
    let mut cache = app.transcript_overlay_render_cache.borrow_mut();
    if cache.key != Some(key) {
        let logical_lines = transcript_lines_for_overlay(app, Some(width));
        cache.rows = wrap_transcript_overlay_rows(&logical_lines, width);
        cache.key = Some(key);
    }
    f(&cache.rows)
}

fn transcript_overlay_render_key(app: &TuiApp, width: u16) -> TranscriptOverlayRenderKey {
    let mut transcript_hasher = std::collections::hash_map::DefaultHasher::new();
    let entries = active_transcript_entries(app);
    for entry in entries {
        entry.id.hash(&mut transcript_hasher);
        entry.revision.hash(&mut transcript_hasher);
    }
    app.subagent_pane.active.hash(&mut transcript_hasher);

    let mut pending_hasher = std::collections::hash_map::DefaultHasher::new();
    active_pending_reasoning(app).hash(&mut pending_hasher);
    if active_subagent_record(app).is_none() && !app.pending_assistant.trim_is_empty() {
        app.pending_assistant.text().hash(&mut pending_hasher);
    }

    TranscriptOverlayRenderKey {
        width,
        transcript_len: entries.len(),
        transcript_revision_hash: transcript_hasher.finish(),
        selected_entry: active_selected_entry(app),
        pending_hash: pending_hasher.finish(),
        show_reasoning_usage: app.show_reasoning_usage,
        coalesce_tool_runs: app.coalesce_tool_runs,
        animation_tick: app.animation_tick,
        palette_generation: render::palette::palette_generation(),
    }
}

fn wrap_transcript_overlay_rows(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    for line in lines {
        wrap_transcript_overlay_line(line, width, &mut rows);
    }
    rows
}

fn wrap_transcript_overlay_line(line: &Line<'static>, width: usize, rows: &mut Vec<Line<'static>>) {
    let mut row_spans: Vec<Span<'static>> = Vec::new();
    let mut row_width = 0usize;
    let mut saw_content = false;

    for span in &line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for ch in span.content.chars() {
            if row_width >= width {
                if !chunk.is_empty() {
                    row_spans.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                rows.push(Line::from(std::mem::take(&mut row_spans)));
                row_width = 0;
            }
            chunk.push(ch);
            row_width += 1;
            saw_content = true;
        }
        if !chunk.is_empty() {
            row_spans.push(Span::styled(chunk, style));
        }
    }

    if saw_content {
        rows.push(Line::from(row_spans));
    } else {
        rows.push(Line::from(""));
    }
}

fn render_transcript_overlay_rows(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[Line<'static>],
    scroll: usize,
) {
    frame.render_widget(ratatui::widgets::Clear, area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let visible_rows = rows
        .iter()
        .skip(scroll)
        .take(usize::from(area.height))
        .cloned()
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible_rows), area);
}

fn transcript_overlay_inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn transcript_overlay_text_and_scrollbar_areas(inner: Rect) -> Option<(Rect, Rect)> {
    if inner.width <= 1 || inner.height == 0 {
        return None;
    }
    let text_area = Rect {
        width: inner.width.saturating_sub(1),
        ..inner
    };
    let scrollbar_area = Rect {
        x: inner.x + inner.width - 1,
        y: inner.y,
        width: 1,
        height: inner.height,
    };
    Some((text_area, scrollbar_area))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptScrollbarGeometry {
    thumb_top: u16,
    thumb_height: u16,
    max_scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptOverlayScrollbarCache {
    scrollbar_area: Rect,
    geometry: TranscriptScrollbarGeometry,
}

fn transcript_overlay_max_scroll_for_content(content_len: usize, viewport_height: u16) -> usize {
    content_len.saturating_sub(usize::from(viewport_height))
}

fn resolved_transcript_overlay_scroll_for_state(
    state: TranscriptOverlayState,
    content_len: usize,
    viewport_height: u16,
) -> usize {
    let max_scroll = transcript_overlay_max_scroll_for_content(content_len, viewport_height);
    if state.scroll == TRANSCRIPT_OVERLAY_SCROLL_BOTTOM {
        max_scroll
    } else {
        state.scroll.min(max_scroll)
    }
}

fn transcript_overlay_scrollbar_geometry(
    content_len: usize,
    viewport_height: u16,
    scroll: usize,
) -> Option<TranscriptScrollbarGeometry> {
    let track_height = usize::from(viewport_height);
    if track_height == 0 || content_len <= track_height {
        return None;
    }
    let max_scroll = content_len.saturating_sub(track_height);
    if max_scroll == 0 {
        return None;
    }
    let thumb_height = ((track_height * track_height) / content_len).clamp(1, track_height);
    let travel = track_height.saturating_sub(thumb_height);
    let scroll = scroll.min(max_scroll);
    let thumb_top = if travel == 0 {
        0
    } else {
        scroll * travel / max_scroll
    };
    Some(TranscriptScrollbarGeometry {
        thumb_top: thumb_top as u16,
        thumb_height: thumb_height as u16,
        max_scroll,
    })
}

#[cfg(test)]
fn transcript_overlay_scroll_for_scrollbar_row(
    row: u16,
    scrollbar_area: Rect,
    content_len: usize,
) -> Option<u16> {
    let geometry = transcript_overlay_scrollbar_geometry(content_len, scrollbar_area.height, 0)?;
    let local_row = row
        .saturating_sub(scrollbar_area.y)
        .min(scrollbar_area.height - 1);
    let track_height = usize::from(scrollbar_area.height);
    let thumb_height = usize::from(geometry.thumb_height);
    let travel = track_height.saturating_sub(thumb_height);
    if travel == 0 {
        return Some(0);
    }
    let centered = usize::from(local_row).saturating_sub(thumb_height / 2);
    let position = centered.min(travel);
    Some(((position * geometry.max_scroll) / travel).min(usize::from(u16::MAX)) as u16)
}

fn transcript_overlay_scroll_for_cached_scrollbar_row(
    row: u16,
    cache: TranscriptOverlayScrollbarCache,
) -> usize {
    let local_row = row
        .saturating_sub(cache.scrollbar_area.y)
        .min(cache.scrollbar_area.height.saturating_sub(1));
    let track_height = usize::from(cache.scrollbar_area.height);
    let thumb_height = usize::from(cache.geometry.thumb_height);
    let travel = track_height.saturating_sub(thumb_height);
    if travel == 0 {
        return 0;
    }
    let centered = usize::from(local_row).saturating_sub(thumb_height / 2);
    let position = centered.min(travel);
    (position * cache.geometry.max_scroll) / travel
}

fn render_transcript_overlay_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    geometry: TranscriptScrollbarGeometry,
) {
    let thumb_end = geometry.thumb_top.saturating_add(geometry.thumb_height);
    let lines = (0..area.height)
        .map(|offset| {
            let in_thumb = offset >= geometry.thumb_top && offset < thumb_end;
            let (symbol, style) = if in_thumb {
                (
                    "█",
                    Style::default()
                        .fg(crate::render::theme::accent())
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("░", Style::default().fg(crate::render::theme::quiet()))
            };
            Line::from(Span::styled(symbol, style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Build the per-entry line list for the overlay: every committed entry is
/// forced to its expanded form, and the live assistant tail is appended so
/// opening Ctrl-T mid-turn does not look frozen.
fn transcript_lines_for_overlay(app: &TuiApp, width: Option<u16>) -> Vec<Line<'static>> {
    // The overlay is the "Ctrl-T for full transcript" escape hatch — body
    // content blocks (e.g. read_tool_output payloads, shell stdout/stderr)
    // honour this verbosity, so pin Verbose to defeat the per-mode line cap
    // even when the user has `/verbosity compact` set for inline cards.
    let overlay_verbosity = ToolOutputVerbosity::Verbose;
    let mut lines = Vec::new();
    if let Some(title) = active_conversation_title(app) {
        lines.push(title);
        lines.push(Line::from(""));
    }
    let entries = active_transcript_entries(app);
    let selected_entry = active_selected_entry(app);
    for (index, entry) in entries.iter().enumerate() {
        match reasoning_run_info(entries, index) {
            Some(ReasoningRun::Suppressed) => continue,
            Some(ReasoningRun::Lead { extras }) => {
                if app.show_reasoning_usage
                    && let TranscriptEntryKind::Reasoning(snapshot) = &entry.kind
                {
                    lines.extend(reasoning_block_lines_with_extras(
                        &snapshot.display_text,
                        false,
                        selected_entry == Some(index),
                        extras,
                    ));
                    lines.push(Line::from(""));
                }
                continue;
            }
            None => {}
        }
        match tool_run_info(entries, index, app.coalesce_tool_runs) {
            Some(ToolRun::Suppressed) => continue,
            Some(ToolRun::Lead { extras }) => {
                let members = collect_tool_run_members(entries, index, extras);
                // Overlay always forces expanded form so users browsing
                // the full transcript can read every member's body.
                lines.extend(format_grouped_tool_result_entry(
                    &members,
                    false,
                    selected_entry == Some(index),
                    overlay_verbosity,
                    width,
                    ToolCardSurface::Plain,
                ));
                continue;
            }
            None => {}
        }
        lines.extend(cached_transcript_entry_lines(
            app.render_cache_session,
            entry,
            selected_entry == Some(index),
            overlay_verbosity,
            message_outcome(entries, index),
            width,
            app.show_reasoning_usage,
            true,
        ));
    }
    let pending = active_pending_assistant_lines(app);
    if !pending.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(pending);
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
        TranscriptEntryKind::ToolResult(tool) => format_tool_result_entry(
            tool,
            false,
            selected,
            tool_output_verbosity,
            width,
            ToolCardSurface::Plain,
        ),
        TranscriptEntryKind::Log(entry) => format_log_entry(entry, false, selected),
        TranscriptEntryKind::PlanCard(data) => format_plan_card_entry(data, false, width),
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
        TranscriptEntryKind::SlashEcho(data) => vec![format_slash_echo_line(data, selected)],
    }
}

/// Memoise the line list for a single transcript entry across redraws.
///
/// Wraps the underlying `format_transcript_entry_with_width` /
/// `format_transcript_entry_expanded` formatters with the per-entry LRU
/// cache in `render::cache`. The cache key is `(session_id, entry_id)`;
/// validation tags are the entry's content `revision`, the live
/// `palette_generation`, and a fingerprint of the per-render context
/// (selected flag, width, verbosity, outcome, show-reasoning toggle,
/// expanded-vs-normal). On cache hit this skips re-rendering markdown,
/// re-running tree-sitter for fenced blocks, and re-walking the entry
/// kind — the dominant per-frame cost for a long transcript.
///
/// `expanded = true` selects the overlay (`Ctrl+T`) variant which
/// forces `entry.collapsed = false`; `expanded = false` honours
/// `entry.collapsed`. The flag is folded into `context_hash` so the
/// overlay's expanded copy and the inline collapsed copy live as
/// separate cache lines under the same entry id.
///
/// The 8 parameters mirror the surface of the two underlying formatters
/// plus the `session_id` discriminator and the `expanded` switch; we
/// allow `clippy::too_many_arguments` rather than introduce a struct
/// purely as a clippy workaround.
#[allow(clippy::too_many_arguments)]
fn cached_transcript_entry_lines(
    session_id: u64,
    entry: &TranscriptEntry,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
    outcome: MessageOutcome,
    width: Option<u16>,
    show_reasoning: bool,
    expanded: bool,
) -> Vec<Line<'static>> {
    let palette_generation = render::palette::palette_generation();
    let context_hash = render_context_hash(
        selected,
        tool_output_verbosity,
        outcome,
        width,
        show_reasoning,
        expanded,
    );
    render::cache::get_or_compute_entry(
        session_id,
        entry.id,
        entry.revision,
        palette_generation,
        context_hash,
        || {
            if expanded {
                format_transcript_entry_expanded(
                    entry,
                    selected,
                    tool_output_verbosity,
                    outcome,
                    width,
                    show_reasoning,
                )
            } else {
                format_transcript_entry_with_width(
                    entry,
                    selected,
                    tool_output_verbosity,
                    outcome,
                    width,
                    show_reasoning,
                )
            }
        },
    )
}

/// Pack the per-render context bits into a single `u64` for the entry
/// cache's validity check. Bits are deliberately laid out non-overlap so
/// flipping any single dimension produces a distinct hash without a
/// hashing pass (the cache only needs equality, not uniform
/// distribution).
///
/// Layout:
/// - bit 0:      selected
/// - bit 1:      show_reasoning
/// - bit 2:      expanded (overlay vs inline)
/// - bits 4-5:   tool_output_verbosity (Compact=0, Normal=1, Verbose=2)
/// - bit 8:      message outcome (Normal=0, Failed=1)
/// - bits 16-31: width (0 when absent)
/// - bit 32:     width-present sentinel (distinguishes `Some(0)` from `None`)
fn render_context_hash(
    selected: bool,
    verbosity: ToolOutputVerbosity,
    outcome: MessageOutcome,
    width: Option<u16>,
    show_reasoning: bool,
    expanded: bool,
) -> u64 {
    let mut h: u64 = 0;
    if selected {
        h |= 1 << 0;
    }
    if show_reasoning {
        h |= 1 << 1;
    }
    if expanded {
        h |= 1 << 2;
    }
    let v: u64 = match verbosity {
        ToolOutputVerbosity::Compact => 0,
        ToolOutputVerbosity::Normal => 1,
        ToolOutputVerbosity::Verbose => 2,
    };
    h |= v << 4;
    let o: u64 = match outcome {
        MessageOutcome::Normal => 0,
        MessageOutcome::Failed => 1,
    };
    h |= o << 8;
    if let Some(w) = width {
        h |= (w as u64) << 16;
        h |= 1u64 << 32;
    }
    h
}

fn active_subagent_record(app: &TuiApp) -> Option<&SubagentRecord> {
    let ConversationSource::Subagent(id) = app.subagent_pane.active else {
        return None;
    };
    app.subagent_pane
        .records
        .iter()
        .find(|record| record.id == id)
}

fn active_subagent_record_mut(app: &mut TuiApp) -> Option<&mut SubagentRecord> {
    let ConversationSource::Subagent(id) = app.subagent_pane.active else {
        return None;
    };
    app.subagent_pane
        .records
        .iter_mut()
        .find(|record| record.id == id)
}

fn active_transcript_entries(app: &TuiApp) -> &[TranscriptEntry] {
    active_subagent_record(app)
        .map(|record| record.transcript.as_slice())
        .unwrap_or(app.transcript.as_slice())
}

fn active_transcript_scroll_from_bottom(app: &TuiApp) -> u16 {
    active_subagent_record(app)
        .map(|record| record.scroll_from_bottom)
        .unwrap_or(app.transcript_scroll_from_bottom)
}

fn set_active_transcript_scroll_from_bottom(app: &mut TuiApp, scroll: u16) {
    if let Some(record) = active_subagent_record_mut(app) {
        record.scroll_from_bottom = scroll;
    } else {
        app.transcript_scroll_from_bottom = scroll;
    }
}

fn active_selected_entry(app: &TuiApp) -> Option<usize> {
    if active_subagent_record(app).is_some() {
        None
    } else {
        app.selected_entry
    }
}

fn active_pending_reasoning(app: &TuiApp) -> &str {
    if active_subagent_record(app).is_some() {
        ""
    } else {
        &app.pending_reasoning
    }
}

fn active_pending_assistant_lines(app: &TuiApp) -> Vec<Line<'static>> {
    if active_subagent_record(app).is_some() {
        Vec::new()
    } else {
        pending_assistant_lines(app)
    }
}

fn active_conversation_title(app: &TuiApp) -> Option<Line<'static>> {
    let record = active_subagent_record(app)?;
    Some(Line::from(vec![
        Span::styled(
            "●",
            Style::default()
                .fg(record.lifecycle.color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            record.agent.clone(),
            Style::default()
                .fg(crate::render::theme::foreground())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" subagent · {}", record.lifecycle.label()),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]))
}

fn transcript_lines_for_render(
    app: &TuiApp,
    width: Option<u16>,
    include_startup_card: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(title) = active_conversation_title(app) {
        lines.push(title);
        lines.push(Line::from(""));
    } else if include_startup_card {
        let card_width = width.unwrap_or(64);
        lines.extend(startup_card_lines(app, card_width));
        lines.push(Line::from(""));
    }
    let entries = active_transcript_entries(app);
    let selected_entry = active_selected_entry(app);
    for (index, item) in entries.iter().enumerate() {
        match reasoning_run_info(entries, index) {
            Some(ReasoningRun::Suppressed) => continue,
            Some(ReasoningRun::Lead { extras }) => {
                if app.show_reasoning_usage
                    && let TranscriptEntryKind::Reasoning(snapshot) = &item.kind
                {
                    lines.extend(reasoning_block_lines_with_extras(
                        &snapshot.display_text,
                        item.collapsed,
                        selected_entry == Some(index),
                        extras,
                    ));
                    lines.push(Line::from(""));
                }
                continue;
            }
            None => {}
        }
        match tool_run_info(entries, index, app.coalesce_tool_runs) {
            Some(ToolRun::Suppressed) => continue,
            Some(ToolRun::Lead { extras }) => {
                let members = collect_tool_run_members(entries, index, extras);
                lines.extend(format_grouped_tool_result_entry(
                    &members,
                    item.collapsed,
                    selected_entry == Some(index),
                    app.tool_output_verbosity,
                    width,
                    ToolCardSurface::Tinted,
                ));
                continue;
            }
            None => {}
        }
        lines.extend(cached_transcript_entry_lines(
            app.render_cache_session,
            item,
            selected_entry == Some(index),
            app.tool_output_verbosity,
            message_outcome(entries, index),
            width,
            app.show_reasoning_usage,
            false,
        ));
    }
    let pending_reasoning = active_pending_reasoning(app);
    if app.show_reasoning_usage && !pending_reasoning.trim().is_empty() {
        lines.extend(streaming_reasoning_lines(pending_reasoning));
    }
    if active_subagent_record(app).is_none()
        && let Some(pending_assistant) = pending_assistant_display_content(app)
    {
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
    let mut lines = vec![Line::from(Span::styled("▾ reasoning…".to_string(), style))];
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
    // Dynamic metadata belongs in the live status line. In inline mode
    // the startup card lives in scrollback and cannot be rewritten, so
    // repeating fields such as directory/model/languages here makes the
    // first frame stale and visually redundant.
    vec![startup_phase_strip(width as usize, app.version)]
}

fn startup_phase_strip(card_width: usize, version: &str) -> Line<'static> {
    // Waxing phases (new → full) on the left, waning phases (full → new)
    // on the right, so the strip reads as one true lunar cycle rather than
    // a mirror of the same glyphs.
    const WAX_FULL: &[&str] = &["○", "☽", "◑", "●"];
    const WANE_FULL: &[&str] = &["●", "◐", "☾", "○"];
    const WAX_COMPACT: &[&str] = &["☽", "◑", "●"];
    const WANE_COMPACT: &[&str] = &["●", "◐", "☾"];
    const WAX_TIGHT: &[&str] = &["◑", "●"];
    const WANE_TIGHT: &[&str] = &["●", "◐"];
    const WAX_MINIMAL: &[&str] = &["●"];
    const WANE_MINIMAL: &[&str] = &["●"];

    let title = "Squeezy";
    let version_text = format!(" v{version}");
    let title_total = title.chars().count() + version_text.chars().count();

    for (wax, wane) in [
        (WAX_FULL, WANE_FULL),
        (WAX_COMPACT, WANE_COMPACT),
        (WAX_TIGHT, WANE_TIGHT),
        (WAX_MINIMAL, WANE_MINIMAL),
    ] {
        let strip_chars = wax.len() * 2 - 1;
        let content_total = strip_chars + 2 + title_total + 2 + strip_chars;
        if content_total <= card_width {
            let left_strip = wax.join(" ");
            let right_strip = wane.join(" ");
            let pad_total = card_width.saturating_sub(content_total);
            let pad_left = pad_total / 2;
            let pad_right = pad_total.saturating_sub(pad_left);
            return Line::from(vec![
                Span::raw(" ".repeat(pad_left)),
                Span::styled(
                    left_strip,
                    Style::default().fg(crate::render::theme::accent()),
                ),
                Span::raw("  "),
                Span::styled(
                    title.to_string(),
                    Style::default()
                        .fg(crate::render::theme::foreground())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    version_text.clone(),
                    Style::default().fg(crate::render::theme::accent()),
                ),
                Span::raw("  "),
                Span::styled(
                    right_strip,
                    Style::default().fg(crate::render::theme::accent()),
                ),
                Span::raw(" ".repeat(pad_right)),
            ]);
        }
    }

    let pad = card_width.saturating_sub(title_total) / 2;
    Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(
            title.to_string(),
            Style::default()
                .fg(crate::render::theme::foreground())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            version_text,
            Style::default().fg(crate::render::theme::accent()),
        ),
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
                format!(" · +{hidden} more"),
                Style::default().fg(crate::render::theme::quiet()),
            ));
        }
    }
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(crate::render::theme::quiet()))
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
/// existing `crate::render::theme::magenta()` palette entry (no new colors) and ASCII
/// brackets with a Unicode `⊕` glyph — matches the other status glyphs
/// (`⟳`, `▸`) already used in this file.
pub(crate) fn format_plan_mode_indicator_line() -> Line<'static> {
    let label_style = Style::default()
        .fg(crate::render::theme::magenta())
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled("⊕ PLAN MODE", label_style),
        Span::styled(
            " · Shift+Tab to exit",
            Style::default().fg(crate::render::theme::quiet()),
        ),
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
            ToolCardSurface::Tinted,
        ),
        TranscriptEntryKind::Log(entry_log) => {
            format_log_entry(entry_log, entry.collapsed, selected)
        }
        TranscriptEntryKind::PlanCard(data) => format_plan_card_entry(data, entry.collapsed, width),
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
        TranscriptEntryKind::SlashEcho(data) => vec![format_slash_echo_line(data, selected)],
    }
}

fn format_slash_echo_line(data: &SlashEchoData, selected: bool) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let mut spans = vec![
        Span::raw(marker),
        Span::styled(
            "›  ",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            data.cmd.clone(),
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !data.args.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            data.args.clone(),
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    Line::from(spans)
}

fn format_plan_card_entry(
    data: &render::plan_card::PlanCardData,
    collapsed: bool,
    width: Option<u16>,
) -> Vec<Line<'static>> {
    // Collapsed cards show just the header so users can fold a long
    // plan out of view without losing the anchor.
    let lines = render::plan_card::render_plan_card(data, width);
    if collapsed {
        return lines.into_iter().take(1).collect();
    }
    lines
}

/// Run the `/diff` slash command: capture a worktree diff (tracked +
/// untracked) via `GitVcs::snapshot` and push a styled card into the
/// transcript. On a clean tree or a non-git workspace we surface a log
/// advisory instead of an empty card. Never truncated by default, so
/// the user always sees the full diff via the existing `render::diff`
/// helpers.
fn handle_slash_diff(app: &mut TuiApp) {
    if app.pending_diff.is_some() {
        app.push_log("/diff: already computing — please wait".to_string());
        return;
    }
    let workspace_root = app.workspace_root.clone();
    let (tx, rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let result = compute_diff_snapshot(&workspace_root);
        let _ = tx.send(result);
    });
    app.pending_diff = Some(rx);
    app.pending_diff_started_at = Some(Instant::now());
    app.needs_redraw = true;
}

/// Synchronous diff snapshot used by the background task. Returns either
/// a renderable card or a list of log lines (errors / "no changes" /
/// "not a git repo"). Kept off the input thread because `vcs.snapshot()`
/// shells out to `git status` + `git diff` and iterates every changed
/// file — fine for a background worker, fatal for the UI thread.
fn compute_diff_snapshot(workspace_root: &Path) -> PendingDiffResult {
    let vcs = match GitVcs::open(workspace_root) {
        Ok(vcs) => vcs,
        Err(err) => {
            return PendingDiffResult {
                logs: vec![format!("/diff failed to open workspace VCS: {err}")],
                card: None,
            };
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
        return PendingDiffResult {
            logs: vec!["/diff: workspace is not a git repository".to_string()],
            card: None,
        };
    }
    let mut logs: Vec<String> = snapshot
        .errors
        .iter()
        .map(|e| format!("/diff git error: {e}"))
        .collect();
    if snapshot.files.is_empty() {
        logs.push("/diff: no uncommitted changes".to_string());
        return PendingDiffResult { logs, card: None };
    }
    PendingDiffResult {
        logs,
        card: Some(build_diff_card(&snapshot)),
    }
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
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
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
        crate::render::theme::secondary(),
        "Diff",
        crate::render::theme::secondary(),
        vec![Span::styled(
            data.summary.clone(),
            Style::default().fg(crate::render::theme::quiet()),
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
            TranscriptEntryKind::Log(entry_log) if is_failure_log(entry_log.message()) => {
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
        if let Some(stripped) = strip_repeated_raw_tool_output(&content, output) {
            content = stripped;
        }
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

fn strip_repeated_raw_tool_output(content: &str, output: &str) -> Option<String> {
    let content = normalize_duplicate_tool_output(content);
    let output = normalize_duplicate_tool_output(output);
    (!output.is_empty() && content == output).then(String::new)
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
    let label_color = if failed {
        crate::render::theme::red()
    } else {
        color
    };
    let action_color = if failed {
        crate::render::theme::red()
    } else {
        color
    };
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
        && let Some(lines) = format_ansi_system_entry(
            selected,
            "• ",
            label_color,
            action,
            action_color,
            &item.content,
        )
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

/// Render system messages whose content embeds ANSI escape sequences
/// (currently emitted by `/context` and `/cost` via `commands_style`).
/// Plain-text system messages fall through to the default single-style
/// renderer so the dozens of other `TranscriptItem::system` callers stay
/// byte-identical.
fn format_ansi_system_entry(
    selected: bool,
    label: &'static str,
    label_color: Color,
    action: &'static str,
    action_color: Color,
    content: &str,
) -> Option<Vec<Line<'static>>> {
    if !content.contains('\x1b') {
        return None;
    }
    let parsed = crate::render::ansi::ansi_to_text(content);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(parsed.lines.len().max(1));
    let mut iter = parsed.lines.into_iter();
    let first = iter.next().unwrap_or_else(|| Line::from(""));
    out.push(action_line_spans(
        selected,
        label,
        label_color,
        action,
        action_color,
        first.spans,
    ));
    for line in iter {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    Some(out)
}

fn format_user_prompt_entry(
    item: &TranscriptItem,
    _selected: bool,
    _width: Option<u16>,
) -> Vec<Line<'static>> {
    let bang_range = bang_command_marker_range(&item.content);
    let mut content = item.content.split('\n').collect::<Vec<_>>();
    if content.is_empty() {
        content.push("");
    }
    let max_text_width = content
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0)
        .max(1);
    let amber = Style::default().fg(crate::render::theme::accent());
    let bullet_style = amber.add_modifier(Modifier::BOLD);
    const INDENT: &str = "  ";
    // Cycle through the moon-phase set so consecutive prompts get a
    // visually different bullet — content-hashed (not index-based) so
    // the marker is stable per message even when prompts are
    // reordered/collapsed in the transcript.
    const BULLETS: &[&str] = &["○", "☽", "◑", "●", "◐", "☾"];
    let bullet_idx = item
        .content
        .bytes()
        .fold(0usize, |acc, b| acc.wrapping_add(b as usize))
        % BULLETS.len();
    let bullet = BULLETS[bullet_idx];

    let mut lines = Vec::with_capacity(content.len() + 2);

    let slash_ranges = input::slash_command_ranges(&item.content);
    let mut line_start = 0usize;
    for (index, line_text) in content.iter().enumerate() {
        let bang_ref = bang_range.as_ref();
        let style_text_at = |abs_offset: usize| -> Style {
            if bang_ref.is_some_and(|range| range.contains(&abs_offset)) {
                Style::default().fg(crate::render::theme::red())
            } else if slash_ranges
                .iter()
                .any(|(start, end)| *start <= abs_offset && abs_offset < *end)
            {
                Style::default().fg(crate::render::theme::accent())
            } else {
                Style::default().fg(crate::render::theme::foreground())
            }
        };
        // First line gets the cycling moon-phase bullet; continuation
        // lines indent in line with where the bullet sat.
        let prefix_span = if index == 0 {
            Span::styled(format!("{bullet} "), bullet_style)
        } else {
            Span::raw("  ".to_string())
        };
        let mut spans = vec![Span::raw(INDENT.to_string()), prefix_span];
        push_styled_segments(&mut spans, line_text, line_start, style_text_at);
        lines.push(Line::from(spans));
        line_start = line_start.saturating_add(line_text.len()).saturating_add(1);
    }

    let bottom = Line::from(vec![
        Span::raw(INDENT.to_string()),
        Span::styled(format!("╰─◖{}╯", "─".repeat(max_text_width)), amber),
    ]);
    lines.push(bottom);
    lines.push(Line::from(""));
    lines
}

/// Byte range covering the leading `!` (single-bang) or `!!`
/// (double-bang, runs locally but skips LLM context) marker after any
/// leading whitespace, or `None` if the line is not a bang command.
/// Returned as a range — rather than the previous single offset — so the
/// prompt renderer can paint both bangs in `crate::render::theme::red()` for `!!cmd`.
fn bang_command_marker_range(text: &str) -> Option<std::ops::Range<usize>> {
    let mut chars = text.char_indices().skip_while(|(_, ch)| ch.is_whitespace());
    let (start, first) = chars.next()?;
    if first != '!' {
        return None;
    }
    let mut end = start + first.len_utf8();
    if let Some((_, next)) = chars.next()
        && next == '!'
    {
        end += next.len_utf8();
    }
    Some(start..end)
}

fn format_assistant_message_entry(
    item: &TranscriptItem,
    collapsed: bool,
    selected: bool,
    outcome: MessageOutcome,
    show_reasoning: bool,
) -> Vec<Line<'static>> {
    let color = if outcome == MessageOutcome::Failed {
        crate::render::theme::red()
    } else {
        crate::render::theme::green()
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
    reasoning_block_lines_with_extras(text, collapsed, selected, 0)
}

/// `extras` is the number of additional adjacent reasoning entries this chip
/// stands in for. When `> 0`, the collapsed chip header gains a `· +N more`
/// suffix so the run reads as one item.
fn reasoning_block_lines_with_extras(
    text: &str,
    collapsed: bool,
    selected: bool,
    extras: usize,
) -> Vec<Line<'static>> {
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
        let mut suffix = if body_lines.len() > 1 {
            format!(
                " … +{} lines (Ctrl-T for full transcript)",
                body_lines.len() - 1
            )
        } else {
            String::new()
        };
        if extras > 0 {
            suffix.push_str(&format!(" · +{extras} more"));
        }
        let summary_sep = if summary.is_empty() { "" } else { " · " };
        lines.push(Line::from(Span::styled(
            format!("{marker}▸ reasoning{summary_sep}{summary}{suffix}"),
            style,
        )));
    } else {
        let header = if extras > 0 {
            format!(
                "{marker}▾ reasoning ({} lines · +{extras} more)",
                body_lines.len().max(1)
            )
        } else {
            format!("{marker}▾ reasoning ({} lines)", body_lines.len().max(1))
        };
        lines.push(Line::from(Span::styled(header, style)));
        for raw in body_lines {
            lines.push(Line::from(Span::styled(format!("▏ {}", raw), style)));
        }
    }
    lines
}

/// Outcome of inspecting a reasoning entry's neighborhood for coalescing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningRun {
    /// Lead of a run of ≥2 adjacent reasoning entries. `extras` is the
    /// number of *additional* entries this lead absorbs.
    Lead { extras: usize },
    /// A non-lead member of a run — caller should render nothing.
    Suppressed,
}

/// If `index` is a `Reasoning` entry, classify how it should render given
/// its neighborhood. Returns `None` for non-reasoning entries or singletons,
/// in which case the caller renders normally.
fn reasoning_run_info(transcript: &[TranscriptEntry], index: usize) -> Option<ReasoningRun> {
    let entry = transcript.get(index)?;
    if !matches!(entry.kind, TranscriptEntryKind::Reasoning(_)) {
        return None;
    }
    let prev_is_reasoning = index > 0
        && matches!(
            transcript[index - 1].kind,
            TranscriptEntryKind::Reasoning(_)
        );
    if prev_is_reasoning {
        return Some(ReasoningRun::Suppressed);
    }
    let mut extras = 0usize;
    let mut j = index + 1;
    while let Some(next) = transcript.get(j) {
        if matches!(next.kind, TranscriptEntryKind::Reasoning(_)) {
            extras += 1;
            j += 1;
        } else {
            break;
        }
    }
    if extras == 0 {
        None
    } else {
        Some(ReasoningRun::Lead { extras })
    }
}

/// Outcome of inspecting a `ToolResult` entry for render-time coalescing.
/// Independent from [`coalesce_tool_transcript_entry`] which mutates the
/// previous entry's `repeat_count` at push time for *retry* attempts of
/// the same tool+path. `tool_run_info` is a pure render-time decision over
/// already-stored entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolRun {
    /// Lead of a run of ≥2 adjacent same-tool same-status entries.
    /// `extras` is the number of additional entries this lead absorbs.
    Lead { extras: usize },
    /// A non-lead member of a run — caller should render nothing.
    Suppressed,
}

/// If `index` is a `ToolResult` that can fold into a same-tool run,
/// classify it. Returns `None` for non-tool entries, singletons, or
/// any tool that fails the [`tool_run_coalesce_eligible`] gate. Caller
/// supplies the config switch so the function stays pure.
fn tool_run_info(
    transcript: &[TranscriptEntry],
    index: usize,
    coalesce_enabled: bool,
) -> Option<ToolRun> {
    if !coalesce_enabled {
        return None;
    }
    let entry = transcript.get(index)?;
    let TranscriptEntryKind::ToolResult(tool) = &entry.kind else {
        return None;
    };
    if !tool_run_coalesce_eligible(tool) {
        return None;
    }
    let tool_name = tool.result.tool_name.clone();
    let status = tool.result.status;
    let prev_matches = index > 0
        && match &transcript[index - 1].kind {
            TranscriptEntryKind::ToolResult(prev) => {
                tool_run_coalesce_eligible(prev)
                    && prev.result.tool_name == tool_name
                    && prev.result.status == status
            }
            _ => false,
        };
    if prev_matches {
        return Some(ToolRun::Suppressed);
    }
    let mut extras = 0usize;
    let mut j = index + 1;
    while let Some(next) = transcript.get(j) {
        let TranscriptEntryKind::ToolResult(next_tool) = &next.kind else {
            break;
        };
        if !tool_run_coalesce_eligible(next_tool)
            || next_tool.result.tool_name != tool_name
            || next_tool.result.status != status
        {
            break;
        }
        extras += 1;
        j += 1;
    }
    if extras == 0 {
        None
    } else {
        Some(ToolRun::Lead { extras })
    }
}

/// Whether a tool entry is eligible for render-time run coalescing.
/// Retry-coalesced entries (`repeat_count > 1`) keep their visible
/// multiplier and stay standalone. Body-as-card tools (`apply_patch`,
/// `write_file`, `plan_patch`, `diff_context`) bypass the preview cap
/// because their card IS the body — folding them into a one-line summary
/// would discard the signal. Hidden-by-default entries are excluded
/// defensively for snapshot / overlay paths.
fn tool_run_coalesce_eligible(tool: &ToolTranscript) -> bool {
    tool.repeat_count == 1
        && !tool_bypasses_preview_cap_for_tool(tool)
        && !tool_result_hidden_by_default(&tool.result)
}

/// Pull out the lead + extras `ToolTranscript` references for a grouped
/// card. Caller must have classified `index` as `ToolRun::Lead` with the
/// matching `extras` count.
fn collect_tool_run_members(
    transcript: &[TranscriptEntry],
    lead_index: usize,
    extras: usize,
) -> Vec<&ToolTranscript> {
    let mut members = Vec::with_capacity(extras + 1);
    for offset in 0..=extras {
        let entry = transcript
            .get(lead_index + offset)
            .expect("collect_tool_run_members called with out-of-range index");
        if let TranscriptEntryKind::ToolResult(tool) = &entry.kind {
            members.push(tool.as_ref());
        } else {
            // Defensive: should never fire because `tool_run_info`
            // already verified each member is a `ToolResult`.
            break;
        }
    }
    members
}

fn collapsed_content_summary(content: &str) -> String {
    let lines = content.lines().collect::<Vec<_>>();
    if lines.len() > 1 {
        let first = compact_text(lines.first().copied().unwrap_or_default(), 120);
        format!(
            "{first} … +{} lines (Ctrl-T for full transcript)",
            lines.len() - 1
        )
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
    card_surface: ToolCardSurface,
) -> Vec<Line<'static>> {
    let (marker, action) = tool_result_action(tool);
    let color = tool_result_display_color(tool);
    let summary_spans = tool_result_summary_spans(tool);
    let header = action_line_spans(selected, marker, color, action, color, summary_spans);
    let body = if collapsed {
        collapsed_tool_preview_lines(tool, tool_output_verbosity, width)
    } else {
        expanded_tool_detail_lines(tool, tool_output_verbosity)
    };
    render_tool_card(header, body, card_surface)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCardSurface {
    Tinted,
    Plain,
}

fn render_tool_card(
    header: Line<'static>,
    body: Vec<Line<'static>>,
    surface: ToolCardSurface,
) -> Vec<Line<'static>> {
    match surface {
        ToolCardSurface::Tinted => wrap_tool_card(header, body),
        ToolCardSurface::Plain => stack_tool_card(header, body),
    }
}

fn stack_tool_card(header: Line<'static>, body: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut lines = vec![header];
    lines.extend(body);
    lines
}

/// Visually group an action header with its detail rows by tinting both
/// with a subtle card surface. Bails out gracefully on terminals that
/// can't render bg blends — `card_background_style()` returns `None` and
/// the layout falls back to today's flat row stack. Shared between the
/// singleton renderer ([`format_tool_result_entry`]) and the grouped
/// renderer ([`format_grouped_tool_result_entry`]).
fn wrap_tool_card(header: Line<'static>, body: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let bg = render::card::card_background_style();
    if bg.is_none() {
        let mut lines = vec![header];
        lines.extend(body);
        return lines;
    }
    let mut lines = vec![render::card::apply_background(header, bg)];
    lines.extend(
        body.into_iter()
            .map(|line| render::card::apply_background(line, bg)),
    );
    if let Some(trailer) = render::card::blank_card_line(bg) {
        lines.push(trailer);
    }
    lines
}

/// Render a `ToolRun::Lead { extras }` as a single grouped card spanning
/// `extras + 1` consecutive same-tool same-status entries.
///
/// In collapsed form the header reads `"Read 3 files"` etc., the body is
/// one summary row per member followed by a `(Ctrl-T for full transcript)`
/// affordance row. In expanded form the header is followed by each
/// member's normal tool-card body rendered inline so the user can scan
/// their full output.
fn format_grouped_tool_result_entry(
    members: &[&ToolTranscript],
    collapsed: bool,
    selected: bool,
    tool_output_verbosity: ToolOutputVerbosity,
    width: Option<u16>,
    card_surface: ToolCardSurface,
) -> Vec<Line<'static>> {
    debug_assert!(members.len() >= 2, "grouped card needs at least 2 members");
    let lead = members[0];
    let count = members.len();
    let (marker, _) = tool_result_action(lead);
    let color = tool_result_display_color(lead);
    let action_label = grouped_action_label(lead);
    let header_summary = vec![Span::styled(
        format!("{count} {}", grouped_action_noun(lead, count)),
        Style::default().fg(crate::render::theme::quiet()),
    )];
    let header = action_line_spans(selected, marker, color, action_label, color, header_summary);

    let mut body: Vec<Line<'static>> = Vec::new();
    if collapsed {
        for member in members {
            body.push(detail_spans_line(tool_oneline_summary(member)));
        }
        body.push(detail_line(
            false,
            crate::render::theme::quiet(),
            "(Ctrl-T for full transcript)".to_string(),
        ));
    } else {
        // Stack each child's full single-tool render. Each is already
        // its own card via `wrap_tool_card`; emit them back-to-back so
        // the grouped card visually contains them.
        for (idx, member) in members.iter().enumerate() {
            if idx > 0 {
                body.push(Line::from(""));
            }
            body.extend(format_tool_result_entry(
                member,
                false,
                false,
                tool_output_verbosity,
                width,
                card_surface,
            ));
        }
    }
    render_tool_card(header, body, card_surface)
}

/// Verb that titles a grouped run — `"Read"`, `"Searched"`, etc. Falls
/// back to the singular action label when a tool doesn't have a distinct
/// grouped form.
fn grouped_action_label(lead: &ToolTranscript) -> &'static str {
    match lead.result.tool_name.as_str() {
        "read_file" | "read_slice" | "read_tool_output" => "Read",
        "glob" => "Globbed",
        "grep" => "Searched",
        "decl_search" => "Searched decls",
        "definition_search" => "Searched defs",
        "reference_search" => "Searched refs",
        "symbol_context" => "Inspected",
        "hierarchy" => "Walked hierarchy",
        "upstream_flow" => "Traced upstream",
        "downstream_flow" => "Traced downstream",
        "repo_map" => "Mapped",
        "shell" | "verify" => "Ran",
        "webfetch" => "Fetched",
        "websearch" => "Searched web",
        _ => tool_result_action(lead).1,
    }
}

/// Noun + plural-aware suffix that follows the count in the grouped
/// header — `"3 files"`, `"2 patterns"`, `"4 commands"`.
fn grouped_action_noun(lead: &ToolTranscript, count: usize) -> String {
    let singular = match lead.result.tool_name.as_str() {
        "read_file" | "read_slice" | "read_tool_output" | "glob" => "file",
        "grep" | "decl_search" | "definition_search" | "reference_search" => "search",
        "symbol_context" => "symbol",
        "hierarchy" | "upstream_flow" | "downstream_flow" => "trace",
        "repo_map" => "map",
        "shell" | "verify" => "command",
        "webfetch" | "websearch" => "request",
        _ => "call",
    };
    if count == 1 {
        singular.to_string()
    } else {
        format!("{singular}s")
    }
}

/// One-line summary for a tool result, used as a row inside a grouped
/// card. Reuses [`tool_result_summary_spans`] which already produces a
/// concise per-tool summary (e.g. `path · 4.9KB of 26.4KB` for reads,
/// `pattern · N matches` for grep). The group header already carries
/// the verb and status color, so the row itself stays crate::render::theme::quiet().
fn tool_oneline_summary(tool: &ToolTranscript) -> Vec<Span<'static>> {
    tool_result_summary_spans(tool)
}

/// Whether this tool result was triggered by the user typing `!<command>`
/// (a "direct user shell" call), as opposed to being initiated by the
/// model. The agent stamps `direct_user_shell: true` on these calls in
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
/// therefore do not truncate by default. Patch output stays uncapped
/// so the user always sees the full edit set.
fn tool_bypasses_preview_cap(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "apply_patch" | "write_file" | "plan_patch" | "diff_context"
    )
}

/// Same intent as [`tool_bypasses_preview_cap`] but consults the result
/// content too: a shell command whose stdout is a unified diff (`git diff`,
/// `git show`, …) should render in full unless the user explicitly opted
/// into folded shell diffs via `tui.shell_diff_inline = "folded"`. The
/// diff IS the card; head/tail-capping it discards every hunk past the
/// first or last five lines.
fn tool_bypasses_preview_cap_for_tool(tool: &ToolTranscript) -> bool {
    if tool_bypasses_preview_cap(tool.result.tool_name.as_str()) {
        return true;
    }
    if matches!(shell_diff_inline_setting(), ShellDiffInline::Full)
        && shell_output_is_unified_diff(tool)
    {
        return true;
    }
    false
}

fn collapsed_tool_preview_lines(
    tool: &ToolTranscript,
    tool_output_verbosity: ToolOutputVerbosity,
    _width: Option<u16>,
) -> Vec<Line<'static>> {
    if tool_bypasses_preview_cap_for_tool(tool) {
        return expanded_tool_detail_lines(tool, tool_output_verbosity);
    }
    let detail = expanded_tool_detail_lines(tool, tool_output_verbosity);
    let cap = tool_preview_line_cap(tool);
    head_tail_truncate_lines(detail, cap)
}

/// Head-tail truncate a list of rendered detail lines, inserting a single
/// "… +N lines (Ctrl-T for full transcript)" ellipsis between the head and tail
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
        crate::render::theme::quiet(),
        format!("… +{omitted} lines (Ctrl-T for full transcript)"),
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

fn format_log_entry(entry: &LogEntry, collapsed: bool, selected: bool) -> Vec<Line<'static>> {
    let message = entry.message.as_str();
    if entry.kind == LogKind::Operational {
        // Operational chrome: dim italic line with no bullet so it visually
        // sinks below content. Always one line; selection inherits the
        // standard marker so keyboard nav still highlights the entry.
        let marker = if selected { "> " } else { "  " };
        let style = Style::default()
            .fg(palette::footer_fg())
            .add_modifier(Modifier::ITALIC);
        let preview = compact_text(message, 200);
        return vec![Line::from(vec![
            Span::raw(marker),
            Span::styled(preview, style),
        ])];
    }
    if entry.kind == LogKind::Info {
        return detail_text_lines(selected, crate::render::theme::cyan(), message);
    }
    if entry.kind == LogKind::Warn {
        // `⚠ message` rendering for warnings so the user can spot
        // config issues and turn failures at a glance. Newlines are flattened to spaces so
        // the whole error reads as one bullet, and the transcript
        // paragraph's `Wrap { trim: false }` line-wraps anything long —
        // errors from providers can be hundreds of characters and need to
        // stay fully visible rather than being cut off with `…`.
        let marker = if selected { "> " } else { "  " };
        let preview = message.replace('\n', " ");
        return vec![Line::from(vec![
            Span::raw(marker),
            Span::styled(
                "⚠ ",
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(preview, Style::default().fg(palette::muted_fg())),
        ])];
    }
    let color = log_color(message);
    if collapsed && !is_failure_log(message) {
        let preview = compact_text(message, 140);
        return vec![detail_line(selected, color, preview)];
    }
    detail_text_lines(selected, color, message)
}

fn role_action(role: &Role) -> (&'static str, Color) {
    match role {
        Role::User => ("Asked", crate::render::theme::accent()),
        Role::Assistant => ("Answered", crate::render::theme::green()),
        Role::System => ("Noted", crate::render::theme::secondary()),
    }
}

fn message_content_style(role: &Role) -> Style {
    match role {
        Role::User => Style::default()
            .fg(palette::muted_fg())
            .bg(crate::render::theme::prompt_bg()),
        Role::Assistant | Role::System => Style::default(),
    }
}

fn log_color(message: &str) -> Color {
    if is_failure_log(message) {
        crate::render::theme::red()
    } else {
        crate::render::theme::secondary()
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
            "│ ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(content.into(), Style::default().fg(palette::muted_fg())),
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
            Style::default().fg(palette::muted_fg()),
        )];
        if let Some(call) = tool.call.as_ref() {
            let label = tool_call_label(call);
            if label != result.tool_name {
                spans.push(Span::styled(
                    " · ",
                    Style::default().fg(crate::render::theme::quiet()),
                ));
                spans.push(Span::styled(
                    label,
                    Style::default().fg(crate::render::theme::quiet()),
                ));
            }
        }
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            tool_result_error_detail(result),
            Style::default().fg(crate::render::theme::quiet()),
        ));
        if tool.repeat_count > 1 {
            spans.push(Span::styled(
                format!(" ({}x)", tool.repeat_count),
                Style::default().fg(crate::render::theme::quiet()),
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
            Style::default().fg(palette::muted_fg()),
        )],
    };
    if tool_result_not_run(tool) {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            tool_result_error_detail(result),
            Style::default().fg(crate::render::theme::quiet()),
        ));
        if tool.repeat_count > 1 {
            spans.push(Span::styled(
                format!(" ({}x)", tool.repeat_count),
                Style::default().fg(crate::render::theme::quiet()),
            ));
        }
        return spans;
    }
    match result.status {
        ToolStatus::Error | ToolStatus::Stale => {
            spans.push(Span::styled(
                " · ",
                Style::default().fg(crate::render::theme::quiet()),
            ));
            spans.push(Span::styled(
                tool_result_error_detail(result),
                Style::default().fg(crate::render::theme::quiet()),
            ));
        }
        ToolStatus::Denied => {
            spans.push(Span::styled(
                " · ",
                Style::default().fg(crate::render::theme::quiet()),
            ));
            spans.push(Span::styled(
                tool_result_denied_detail(result),
                Style::default().fg(crate::render::theme::quiet()),
            ));
        }
        ToolStatus::Cancelled => {
            spans.push(Span::styled(
                " · cancelled",
                Style::default().fg(crate::render::theme::quiet()),
            ));
        }
        ToolStatus::Success => {}
    }
    if tool.repeat_count > 1 {
        spans.push(Span::styled(
            format!(" ({}x)", tool.repeat_count),
            Style::default().fg(crate::render::theme::quiet()),
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
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(palette::muted_fg()),
    )];
    if let Some(total) = number_field(&tool.result.content, "total_matches")
        .or_else(|| number_field(&tool.result.content, "returned_matches"))
    {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{total} matches"),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn semantic_tool_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let label = tool_call_label_or_name(tool);
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(palette::muted_fg()),
    )];
    if let Some(matches) = number_field(&tool.result.content, "total_matches")
        .or_else(|| number_field(&tool.result.content, "returned_matches"))
        .or_else(|| {
            tool.result.content["packets"]
                .as_array()
                .map(|items| items.len() as u64)
        })
    {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{matches} matches"),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    spans
}

fn repo_map_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        "repo map",
        Style::default().fg(palette::muted_fg()),
    )];
    if let Some(files) = tool.result.content["stats"]["files"].as_u64() {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{files} files"),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    if let Some(symbols) = tool.result.content["stats"]["symbols"].as_u64() {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{symbols} symbols"),
            Style::default().fg(crate::render::theme::secondary()),
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
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(palette::muted_fg()),
    )];
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
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{matches} matches"),
            Style::default().fg(crate::render::theme::secondary()),
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
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(palette::muted_fg()),
    )];
    if let Some(paths) = tool.result.content["paths"]
        .as_array()
        .map(|items| items.len() as u64)
    {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{paths} paths"),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn read_file_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let label = tool_call_label_or_name(tool);
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(palette::muted_fg()),
    )];
    if let Some(bytes) = number_field(&tool.result.content, "bytes_returned") {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format_bytes(bytes),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    } else if let Some(ranges) = tool.result.content["ranges"]
        .as_array()
        .map(|items| items.len() as u64)
    {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{ranges} ranges"),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn read_tool_output_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let label = saved_tool_output_meta(tool)
        .map(|meta| match meta.tool_name.as_deref() {
            Some(name) => format!("expand saved {}", saved_tool_output_label(name)),
            None => "expand saved tool output".to_string(),
        })
        .or_else(|| {
            saved_compiler_output_summary(tool).map(|_| "expand saved compiler output".to_string())
        })
        .unwrap_or_else(|| "expand saved tool output".to_string());
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(palette::muted_fg()),
    )];
    if let Some(bytes) = number_field(&tool.result.content, "bytes_returned") {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format_bytes(bytes),
            Style::default().fg(crate::render::theme::secondary()),
        ));
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
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(palette::muted_fg()),
    )];
    let additions = files.iter().map(|file| file.additions).sum::<u64>();
    let deletions = files.iter().map(|file| file.deletions).sum::<u64>();
    if additions > 0 || deletions > 0 {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("+{additions} -{deletions}"),
            Style::default().fg(crate::render::theme::quiet()),
        ));
    } else if let Some(count) = number_field(&tool.result.content, "matches") {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{count} matches"),
            Style::default().fg(crate::render::theme::secondary()),
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

const WRITE_FILE_DIFF_PREVIEW_LINES: usize = 200;

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
    if files.is_empty()
        && let Some(patch) = tool.result.content["unified_diff"]
            .as_str()
            .filter(|patch| !patch.trim().is_empty())
    {
        files.extend(edit_files_from_unified_diff(
            patch,
            edit_file_paths(&tool.result.content),
        ));
    }
    if files.is_empty()
        && let Some(file) = edit_file_from_write_call(tool)
    {
        files.push(file);
    }
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

fn edit_file_from_write_call(tool: &ToolTranscript) -> Option<EditChangedFile> {
    if tool.result.tool_name != "write_file" {
        return None;
    }
    let call = tool.call.as_ref()?;
    let path =
        string_arg(&tool.result.content, "path").or_else(|| string_arg(&call.arguments, "path"))?;
    if !tool
        .result
        .content
        .get("before_sha256")
        .is_some_and(|value| value.is_null())
    {
        return Some(EditChangedFile {
            path,
            additions: 0,
            deletions: 0,
            patch: None,
            patch_truncated: false,
        });
    }
    let content = call.arguments.get("content")?.as_str()?;
    let patch = render_write_file_preview_diff(&path, content)?;
    let additions = patch
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .count() as u64;
    let total_lines = content.lines().count();

    Some(EditChangedFile {
        path,
        additions,
        deletions: 0,
        patch: Some(patch),
        patch_truncated: total_lines > WRITE_FILE_DIFF_PREVIEW_LINES,
    })
}

fn render_write_file_preview_diff(path: &str, content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let mut out = format!("--- a/{path}\n+++ b/{path}\n@@ -0,0 +1 @@\n");
    for line in content.lines().take(WRITE_FILE_DIFF_PREVIEW_LINES) {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

fn edit_file_paths(content: &serde_json::Value) -> Vec<String> {
    content["files"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["path"].as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn edit_files_from_unified_diff(patch: &str, fallback_paths: Vec<String>) -> Vec<EditChangedFile> {
    let mut files = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();
    let mut fallback_index = 0;

    for line in patch.lines() {
        if line.starts_with("--- ") && !current_lines.is_empty() {
            push_edit_diff_file(
                &mut files,
                current_path.take(),
                &fallback_paths,
                &mut fallback_index,
                std::mem::take(&mut current_lines),
            );
        }
        if line.starts_with("+++ ") {
            current_path = diff_header_path(line).or_else(|| current_path.take());
        } else if line.starts_with("--- ") {
            current_path = diff_header_path(line);
        }
        current_lines.push(line.to_string());
    }
    push_edit_diff_file(
        &mut files,
        current_path,
        &fallback_paths,
        &mut fallback_index,
        current_lines,
    );

    if files.is_empty() && !patch.trim().is_empty() {
        let path = fallback_paths
            .first()
            .cloned()
            .unwrap_or_else(|| "patch".to_string());
        let (additions, deletions) = count_unified_diff_changes(patch);
        files.push(EditChangedFile {
            path,
            additions,
            deletions,
            patch: Some(patch.to_string()),
            patch_truncated: false,
        });
    }
    files
}

fn push_edit_diff_file(
    files: &mut Vec<EditChangedFile>,
    path: Option<String>,
    fallback_paths: &[String],
    fallback_index: &mut usize,
    lines: Vec<String>,
) {
    if lines.is_empty() {
        return;
    }
    let patch = lines.join("\n");
    let path = path.or_else(|| {
        let value = fallback_paths.get(*fallback_index).cloned();
        *fallback_index += 1;
        value
    });
    let Some(path) = path else {
        return;
    };
    let (additions, deletions) = count_unified_diff_changes(&patch);
    files.push(EditChangedFile {
        path,
        additions,
        deletions,
        patch: Some(patch),
        patch_truncated: false,
    });
}

fn diff_header_path(line: &str) -> Option<String> {
    let path = line
        .strip_prefix("+++ ")
        .or_else(|| line.strip_prefix("--- "))?
        .trim();
    if path == "/dev/null" || path.is_empty() {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_string(),
    )
}

fn count_unified_diff_changes(patch: &str) -> (u64, u64) {
    let additions = patch
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .count() as u64;
    let deletions = patch
        .lines()
        .filter(|line| line.starts_with('-') && !line.starts_with("---"))
        .count() as u64;
    (additions, deletions)
}

fn diff_context_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    let mode = string_arg(&tool.result.content, "mode")
        .map(|mode| format!("diff context ({mode})"))
        .unwrap_or_else(|| "diff context".to_string());
    let mut spans = vec![Span::styled(mode, Style::default().fg(palette::muted_fg()))];
    let files = tool.result.content["summary"]["files_changed"]
        .as_u64()
        .or_else(|| {
            tool.result.content["files"]
                .as_array()
                .map(|items| items.len() as u64)
        });
    if let Some(files) = files {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{files} files"),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    let additions = tool.result.content["summary"]["additions"].as_u64();
    let deletions = tool.result.content["summary"]["deletions"].as_u64();
    if additions.unwrap_or(0) > 0 || deletions.unwrap_or(0) > 0 {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("+{} -{}", additions.unwrap_or(0), deletions.unwrap_or(0)),
            Style::default().fg(crate::render::theme::quiet()),
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
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(palette::muted_fg()),
    )];
    if let Some(symbols) = tool.result.content["symbols"]
        .as_array()
        .map(|items| items.len() as u64)
        .filter(|count| *count > 0)
    {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{symbols} symbols"),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    if let Some(paths) = tool.result.content["impact"]["neighborhood_paths"]
        .as_array()
        .map(|items| items.len() as u64)
        .filter(|count| *count > 0)
    {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(crate::render::theme::quiet()),
        ));
        spans.push(Span::styled(
            format!("{paths} paths"),
            Style::default().fg(crate::render::theme::secondary()),
        ));
    }
    if tool.result.content["graph_available"].as_bool() == Some(false) {
        spans.push(Span::styled(
            " · graph unavailable",
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    append_truncation_hint(&mut spans, tool);
    spans
}

fn web_summary_spans(tool: &ToolTranscript) -> Vec<Span<'static>> {
    vec![Span::styled(
        tool_call_label_or_name(tool),
        Style::default().fg(palette::muted_fg()),
    )]
}

fn tool_call_label_or_name(tool: &ToolTranscript) -> String {
    tool.call
        .as_ref()
        .map(tool_call_label)
        .unwrap_or_else(|| tool.result.tool_name.clone())
}

/// Describe a `verify` call by its scope/level — its real arguments — rather
/// than a `command` field it doesn't have. A model that mistakes `verify` for
/// `shell` and passes `command: "full"` would otherwise surface that stray
/// value as the label; the `prepare_verify_arguments` hook re-homes it, and
/// this reads the structured fields with the tool's own defaults.
fn verify_call_label(call: &ToolCall) -> String {
    let scope = string_arg(&call.arguments, "scope").unwrap_or_else(|| "diff".to_string());
    let level = string_arg(&call.arguments, "level").unwrap_or_else(|| "quick".to_string());
    format!("{scope}/{level}")
}

pub(crate) fn tool_call_label(call: &ToolCall) -> String {
    match call.name.as_str() {
        "shell" => string_arg(&call.arguments, "command")
            .or_else(|| string_arg(&call.arguments, "description"))
            .unwrap_or_else(|| call.name.clone()),
        "verify" => verify_call_label(call),
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
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD),
    );
    let args = active_tool_args(call);
    if args.is_empty() {
        return vec![name_span];
    }
    let mut spans = vec![
        name_span,
        Span::styled(
            ": ",
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if matches!(call.name.as_str(), "shell" | "verify") {
        spans.extend(command_spans(&compact_text(&args, 80)));
    } else {
        spans.push(Span::styled(
            compact_text(&args, 80),
            Style::default().fg(palette::muted_fg()),
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
        Span::styled(" · ", Style::default().fg(crate::render::theme::quiet())),
        Span::styled(
            format!("{secs}s"),
            Style::default().fg(crate::render::theme::quiet()),
        ),
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
        "shell" => string_arg(&call.arguments, "command")
            .or_else(|| string_arg(&call.arguments, "description"))
            .unwrap_or_default(),
        "verify" => verify_call_label(call),
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

/// Title-cased display name for the working-row label (e.g. "Shell: …",
/// "Read: …"). Known tools get explicit casing; unknown tools fall back
/// to ASCII-uppercase first letter so a server-defined tool like
/// `slack_search` reads as `Slack_search` rather than the raw slug.
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
        "symbol_context" => expanded_symbol_context_detail_lines(tool, verbosity),
        "grep" | "glob" | "read_file" | "read_slice" | "read_tool_output" => {
            expanded_read_search_detail_lines(tool, verbosity)
        }
        _ => expanded_generic_tool_detail_lines(tool, verbosity),
    }
}

fn expanded_symbol_context_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let packet_cap = match verbosity {
        ToolOutputVerbosity::Compact => 3,
        ToolOutputVerbosity::Normal => 5,
        ToolOutputVerbosity::Verbose => usize::MAX,
    };
    let mut lines = Vec::new();
    if let Some(call) = tool.call.as_ref()
        && let Some(query) = string_arg(&call.arguments, "query")
    {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("query `{query}`"),
        ));
    }
    let packets = tool.result.content["packets"].as_array();
    let total = packets.map(|p| p.len()).unwrap_or(0);
    if total == 0 {
        if let Some(reason) = string_arg(&tool.result.content, "reason") {
            lines.push(detail_line(false, crate::render::theme::quiet(), reason));
        } else {
            lines.push(detail_line(
                false,
                crate::render::theme::quiet(),
                "no symbols matched".to_string(),
            ));
        }
        return lines;
    }
    lines.push(detail_line(
        false,
        crate::render::theme::quiet(),
        format!("packets {total}"),
    ));
    if let Some(packets) = packets {
        for packet in packets.iter().take(packet_cap) {
            let name = packet["name"].as_str().unwrap_or("?");
            let kind = packet["kind"].as_str().unwrap_or("");
            let path = packet["path"].as_str().unwrap_or("?");
            let line = packet["span"]["start_line"].as_u64().unwrap_or(0);
            let refs = packet["references"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            let callers = packet["callers"].as_array().map(|a| a.len()).unwrap_or(0);
            let kind_suffix = if kind.is_empty() {
                String::new()
            } else {
                format!(" ({kind})")
            };
            let counts = if refs == 0 && callers == 0 {
                String::new()
            } else {
                format!(" · refs {refs} · callers {callers}")
            };
            lines.push(detail_line(
                false,
                crate::render::theme::quiet(),
                format!("{path}:{line} {name}{kind_suffix}{counts}"),
            ));
        }
    }
    if total > packet_cap {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!(
                "+{} more packets (Ctrl-T for full transcript)",
                total - packet_cap
            ),
        ));
    }
    lines
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
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("cwd {workdir}"),
        ));
    }
    if tool.result.status != ToolStatus::Success
        && let Some(exit_code) = tool
            .result
            .content
            .get("exit_code")
            .and_then(|value| value.as_i64())
    {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("exit {exit_code}"),
        ));
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
    let is_diff = shell_text_looks_like_diff(&output);
    let mut lines = vec![shell_output_title_line(&command, &workdir)];
    lines.extend(head_tail_lines(&output, limit).into_iter().map(|line| {
        if line.truncated_marker {
            detail_line(false, crate::render::theme::quiet(), line.text)
        } else if is_diff {
            shell_output_diff_line(&line.text)
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
            "│ ",
            Style::default()
                .fg(crate::render::theme::quiet())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(command_spans(command));
    spans.push(Span::styled(
        " in ",
        Style::default().fg(crate::render::theme::quiet()),
    ));
    spans.push(Span::styled(
        workdir.to_string(),
        Style::default().fg(palette::muted_fg()),
    ));
    spans.push(Span::styled(
        ":",
        Style::default().fg(crate::render::theme::quiet()),
    ));
    Line::from(spans)
}

fn shell_output_line(content: &str) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(styled_output_spans(content));
    Line::from(spans)
}

fn shell_text_looks_like_diff(content: &str) -> bool {
    let mut saw_rustfmt_header = false;
    let mut saw_change = false;
    for line in content.lines() {
        let stripped = strip_ansi_escape_sequences(line);
        let trimmed = stripped.trim_start();
        if trimmed.starts_with("@@ -") && trimmed.contains(" @@") {
            return true;
        }
        if trimmed.starts_with("Diff in ") {
            saw_rustfmt_header = true;
        }
        if is_diff_change_line(trimmed) {
            saw_change = true;
        }
    }
    saw_rustfmt_header && saw_change
}

fn is_diff_change_line(trimmed: &str) -> bool {
    (trimmed.starts_with('+') && !trimmed.starts_with("+++"))
        || (trimmed.starts_with('-') && !trimmed.starts_with("---"))
}

/// Diff-aware variant of [`shell_output_line`]. Lines that begin with `+`
/// or `-` (but NOT `+++` / `---` file headers) get a tinted background to
/// mirror the inline-diff styling reviewers expect from GitHub / Claude
/// Code. Hunk headers (`@@ ... @@`) get a muted accent so the eye still
/// finds the section breaks; everything else falls through to the default
/// styled output.
fn shell_output_diff_line(content: &str) -> Line<'static> {
    diff_output_line(content, vec![Span::raw("  ")])
}

fn detail_output_diff_line(content: &str) -> Line<'static> {
    diff_output_line(
        content,
        vec![
            Span::raw("  "),
            Span::styled(
                "│ ",
                Style::default()
                    .fg(crate::render::theme::quiet())
                    .add_modifier(Modifier::BOLD),
            ),
        ],
    )
}

fn diff_output_line(content: &str, mut spans: Vec<Span<'static>>) -> Line<'static> {
    let content = strip_ansi_escape_sequences(content);
    let trimmed = content.trim_start_matches(' ');
    let leading_ws = &content[..content.len() - trimmed.len()];
    if !leading_ws.is_empty() {
        spans.push(Span::raw(leading_ws.to_string()));
    }
    let style = if trimmed.starts_with("+++") || trimmed.starts_with("---") {
        Style::default().fg(palette::muted_fg())
    } else if let Some(rest) = trimmed.strip_prefix('+') {
        let bg = render::diff::diff_add_bg();
        let style = Style::default().bg(bg);
        spans.push(Span::styled("+".to_string(), style));
        spans.push(Span::styled(rest.to_string(), style));
        let mut line = Line::from(spans);
        line = line.style(Style::default().bg(bg));
        return line;
    } else if let Some(rest) = trimmed.strip_prefix('-') {
        let bg = render::diff::diff_del_bg();
        let style = Style::default().bg(bg);
        spans.push(Span::styled("-".to_string(), style));
        spans.push(Span::styled(rest.to_string(), style));
        let mut line = Line::from(spans);
        line = line.style(Style::default().bg(bg));
        return line;
    } else if trimmed.starts_with("@@") {
        Style::default()
            .fg(crate::render::theme::magenta())
            .add_modifier(Modifier::BOLD)
    } else {
        spans.extend(styled_output_spans(trimmed));
        return Line::from(spans);
    };
    spans.push(Span::styled(trimmed.to_string(), style));
    Line::from(spans)
}

fn strip_ansi_escape_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some('(' | ')' | '*' | '+' | '-' | '.' | '/') => {
                let _ = chars.next();
            }
            Some(_) | None => {}
        }
    }
    output
}

fn expanded_decl_search_detail_lines(
    tool: &ToolTranscript,
    _verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(total) = number_field(&tool.result.content, "total_matches") {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("total matches {total}"),
        ));
    }
    if let Some(returned) = number_field(&tool.result.content, "returned_matches") {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("shown matches {returned}"),
        ));
    }
    if let Some(languages) = compact_json_object(&tool.result.content["counts_by_language"]) {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("languages {languages}"),
        ));
    }
    if let Some(kinds) = compact_json_object(&tool.result.content["counts_by_kind"]) {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("kinds {kinds}"),
        ));
    }
    lines
}

fn expanded_repo_map_detail_lines(tool: &ToolTranscript) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(files) = tool.result.content["stats"]["files"].as_u64() {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("files {files}"),
        ));
    }
    if let Some(symbols) = tool.result.content["stats"]["symbols"].as_u64() {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("symbols {symbols}"),
        ));
    }
    if let Some(languages) = compact_json_object(&tool.result.content["languages"]) {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("languages {languages}"),
        ));
    }
    lines
}

fn expanded_diff_context_detail_lines(tool: &ToolTranscript) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(mode) = string_arg(&tool.result.content, "mode") {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("mode {mode}"),
        ));
    }
    let summary = &tool.result.content["summary"];
    if let Some(files) = summary["files_changed"].as_u64() {
        let additions = summary["additions"].as_u64().unwrap_or(0);
        let deletions = summary["deletions"].as_u64().unwrap_or(0);
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
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
            lines.push(detail_line(false, crate::render::theme::quiet(), summary));
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
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("objective {objective}"),
        ));
    }
    if let Some(plan_id) = string_arg(&tool.result.content, "plan_id") {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("plan {plan_id}"),
        ));
    }
    if let Some(symbols) = tool.result.content["symbols"].as_array() {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("symbols {}", symbols.len()),
        ));
    }
    if let Some(paths) = tool.result.content["impact"]["neighborhood_paths"].as_array() {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("paths {}", paths.len()),
        ));
        lines.extend(paths.iter().take(5).filter_map(|path| {
            path.as_str().map(|path| {
                detail_line(false, crate::render::theme::quiet(), format!("path {path}"))
            })
        }));
    }
    if let Some(next) = tool.result.content["next_action"]["reason"].as_str() {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("next {next}"),
        ));
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
        lines.push(detail_line(false, crate::render::theme::quiet(), summary));
        if let Some(patch) = file.patch.as_deref().filter(|patch| !patch.is_empty()) {
            lines.push(detail_line(false, crate::render::theme::quiet(), "diff"));
            lines.extend(render_diff_patch_full_lines(patch, file.path.as_str()));
        }
    }
    if let Some(matches) = number_field(&tool.result.content, "matches") {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("matches {matches}"),
        ));
    }
    if let Some(contexts) = tool.result.content["match_contexts"].as_array() {
        lines.extend(contexts.iter().take(5).filter_map(|context| {
            let index = context["match_index"].as_u64()?;
            let line = context["line"].as_u64()?;
            let preview = context["preview"].as_str()?;
            Some(detail_line(
                false,
                crate::render::theme::quiet(),
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
        "glob" => expanded_glob_detail_lines_v(tool, verbosity),
        "grep" => expanded_grep_detail_lines_v(tool, verbosity),
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

fn expanded_glob_detail_lines_v(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let path_cap = match verbosity {
        ToolOutputVerbosity::Compact => 3,
        _ => 8,
    };
    let mut lines = Vec::new();
    if let Some(pattern) = string_arg(&tool.result.content["metadata"], "pattern") {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("pattern {pattern}"),
        ));
    }
    if !matches!(verbosity, ToolOutputVerbosity::Compact)
        && let Some(path) = string_arg(&tool.result.content["metadata"], "path")
    {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("root {path}"),
        ));
    }
    if let Some(paths) = tool.result.content["paths"].as_array() {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("paths {}", paths.len()),
        ));
        lines.extend(paths.iter().take(path_cap).filter_map(|path| {
            path.as_str().map(|path| {
                detail_line(false, crate::render::theme::quiet(), format!("path {path}"))
            })
        }));
    }
    lines
}

fn expanded_grep_detail_lines_v(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    let match_cap = match verbosity {
        ToolOutputVerbosity::Compact => 3,
        _ => 6,
    };
    let path_cap = match verbosity {
        ToolOutputVerbosity::Compact => 3,
        _ => 8,
    };
    let mut lines = Vec::new();
    if let Some(pattern) = string_arg(&tool.result.content["metadata"], "pattern") {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("pattern {pattern}"),
        ));
    }
    if !matches!(verbosity, ToolOutputVerbosity::Compact)
        && let Some(path) = string_arg(&tool.result.content["metadata"], "path")
    {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("root {path}"),
        ));
    }
    if let Some(count) = number_field(&tool.result.content, "count") {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("matches {count}"),
        ));
    }
    lines.extend(path_detail_lines(
        &tool.result.content["paths"],
        "",
        path_cap,
    ));
    if let Some(matches) = tool.result.content["matches"].as_array() {
        for item in matches.iter().take(match_cap) {
            let path = item["path"].as_str().unwrap_or("?");
            let line = item["line"].as_u64().unwrap_or(0);
            let text = item["text"].as_str().unwrap_or_default();
            lines.push(detail_line(
                false,
                crate::render::theme::quiet(),
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
    let path = string_arg(&tool.result.content, "path");
    let bytes = number_field(&tool.result.content, "bytes_returned");
    let total = number_field(&tool.result.content, "total_bytes");
    let ranges = tool.result.content["ranges"]
        .as_array()
        .map(|r| r.len())
        .unwrap_or(0);

    // Compact mode: a single chip summarising the read. The content body
    // and per-field chips dominate transcripts otherwise — and Read's full
    // text is already what the agent acted on, not something the user
    // needs to re-read here.
    if matches!(verbosity, ToolOutputVerbosity::Compact) {
        let mut summary = path.clone().unwrap_or_else(|| "?".to_string());
        if let Some(bytes) = bytes {
            let total = total.unwrap_or(bytes);
            summary.push_str(&format!(
                " · {} of {}",
                format_bytes(bytes),
                format_bytes(total)
            ));
        }
        if ranges > 1 {
            summary.push_str(&format!(" · {ranges} ranges"));
        }
        return vec![detail_line(false, crate::render::theme::quiet(), summary)];
    }

    let mut lines = Vec::new();
    if let Some(path) = path {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("path {path}"),
        ));
    }
    if let Some(bytes) = bytes {
        let total = total.unwrap_or(bytes);
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("bytes {} of {}", format_bytes(bytes), format_bytes(total)),
        ));
    }
    if ranges > 0 {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("ranges {ranges}"),
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
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("handle {handle}"),
        ));
    }
    if let Some(bytes) = number_field(&tool.result.content, "bytes_returned") {
        let total = number_field(&tool.result.content, "total_bytes").unwrap_or(bytes);
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("bytes {} of {}", format_bytes(bytes), format_bytes(total)),
        ));
    }
    if let Some(saved) = saved_tool_output_meta(tool) {
        let label = saved
            .tool_name
            .as_deref()
            .map(saved_tool_output_label)
            .unwrap_or_else(|| "tool output".to_string());
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("saved {label}"),
        ));
        if let Some(result) = saved.parsed {
            let nested = ToolTranscript {
                call: None,
                result,
                repeat_count: 1,
            };
            lines.extend(expanded_tool_detail_lines(&nested, verbosity));
            return lines;
        }
        if !matches!(verbosity, ToolOutputVerbosity::Verbose) {
            let detail = if saved.partial {
                "content saved tool-result JSON (partial; hidden in normal mode)"
            } else {
                "content saved tool-result JSON (hidden in normal mode)"
            };
            lines.push(detail_line(false, crate::render::theme::quiet(), detail));
            return lines;
        }
    }
    if let Some(summary) = saved_compiler_output_summary(tool) {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            "saved compiler output",
        ));
        if !summary.messages.is_empty() {
            lines.push(detail_line(
                false,
                crate::render::theme::quiet(),
                "compiler messages",
            ));
            for line in head_tail_lines(
                &summary.messages.join("\n"),
                saved_output_preview_limit(verbosity),
            ) {
                if line.truncated_marker {
                    lines.push(detail_line(false, crate::render::theme::quiet(), line.text));
                } else {
                    lines.push(detail_spans_line(styled_output_spans(&line.text)));
                }
            }
        }
        if !summary.stderr.is_empty() {
            lines.push(detail_line(false, crate::render::theme::quiet(), "stderr"));
            for line in head_tail_lines(
                &summary.stderr.join("\n"),
                saved_output_preview_limit(verbosity),
            ) {
                if line.truncated_marker {
                    lines.push(detail_line(false, crate::render::theme::quiet(), line.text));
                } else {
                    lines.push(detail_spans_line(styled_output_spans(&line.text)));
                }
            }
        }
        if !matches!(verbosity, ToolOutputVerbosity::Verbose) {
            let mut detail = format!(
                "compiler JSON hidden in normal mode ({} message lines)",
                summary.hidden_json_lines
            );
            if summary.partial {
                detail.push_str(" · partial result");
            }
            lines.push(detail_line(false, crate::render::theme::quiet(), detail));
            return lines;
        }
    }
    if let Some(content) = string_arg(&tool.result.content, "content") {
        // Spilled tool payloads are routinely hundreds of lines of raw JSON,
        // so fold inline even on the expanded card — the overlay (which
        // pins Verbose) still hands the full content to anyone hitting
        // Ctrl-T. Generic `output_block_lines` stays unbounded so the
        // existing "expand grep → see every match" behaviour is preserved.
        let limit = saved_output_preview_limit(verbosity);
        let byte_limit = saved_output_line_byte_limit(verbosity);
        if !content.trim().is_empty() {
            lines.push(detail_line(false, crate::render::theme::quiet(), "content"));
            for line in head_tail_lines(&content, limit) {
                if line.truncated_marker {
                    lines.push(detail_line(false, crate::render::theme::quiet(), line.text));
                } else {
                    // Clamp each line by bytes: spilled payloads are routinely
                    // minified single-line JSON, which the line-count cap above
                    // can't bound — one such line would otherwise wrap into
                    // hundreds of rows. Verbose (the Ctrl-T overlay) is left
                    // unbounded so the full content stays available.
                    let text = truncate_bytes(&line.text, byte_limit);
                    lines.push(detail_spans_line(styled_output_spans(&text)));
                }
            }
        }
    }
    lines
}

fn saved_output_preview_limit(verbosity: ToolOutputVerbosity) -> usize {
    match verbosity {
        ToolOutputVerbosity::Compact => 12,
        ToolOutputVerbosity::Normal => 40,
        ToolOutputVerbosity::Verbose => usize::MAX,
    }
}

fn saved_output_line_byte_limit(verbosity: ToolOutputVerbosity) -> usize {
    match verbosity {
        ToolOutputVerbosity::Compact => TOOL_PREVIEW_COMPACT_BYTES,
        ToolOutputVerbosity::Normal => TOOL_PREVIEW_NORMAL_BYTES,
        ToolOutputVerbosity::Verbose => usize::MAX,
    }
}

#[derive(Debug)]
struct SavedToolOutputMeta {
    tool_name: Option<String>,
    parsed: Option<ToolResult>,
    partial: bool,
}

fn saved_tool_output_meta(tool: &ToolTranscript) -> Option<SavedToolOutputMeta> {
    if tool.result.tool_name != "read_tool_output" {
        return None;
    }
    // Cargo/compiler JSON streams have a dedicated summarizer; yield to it
    // rather than folding them as a generic saved tool-result.
    if saved_compiler_output_summary(tool).is_some() {
        return None;
    }
    let content = string_arg(&tool.result.content, "content")?;
    let trimmed = content.trim_start();
    // Spilled tool results are written in the `model_output()` shape
    // (`{"status":..,"content":..}`), which carries no `tool_name`. Treat any
    // JSON object/array body as a saved tool-result so it folds to a receipt
    // instead of splatting raw JSON across the scrollback; the inner tool name
    // is best-effort and only used to label the receipt.
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let parsed = serde_json::from_str::<ToolResult>(trimmed).ok();
    let tool_name = parsed
        .as_ref()
        .map(|result| result.tool_name.clone())
        .or_else(|| json_string_field_prefix(trimmed, "tool_name"));
    let partial = tool.result.content["truncated"].as_bool().unwrap_or(false) || parsed.is_none();
    Some(SavedToolOutputMeta {
        tool_name,
        parsed,
        partial,
    })
}

fn json_string_field_prefix(source: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let start = source.find(&needle)? + needle.len();
    let mut rest = source[start..].trim_start();
    rest = rest.strip_prefix(':')?.trim_start();
    let mut deserializer = serde_json::Deserializer::from_str(rest);
    String::deserialize(&mut deserializer).ok()
}

fn saved_tool_output_label(tool_name: &str) -> String {
    match tool_name {
        "diff_context" => "diff context".to_string(),
        "repo_map" => "repo map".to_string(),
        "decl_search" => "declarations".to_string(),
        "symbol_context" => "symbol context".to_string(),
        "read_file" | "read_slice" => "read".to_string(),
        "apply_patch" => "patch".to_string(),
        "write_file" => "write".to_string(),
        other => other.replace('_', " "),
    }
}

#[derive(Debug)]
struct SavedCompilerOutputSummary {
    messages: Vec<String>,
    stderr: Vec<String>,
    hidden_json_lines: usize,
    partial: bool,
}

fn saved_compiler_output_summary(tool: &ToolTranscript) -> Option<SavedCompilerOutputSummary> {
    if tool.result.tool_name != "read_tool_output" {
        return None;
    }
    let content = string_arg(&tool.result.content, "content")?;
    let partial = tool.result.content["truncated"].as_bool().unwrap_or(false);
    compiler_output_summary(&content, partial)
}

fn compiler_output_summary(content: &str, partial: bool) -> Option<SavedCompilerOutputSummary> {
    let mut messages = Vec::new();
    let mut stderr = Vec::new();
    let mut hidden_json_lines = 0usize;
    let mut saw_compiler_stream = false;
    let mut in_stderr = false;

    for raw in content.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed == "===== stderr =====" {
            in_stderr = true;
            continue;
        }
        if trimmed.starts_with("=====") {
            in_stderr = false;
            continue;
        }
        if in_stderr {
            if !line.trim().is_empty() {
                stderr.push(line.to_string());
            }
            continue;
        }
        if let Some(message) = cargo_json_message_line(trimmed) {
            saw_compiler_stream = true;
            if !message.is_empty() {
                messages.extend(message.lines().map(ToOwned::to_owned));
            }
            hidden_json_lines += 1;
        } else if looks_like_cargo_json_line(trimmed) {
            saw_compiler_stream = true;
            hidden_json_lines += 1;
        }
    }

    if saw_compiler_stream || !stderr.is_empty() {
        Some(SavedCompilerOutputSummary {
            messages,
            stderr,
            hidden_json_lines,
            partial,
        })
    } else {
        None
    }
}

fn cargo_json_message_line(line: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    match value["reason"].as_str()? {
        "compiler-message" => value["message"]["rendered"]
            .as_str()
            .or_else(|| value["message"]["message"].as_str())
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn looks_like_cargo_json_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    if line.contains("\"reason\":\"compiler-artifact\"")
        || line.contains("\"reason\":\"build-script-executed\"")
        || line.contains("\"reason\":\"build-finished\"")
        || line.contains("\"package_id\":\"registry+https://github.com/rust-lang/crates.io-index#")
    {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| value["reason"].as_str().map(str::to_owned))
        .is_some_and(|reason| {
            matches!(
                reason.as_str(),
                "compiler-artifact" | "build-script-executed" | "build-finished"
            )
        })
}

fn expanded_generic_tool_detail_lines(
    tool: &ToolTranscript,
    verbosity: ToolOutputVerbosity,
) -> Vec<Line<'static>> {
    // Verbose mode preserves the legacy `details {...}` JSON dump so users
    // who really want the raw payload (debugging a new tool, comparing two
    // runs) can still get it via `/verbosity verbose`.
    if matches!(verbosity, ToolOutputVerbosity::Verbose) {
        let preview = preview_tool_result(&tool.result, verbosity);
        return output_block_lines("details", &preview, verbosity);
    }

    // 1. If the tool surfaces plain stdout/stderr/output, render that —
    //    walls of JSON aren't useful when the tool already gives us text.
    if let Some(text) = tool_result_output_text(&tool.result) {
        let preview = truncate_bytes(&text, TOOL_PREVIEW_COMPACT_BYTES);
        return output_block_lines("output", &preview, verbosity);
    }

    // 2. Otherwise summarise the top-level keys of the result. Each value
    //    gets at most one chip; the entry caps at 3 chips total so the
    //    summary stays scannable. Full JSON is one verbosity flip away.
    let Some(object) = tool.result.content.as_object() else {
        let preview = preview_tool_result(&tool.result, verbosity);
        return output_block_lines("details", &preview, verbosity);
    };
    if object.is_empty() {
        return Vec::new();
    }
    let max_chips = if matches!(verbosity, ToolOutputVerbosity::Compact) {
        3
    } else {
        6
    };
    let mut lines = Vec::new();
    let mut shown = 0usize;
    let total_keys = object.len();
    for (key, value) in object {
        if shown >= max_chips {
            break;
        }
        let summary = summarize_json_value(value);
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!("{key} {summary}"),
        ));
        shown += 1;
    }
    if total_keys > shown {
        lines.push(detail_line(
            false,
            crate::render::theme::quiet(),
            format!(
                "+{} more fields (Ctrl-T for full transcript)",
                total_keys - shown
            ),
        ));
    }
    lines
}

/// One-line summary of a JSON value, used by the generic tool renderer.
/// Strings get inline-quoted (truncated); arrays/objects collapse to a
/// count; scalars print as-is.
fn summarize_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("\"{}\"", compact_text(s, 80)),
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                "[]".to_string()
            } else {
                format!("{} items", items.len())
            }
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                "{}".to_string()
            } else {
                let keys = map.keys().take(3).cloned().collect::<Vec<_>>().join(", ");
                let suffix = if map.len() > 3 {
                    format!(", +{}", map.len() - 3)
                } else {
                    String::new()
                };
                format!("{{{keys}{suffix}}}")
            }
        }
    }
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
    let is_diff = shell_text_looks_like_diff(content);
    let mut rendered = vec![detail_line(false, crate::render::theme::quiet(), label)];
    rendered.extend(lines.into_iter().map(|line| {
        if line.truncated_marker {
            detail_line(false, crate::render::theme::quiet(), line.text)
        } else if is_diff {
            detail_output_diff_line(&line.text)
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
        text: format!("… +{omitted} lines (Ctrl-T for full transcript)"),
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
            "│ ",
            Style::default()
                .fg(crate::render::theme::quiet())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(content);
    Line::from(spans)
}

fn detail_rendered_line(line: Line<'static>) -> Line<'static> {
    let style = line.style;
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            "│ ",
            Style::default()
                .fg(crate::render::theme::quiet())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(line.spans);
    Line::from(spans).style(style)
}

fn command_spans(command: &str) -> Vec<Span<'static>> {
    let tokens = command
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return vec![Span::styled(
            command.to_string(),
            Style::default().fg(palette::muted_fg()),
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
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD)
        } else if token.starts_with('-') {
            Style::default().fg(crate::render::theme::accent())
        } else if token.starts_with('"') || token.starts_with('\'') {
            Style::default().fg(crate::render::theme::green())
        } else if token.contains('/') || token.contains('.') {
            Style::default().fg(palette::muted_fg())
        } else {
            Style::default().fg(crate::render::theme::quiet())
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
            spans.push(Span::styled(
                ch.to_string(),
                Style::default().fg(crate::render::theme::quiet()),
            ));
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
        Style::default()
            .fg(crate::render::theme::red())
            .add_modifier(Modifier::BOLD)
    } else if matches!(lower.as_str(), "warning" | "warn") {
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD)
    } else if matches!(lower.as_str(), "ok" | "passed" | "success" | "done") {
        Style::default()
            .fg(crate::render::theme::green())
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
        Style::default()
            .fg(crate::render::theme::secondary())
            .add_modifier(Modifier::BOLD)
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
        .map(|path| detail_line(false, crate::render::theme::quiet(), format!("path {path}")))
        .collect()
}

fn append_truncation_hint(spans: &mut Vec<Span<'static>>, tool: &ToolTranscript) {
    // Two distinct truncation modes share `cost_hint.truncated`:
    //   * Spill: result was too large for the model context, so the full
    //     payload was written to disk under `.squeezy/tool_outputs/<sha>`.
    //     The model gets a handle it can pass to `read_tool_output`. The
    //     card body shows a preview; the rest is NOT in the transcript
    //     and Ctrl-T can't surface it, so name the file directly — that
    //     is the only escape hatch a curious user has.
    //   * Tool-cap: the tool itself returned a partial slice (repo_map
    //     packet cap, decl_search row cap, etc.). The model needs to
    //     re-query with narrower filters to see more.
    // The old shared "more available" label promised something Ctrl-T
    // couldn't deliver — distinguish both so the affordance matches reality.
    let spilled = tool.result.content["spilled"].as_bool().unwrap_or(false);
    if spilled {
        let path = tool.result.content["on_disk_path"]
            .as_str()
            .map(|p| compact_path(std::path::Path::new(p)))
            .unwrap_or_else(|| "spilled to disk".to_string());
        spans.push(Span::styled(
            format!(" · saved {path}"),
            Style::default().fg(crate::render::theme::quiet()),
        ));
        return;
    }
    if tool.result.cost_hint.truncated
        || tool.result.content["truncated"].as_bool().unwrap_or(false)
    {
        spans.push(Span::styled(
            " · partial result",
            Style::default().fg(crate::render::theme::quiet()),
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
    // Informational tool results (no error / reason / stderr, no exit
    // code) can still carry a structured `message` summarising the
    // outcome — e.g. an empty-store undo's "nothing to undo". Surface
    // that here so the detail line stays actionable instead of
    // tombstoning to "no output" whenever this helper is consulted off
    // the failure path.
    if let Some(message) = result
        .content
        .get("message")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return compact_text(message, 140);
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
        ToolStatus::Success => crate::render::theme::green(),
        ToolStatus::Error | ToolStatus::Stale => crate::render::theme::red(),
        ToolStatus::Denied | ToolStatus::Cancelled => crate::render::theme::secondary(),
    }
}

fn tool_result_display_color(tool: &ToolTranscript) -> Color {
    if tool_result_not_run(tool) || is_retryable_tool_result(&tool.result) {
        crate::render::theme::secondary()
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
            Self::Idle => crate::render::theme::accent(),
            Self::Running => {
                if tick % 8 < 4 {
                    crate::render::theme::secondary()
                } else {
                    crate::render::theme::accent()
                }
            }
            Self::Succeeded => crate::render::theme::green(),
            Self::Failed => crate::render::theme::red(),
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
    // At idle the indicator is a steady amber crescent moon. Animating it
    // forced a real cell change every 320 ms, which kept terminal-emulator
    // per-tab activity indicators buzzing forever even though the agent was
    // doing nothing.
    if app.turn_visual == TurnVisualState::Idle {
        return Span::styled(
            "☽",
            Style::default()
                .fg(crate::render::theme::accent())
                .add_modifier(Modifier::BOLD),
        );
    }
    let color = if (prompt_elapsed_ms(app) / 800).is_multiple_of(2) {
        crate::render::theme::secondary()
    } else {
        crate::render::theme::accent()
    };
    Span::styled(
        prompt_coin_frame(app),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn prompt_coin_frame(app: &TuiApp) -> &'static str {
    // A full lunar cycle (new → waxing crescent → first quarter → full →
    // last quarter → waning crescent) so the coin turns through real
    // phases, matching the header band.
    const FRAMES: [&str; 6] = ["○", "☽", "◑", "●", "◐", "☾"];
    if app.turn_visual == TurnVisualState::Idle {
        return "☽";
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
    Span::styled("┃", Style::default().fg(crate::render::theme::accent()))
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
    let queue_overlay_lines = app
        .prompt_queue_overlay
        .as_ref()
        .map(|state| prompt_queue::render_lines(state, &app.prompt_queue).len())
        .unwrap_or(0);
    let overlay_lines = if queue_overlay_lines > 0 {
        0
    } else {
        app.overlay
            .as_ref()
            .map(|o| o.render_lines().len())
            .unwrap_or(0)
    };
    let mention_lines = if queue_overlay_lines == 0 {
        app.mention_popup
            .as_ref()
            .map(|p| p.matches.len().min(5))
            .unwrap_or(0)
    } else {
        0
    };
    let suggestion_lines = if queue_overlay_lines == 0 && overlay_lines == 0 && mention_lines == 0 {
        // Cap at SLASH_MENU_MAX_ITEMS so the popup_height computation
        // matches what `slash_suggestion_lines` will actually render.
        // Using the unbounded `slash_suggestions().len()` caused the
        // popup_height to factor in items that get clipped, which left
        // no slack for the actual window — and the menu rendered the
        // bottom slice of the SLASH_MENU_MAX_ITEMS window instead of
        // the top.
        slash_suggestion_lines(app, width).len()
    } else {
        0
    };
    // The indicator strip is always reserved 1 row when the queue is
    // non-empty so the click target stays put whether the reorder
    // overlay is open or closed.
    let indicator_lines = if app.prompt_queue.is_empty() { 0 } else { 1 };
    // Include the slash-suggestion height in the popup_height budget so
    // `max_height` grows past `PROMPT_MAX_HEIGHT` when the menu needs
    // more vertical room. Without this, a long match list collapses to
    // the 8-row prompt cap and clips the top of the menu.
    let popup_height =
        queue_overlay_lines + overlay_lines + mention_lines + suggestion_lines + indicator_lines;
    let max_height = (PROMPT_MAX_HEIGHT as usize).max(popup_height + PROMPT_MIN_HEIGHT as usize);
    prompt_visual_line_count(&app.input, width)
        .saturating_add(3)
        .saturating_add(queue_overlay_lines)
        .saturating_add(overlay_lines)
        .saturating_add(mention_lines)
        .saturating_add(suggestion_lines)
        .saturating_add(indicator_lines)
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
            prompt_coin_span(app),
            Span::raw("  "),
            prompt_cursor_span(),
        ])];
    }
    let cursor = input_cursor(app);
    let parts = app.input.split('\n').collect::<Vec<_>>();
    let slash_ranges = input::slash_command_ranges(&app.input);
    let bang_range = bang_command_marker_range(&app.input);
    let mut line_start = 0usize;
    parts
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 {
                vec![prompt_coin_span(app), Span::raw("  ")]
            } else {
                vec![Span::raw("   ")]
            };
            let mut spans = prefix;
            let line_end = line_start + line.len();
            let style_text_at = |abs_offset: usize| -> Style {
                if bang_range
                    .as_ref()
                    .is_some_and(|range| range.contains(&abs_offset))
                {
                    Style::default().fg(crate::render::theme::red())
                } else if let Some(style) = prompt_attachment_style_at(app, abs_offset) {
                    style
                } else if slash_ranges
                    .iter()
                    .any(|(start, end)| *start <= abs_offset && abs_offset < *end)
                {
                    Style::default().fg(crate::render::theme::accent())
                } else {
                    Style::default().fg(crate::render::theme::foreground())
                }
            };
            if cursor >= line_start && cursor <= line_end {
                let split_at = cursor.saturating_sub(line_start).min(line.len());
                let (before, after) = line.split_at(split_at);
                if !before.is_empty() {
                    push_styled_segments(&mut spans, before, line_start, style_text_at);
                }
                spans.push(prompt_cursor_span());
                if !after.is_empty() {
                    let after_start = line_start + split_at;
                    push_styled_segments(&mut spans, after, after_start, style_text_at);
                }
            } else {
                push_styled_segments(&mut spans, line, line_start, style_text_at);
            }
            line_start = line_end.saturating_add(1);
            Line::from(spans)
        })
        .collect()
}

fn prompt_attachment_style_at(app: &TuiApp, abs_offset: usize) -> Option<Style> {
    for attachment in &app.prompt_attachments {
        for (start, _) in app.input.match_indices(&attachment.placeholder) {
            let end = start + attachment.placeholder.len();
            if (start..end).contains(&abs_offset) {
                return Some(
                    Style::default()
                        .fg(crate::render::theme::cyan())
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
    }
    None
}

/// Push styled spans for `chunk`, splitting anywhere the computed style
/// changes so special command prefixes stay visually distinct from the rest
/// of the prompt.
fn push_styled_segments(
    spans: &mut Vec<Span<'static>>,
    chunk: &str,
    chunk_start: usize,
    style_text_at: impl Fn(usize) -> Style,
) {
    let mut current = String::new();
    let mut current_style: Option<Style> = None;
    for (relative, ch) in chunk.char_indices() {
        let offset = chunk_start + relative;
        let style = style_text_at(offset);
        if current_style.is_some_and(|existing| existing != style) {
            spans.push(Span::styled(
                std::mem::take(&mut current),
                current_style.unwrap(),
            ));
        }
        current_style = Some(style);
        current.push(ch);
    }
    if let Some(style) = current_style {
        spans.push(Span::styled(current, style));
    }
}

fn prompt_input_lines(app: &TuiApp, height: u16, width: u16) -> Vec<Line<'static>> {
    let content = prompt_input_content_lines(app);
    let cursor_line = app.input[..input_cursor(app)].matches('\n').count();
    composer_bubble_lines(content, cursor_line, height, width)
}

fn composer_bubble_lines(
    content: Vec<Line<'static>>,
    cursor_line: usize,
    height: u16,
    width: u16,
) -> Vec<Line<'static>> {
    let width = width as usize;
    let height = height as usize;
    if width < 4 || height < 2 {
        return content;
    }
    let amber = Style::default().fg(crate::render::theme::accent());

    // Open layout: a single top rule with the typed content floating
    // underneath. No vertical sides or bottom rule — the latter added a
    // heavy extra divider below the cursor and made the composer feel boxed
    // in on the default terminal background.
    let rule = Line::from(Span::styled("─".repeat(width), amber));

    let interior_rows = height.saturating_sub(1);
    let mut content = content;
    if content.len() > interior_rows {
        // Pick the visible window so the cursor's line stays on screen.
        // Center the cursor in the window when there's room; clamp to
        // the content's start/end so Up at the top doesn't show phantom
        // rows above line 0 and Down at the bottom doesn't show rows
        // past the last typed line.
        let cursor_line = cursor_line.min(content.len() - 1);
        let half = interior_rows / 2;
        let max_start = content.len() - interior_rows;
        let window_start = cursor_line.saturating_sub(half).min(max_start);
        let window_end = window_start + interior_rows;
        content = content[window_start..window_end].to_vec();
    }

    // Center the content vertically inside the interior so the coin
    // hovers between the rules instead of hugging the top one.
    let spare = interior_rows.saturating_sub(content.len());
    let top_pad = spare / 2;
    let bot_pad = spare - top_pad;

    let mut lines = Vec::with_capacity(height);
    lines.push(rule);
    for _ in 0..top_pad {
        lines.push(Line::from(""));
    }
    for line in content {
        let mut spans = vec![Span::raw(" ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
    for _ in 0..bot_pad {
        lines.push(Line::from(""));
    }
    lines
}

fn slash_suggestion_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let suggestions = input::slash_suggestions_for_app(app);
    let visible = visible_slash_suggestions(&suggestions, app.slash_menu_index);
    let command_width = visible
        .iter()
        .map(|command| command.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(12);
    let task_active = turn_in_progress(app);
    let mut lines = Vec::new();
    for (index, command) in visible.iter().enumerate() {
        let absolute_index =
            slash_menu_window_start(suggestions.len(), app.slash_menu_index).saturating_add(index);
        let selected = absolute_index
            == app
                .slash_menu_index
                .min(suggestions.len().saturating_sub(1));
        let dimmed = command.is_dimmed(task_active);
        let marker = if selected { "› " } else { "  " };
        let command_padding =
            " ".repeat(command_width.saturating_sub(command.name.chars().count()) + 2);
        let name_color = if selected {
            crate::render::theme::secondary()
        } else if dimmed {
            crate::render::theme::foreground()
        } else {
            crate::render::theme::accent()
        };
        let mut name_style = Style::default().fg(name_color);
        if dimmed {
            name_style = name_style.add_modifier(Modifier::ITALIC);
        }
        let mut description_style = Style::default().fg(crate::render::theme::quiet());
        if dimmed {
            description_style = description_style.fg(crate::render::theme::foreground());
        }
        let hint_style = Style::default()
            .fg(crate::render::theme::cyan())
            .add_modifier(Modifier::ITALIC);
        let mut spans = vec![
            Span::styled(
                marker,
                Style::default().fg(if selected {
                    crate::render::theme::secondary()
                } else {
                    crate::render::theme::quiet()
                }),
            ),
            Span::styled(command.name, name_style),
            Span::styled(
                command_padding,
                Style::default().fg(crate::render::theme::quiet()),
            ),
            Span::styled(command.description.to_string(), description_style),
        ];
        let hint_span = command
            .parameter_hint
            .map(|hint| Span::styled(format!(" {hint}"), hint_style));
        let badges = command.capability_badges();
        let badge_span = if badges.is_empty() {
            None
        } else {
            Some(Span::styled(
                format!("  [{}]", badges.join("|")),
                Style::default()
                    .fg(if dimmed {
                        crate::render::theme::foreground()
                    } else {
                        crate::render::theme::accent()
                    })
                    .add_modifier(Modifier::ITALIC),
            ))
        };
        let dimmed_span = if dimmed {
            Some(Span::styled(
                "  (unavailable during turn)",
                Style::default()
                    .fg(crate::render::theme::quiet())
                    .add_modifier(Modifier::ITALIC),
            ))
        } else {
            None
        };
        if let Some(hint_span) = hint_span {
            let mut inline_spans = spans.clone();
            inline_spans.push(hint_span.clone());
            if let Some(badge_span) = badge_span.clone() {
                inline_spans.push(badge_span);
            }
            if let Some(dimmed_span) = dimmed_span.clone() {
                inline_spans.push(dimmed_span);
            }
            if spans_width(&inline_spans) <= width.max(1) as usize {
                lines.push(Line::from(inline_spans));
            } else {
                if let Some(badge_span) = badge_span {
                    spans.push(badge_span);
                }
                if let Some(dimmed_span) = dimmed_span {
                    spans.push(dimmed_span);
                }
                lines.push(Line::from(spans));
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(command_width + 4)),
                    hint_span,
                ]));
            }
        } else {
            if let Some(badge_span) = badge_span {
                spans.push(badge_span);
            }
            if let Some(dimmed_span) = dimmed_span {
                spans.push(dimmed_span);
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
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
    let queue_overlay_lines: Vec<Line<'static>> = app
        .prompt_queue_overlay
        .as_ref()
        .map(|state| prompt_queue::render_lines(state, &app.prompt_queue))
        .unwrap_or_default();
    let queue_open = !queue_overlay_lines.is_empty();
    let overlay_lines = if queue_open {
        Vec::new()
    } else {
        overlay_picker_lines(app)
    };
    let mention_lines = if queue_open || !overlay_lines.is_empty() {
        Vec::new()
    } else {
        mention_popup_lines(app)
    };
    let suggestion_lines = if queue_open || !overlay_lines.is_empty() || !mention_lines.is_empty() {
        Vec::new()
    } else {
        slash_suggestion_lines(app, area.width)
    };
    // Keep the indicator visible even when the overlay is open so the
    // same row stays clickable to toggle it back closed. The glyph
    // switches between `>` and `v` to reflect state.
    let indicator_line =
        prompt_queue::indicator_line(&app.prompt_queue, app.turn_rx.is_some(), queue_open);
    let extra_height = queue_overlay_lines.len()
        + overlay_lines.len()
        + mention_lines.len()
        + suggestion_lines.len()
        + indicator_line.iter().count();
    let prompt_height = area.height.saturating_sub(extra_height as u16);
    let mut lines = prompt_input_lines(app, prompt_height, area.width);
    let indicator_row_offset = lines.len() as u16;
    let indicator_present = indicator_line.is_some();
    if let Some(line) = indicator_line {
        lines.push(line);
    }
    if indicator_present {
        // Indicator sits on `area.y + indicator_row_offset`, spanning
        // the full width. Registered with the per-frame click registry
        // so `handle_mouse` hit-tests it on left-click.
        let row = area.y.saturating_add(indicator_row_offset);
        if row < area.y.saturating_add(area.height) {
            app.register_click(
                Rect {
                    x: area.x,
                    y: row,
                    width: area.width,
                    height: 1,
                },
                ClickAction::ToggleQueueOverlay,
            );
        }
    }
    lines.extend(queue_overlay_lines);
    lines.extend(overlay_lines);
    lines.extend(mention_lines);
    lines.extend(suggestion_lines);
    let scroll = lines.len().saturating_sub(area.height as usize) as u16;
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(crate::render::theme::foreground()))
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
    let mut lines: Vec<Line<'static>> = popup
        .matches
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let selected = index == popup.selected;
            let marker = if selected { "› " } else { "  " };
            let display = path.display().to_string();
            let style = if selected {
                Style::default()
                    .fg(crate::render::theme::secondary())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette::muted_fg())
            };
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected {
                        crate::render::theme::secondary()
                    } else {
                        crate::render::theme::quiet()
                    }),
                ),
                Span::styled(display, style),
            ])
        })
        .collect();
    // `(idx/total)` footer. `total` is the pre-truncation candidate
    // count so the user can see when more matches exist beyond the
    // displayed window (capped at `MAX_MATCHES`). When the workspace walk
    // itself was capped, append a hint that the candidate set is
    // incomplete so a missing match isn't read as "no such file".
    let total = popup.total.max(popup.matches.len());
    let footer = if popup.truncated {
        format!(
            "  {}/{}  (+ more files not shown — refine query)",
            popup.selected + 1,
            total
        )
    } else {
        format!("  {}/{}", popup.selected + 1, total)
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default()
            .fg(crate::render::theme::quiet())
            .add_modifier(Modifier::DIM),
    )));
    lines
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
    } else if app.repo.pending {
        "…"
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
        SessionMode::Plan => crate::render::theme::magenta(),
        SessionMode::Build => crate::render::theme::green(),
    }
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph = Paragraph::new(format_status_lines(app, area.width));
    frame.render_widget(paragraph, area);
}

fn subagent_pane_height(app: &TuiApp) -> u16 {
    if app.subagent_pane.records.is_empty() {
        return 0;
    }
    (1 + app.subagent_pane.records.len()).min(7) as u16
}

fn render_subagent_pane(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    if area.height == 0 {
        return;
    }
    let visible = usize::from(area.height);
    let record_count = app.subagent_pane.records.len();
    // The `main` header is pinned to the top; records scroll within the
    // remaining slots so a high-fanout list overflows without hiding the
    // header. With room for every record we render the full list.
    let record_slots = visible.saturating_sub(1);
    if record_count <= record_slots || record_slots == 0 {
        frame.render_widget(Paragraph::new(subagent_pane_lines(app, area.width)), area);
        return;
    }
    // Records overflow the window. Slide it so the selected record stays
    // visible; when nothing record-level is selected (the default while a
    // turn is in flight and the pane is unfocused), anchor to the newest
    // records so the live view tracks the current fanout instead of pinning
    // to the oldest few. `selected == 0` addresses the `main` row.
    let start = match app.subagent_pane.selected.checked_sub(1) {
        Some(sel) if sel < record_slots => 0,
        Some(sel) => (sel + 1 - record_slots).min(record_count - record_slots),
        None => record_count - record_slots,
    };
    let hidden_above = start;
    let hidden_below = record_count - (start + record_slots);
    let mut main_row = subagent_main_row(app, area.width);
    if hidden_above > 0 {
        main_row.spans.push(Span::styled(
            format!(" · ↑{hidden_above} more"),
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    let mut lines = Vec::with_capacity(record_slots + 1);
    lines.push(main_row);
    for (index, record) in app
        .subagent_pane
        .records
        .iter()
        .enumerate()
        .skip(start)
        .take(record_slots)
    {
        lines.push(subagent_record_row(app, index, record, area.width));
    }
    if hidden_below > 0
        && let Some(last) = lines.last_mut()
    {
        last.spans.push(Span::styled(
            format!(" · +{hidden_below} more"),
            Style::default().fg(crate::render::theme::quiet()),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn subagent_pane_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(subagent_main_row(app, width));
    for (index, record) in app.subagent_pane.records.iter().enumerate() {
        lines.push(subagent_record_row(app, index, record, width));
    }
    lines
}

fn subagent_main_row(app: &TuiApp, width: u16) -> Line<'static> {
    let selected = app.subagent_pane.selected == 0;
    let active = matches!(app.subagent_pane.active, ConversationSource::Main);
    // Glyph encodes selection only: ● = the conversation currently shown,
    // ○ = the others. Run status (for subagent rows) rides on colour + the
    // leading lifecycle word, so the marker no longer conflates the two.
    let glyph = if active { "●" } else { "○" };
    let style = if selected {
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(crate::render::theme::quiet())
    };
    let hint = if selected && app.subagent_pane.focused {
        "↑/↓ switch · Enter scroll · Esc back"
    } else if active {
        "active"
    } else {
        ""
    };
    subagent_pane_row(
        glyph,
        style,
        "main",
        Style::default().fg(crate::render::theme::foreground()),
        hint,
        width,
    )
}

fn subagent_record_row(
    app: &TuiApp,
    index: usize,
    record: &SubagentRecord,
    width: u16,
) -> Line<'static> {
    let row = index + 1;
    let selected = app.subagent_pane.selected == row;
    let active = app.subagent_pane.active == ConversationSource::Subagent(record.id);
    // ● = shown conversation, ○ = others (selection). The lifecycle colour
    // below and the leading lifecycle word carry run status separately.
    let glyph = if active { "●" } else { "○" };
    let glyph_style = Style::default()
        .fg(record.lifecycle.color())
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    let detail = if selected && app.subagent_pane.focused {
        "↑/↓ switch · Enter scroll · Esc back".to_string()
    } else if !record.latest.trim().is_empty() {
        record.latest.clone()
    } else {
        compact_text(&record.prompt, 120)
    };
    let detail = match record.metrics.as_ref() {
        Some(metrics) if !selected || !app.subagent_pane.focused => format!(
            "{} · tools={} bytes={}",
            detail,
            metrics.subagent_tool_calls.max(metrics.tool_calls),
            metrics.subagent_bytes_read.max(metrics.bytes_read)
        ),
        _ => detail,
    };
    // Lead with the lifecycle word so run state survives a monochrome
    // terminal, where the marker only encodes selection (○/●) and colour
    // carries the failed/running/capped state.
    let detail = format!("{} · {}", record.lifecycle.label(), detail);
    // Disambiguate same-kind rows during parallel fanout (e.g. three
    // "delegate" subagents) with their row ordinal.
    let label = format!("{} #{row}", record.agent);
    subagent_pane_row(
        glyph,
        glyph_style,
        &label,
        Style::default().fg(crate::render::theme::foreground()),
        &detail,
        width,
    )
}

fn subagent_pane_row(
    glyph: &str,
    glyph_style: Style,
    label: &str,
    label_style: Style,
    detail: &str,
    width: u16,
) -> Line<'static> {
    let label_width = label.chars().count();
    let reserved = 4 + label_width;
    let detail_width = usize::from(width).saturating_sub(reserved + 2);
    let detail = fit_chars(detail, detail_width);
    Line::from(vec![
        Span::styled(glyph.to_string(), glyph_style),
        Span::raw(" "),
        Span::styled(label.to_string(), label_style),
        Span::raw("  "),
        Span::styled(detail, Style::default().fg(crate::render::theme::quiet())),
    ])
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
    let hints_span = Span::styled(
        format_status_hints(app),
        Style::default().fg(crate::render::theme::quiet()),
    );
    let detail = configured_status_line_items(app).and_then(|items| {
        status::render_status_detail_line(app, &items, app.status_line_use_colors)
    });
    // Active detail items (configured, or the built-in default list) take the
    // place of `dir … · git …` on row 1; otherwise both rows duplicate the
    // same data. Mode label stays right-aligned. An explicit empty list, or a
    // list whose items all render empty, falls back to the historical overview.
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
            Style::default().fg(crate::render::theme::quiet()),
        ))
    } else {
        Line::from(hints_span)
    };
    vec![top, bottom]
}

/// Right-align the mode label on a status row whose left side is the
/// detail line. Mirrors [`format_status_overview_line`]'s alignment
/// math but preserves the detail line's styled spans.
fn compose_status_overview_with_detail(
    mut detail: Line<'static>,
    app: &TuiApp,
    width: u16,
) -> Line<'static> {
    let right = mode_status_text(app);
    let right_width = right.chars().count();
    let gap_width = usize::from(width).saturating_sub(right_width).min(1);
    let detail_limit = usize::from(width).saturating_sub(right_width + gap_width);
    truncate_line_to_width(&mut detail, detail_limit);
    let detail_width: usize = detail
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    let padding_width = usize::from(width).saturating_sub(detail_width + right_width);
    if padding_width > 0 {
        detail.spans.push(Span::raw(" ".repeat(padding_width)));
    }
    detail.spans.push(Span::styled(
        right,
        Style::default().fg(mode_status_color(app.mode)),
    ));
    detail
}

fn truncate_line_to_width(line: &mut Line<'static>, width: usize) {
    let current_width: usize = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    if current_width <= width {
        return;
    }
    if width == 0 {
        line.spans.clear();
        return;
    }
    let marker = if width <= 3 {
        ".".repeat(width)
    } else {
        "...".to_string()
    };
    let content_width = width.saturating_sub(marker.chars().count());
    let mut used = 0usize;
    let mut spans = Vec::new();
    for span in std::mem::take(&mut line.spans) {
        if used >= content_width {
            break;
        }
        let span_width = span.content.chars().count();
        if used + span_width <= content_width {
            used += span_width;
            spans.push(span);
            continue;
        }
        let take = content_width - used;
        if take > 0 {
            let text = span.content.chars().take(take).collect::<String>();
            spans.push(Span::styled(text, span.style));
        }
        break;
    }
    let marker_style = spans
        .last()
        .map(|span: &Span<'static>| span.style)
        .unwrap_or_default();
    spans.push(Span::styled(marker, marker_style));
    line.spans = spans;
}

fn detail_was_present(app: &TuiApp) -> bool {
    configured_status_line_items(app).is_some()
}

/// User-configured `[tui].status_line`, or the built-in default list when the
/// TOML key is unset. An explicit empty list still disables the detail row.
fn configured_status_line_items(app: &TuiApp) -> Option<Vec<status::StatusLineItem>> {
    match &app.status_line_items {
        Some(list) if list.is_empty() => None,
        Some(list) => Some(list.clone()),
        None => Some(status::DEFAULT_STATUS_LINE_ITEMS.to_vec()),
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
    let base = format_status_hint_base(app);
    // `app.status` carries the per-action acknowledgement string set
    // by `toggle_*`, `dispatch_command`, mode-switch, and friends.
    // Before this branch surfaced anything visible the field was
    // effectively write-only — keystrokes confirmed silently. When
    // the value matches a user-action allowlist (toggle, mode switch,
    // slash result, etc.), prepend it as a transient badge in FRONT
    // of the full hint row so the user gets visible feedback without
    // losing the help text. The ack stays put until the next state
    // change overwrites `app.status` — that's fine: it's always
    // adjacent to, not in place of, the affordance list.
    if let Some(transient) = transient_status_message(app) {
        format!("{transient} · {base}")
    } else {
        base
    }
}

fn format_status_hint_base(app: &TuiApp) -> String {
    if let Some(overlay) = app.transcript_overlay.as_ref() {
        return if overlay.mode.mouse_capture() {
            "PgUp/PgDn/Wheel scroll · M native selection · drag right gutter scroll · Shift-drag select · Esc close"
                .to_string()
        } else {
            "PgUp/PgDn/Wheel scroll · M scrollbar drag · native select/copy · Esc close".to_string()
        };
    }
    if app.subagent_pane.focused {
        return "Up/Down switch · Enter scroll · Del clear done · type/Esc back to prompt"
            .to_string();
    }
    if let Some(pending) = app.pending_request_user_input.as_ref() {
        if pending.request.choices.is_empty() && pending.request.allow_freeform {
            return "type your answer · Enter send · Esc cancel".to_string();
        }
        if pending.request.allow_freeform {
            return "Up/Down choose · type selects Answer · Enter sends dotted row · Esc cancel"
                .to_string();
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
    } else if app.pending_feedback.is_some() {
        return "Enter/Y send feedback · Esc/N discard".to_string();
    } else if app.cancel.is_some() {
        let mut hint = String::from(
            "Ctrl-C/Esc interrupt · Enter queue · Ctrl+J newline · Ctrl-P task · Ctrl-T full transcript · Ctrl-Y copy · /help",
        );
        if !app.subagent_pane.records.is_empty() {
            hint.push_str(" · Down subagents");
        }
        if !app.prompt_queue.is_empty() {
            hint.push_str(&format!(" · Ctrl+X Q reorder ({})", app.prompt_queue.len()));
        }
        return hint;
    } else if app.exit_confirm_armed {
        return "Ctrl+C or Y to exit · any other key to cancel".to_string();
    }
    if app.cancelled_prompt.is_some() && app.turn_rx.is_none() && app.input.is_empty() {
        // We're idle right after a cancelled/failed turn — surface the
        // recovery affordance before the regular hint set.
        return "Ctrl-R restore last prompt · Enter send · Ctrl+J newline · /help".to_string();
    }
    let mut base = if app.alternate_scroll_enabled {
        "Enter send · !cmd shell · Wheel/PgUp/PgDn scroll · Up/Down menu · Alt+Up/Down history · Ctrl+J newline · Ctrl-T full transcript · /help"
            .to_string()
    } else {
        "Enter send · !cmd shell · Up/Down menu/history · Ctrl+J newline · Ctrl-T full transcript · /help"
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
    if !app.prompt_queue.is_empty() {
        base.push_str(&format!(
            " · queued: {} · Ctrl+X Q reorder",
            app.prompt_queue.len()
        ));
    }
    if !app.subagent_pane.records.is_empty() {
        base.push_str(" · Down subagents");
    }
    base
}

/// Allowlisted prefixes for status strings the user explicitly
/// triggered (toggle, mode switch, slash command result, prompt
/// queue op). The agent's mid-turn `app.status` writes ("running
/// grep", "queued shell", "thinking", subagent lifecycle) are NOT
/// listed — those already surface via `turn_progress_segment` /
/// `active_tool`, and double-rendering them would make every
/// tool-call event flicker the hint row.
const USER_ACTION_STATUS_PREFIXES: &[&str] = &[
    "mode switched ",
    "already in ",
    "stay in ",
    "plan prompt dismissed",
    "chord cancelled",
    "exit cancelled",
    "Ctrl+X…",
    "transcript overlay",
    "subagent pane ",
    "main conversation selected",
    "subagent conversation selected",
    "cleared finished subagents",
    "task panel ",
    "/statusline cancelled",
    "no recent session",
    "session quick-switch",
    "resume failed",
    "restored last prompt",
    "select a transcript entry",
];

/// `app.status` value to surface as a transient acknowledgement, or
/// `None` when the current value is a lifecycle placeholder
/// (`"ready"`, `"thinking"`, tool-progress writes) that the hint row
/// shouldn't clobber.
fn transient_status_message(app: &TuiApp) -> Option<String> {
    let status = app.status.trim();
    if status.is_empty() {
        return None;
    }
    USER_ACTION_STATUS_PREFIXES
        .iter()
        .any(|prefix| status.starts_with(prefix))
        .then(|| status.to_string())
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
        SqueezyError::ProviderRequest(message) | SqueezyError::ProviderStream(message) => {
            // The provider layer humanises Anthropic 4xx envelopes and
            // tags non-transient ones with [`NON_RETRYABLE_MARKER`] so
            // the status line and turn-failed banner can suppress the
            // "retry or check provider/network status" suffix on
            // genuinely terminal errors (400 invalid_request, 401, 403,
            // 404). 5xx, 429, and unknown shapes keep the retry hint.
            // See `crates/squeezy-llm/src/anthropic_error.rs`.
            let (non_retryable, stripped) =
                squeezy_llm::anthropic_error::strip_non_retryable_marker(message);
            let prefix = match error {
                SqueezyError::ProviderRequest(_) => "provider request failed",
                _ => "provider stream failed",
            };
            if non_retryable {
                format!("{prefix}: {stripped}")
            } else {
                format!("{prefix}: {stripped}; retry or check provider/network status")
            }
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

pub(crate) trait Clipboard: Send {
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
    /// The background probe (git snapshot + `gh pr view` + `git diff`) has
    /// not landed yet. `RepoStatus::detect` runs off the startup path so the
    /// prompt is interactive immediately; the status bar shows a neutral
    /// placeholder until [`drain_repo_status`] swaps in the real result.
    pending: bool,
}

impl RepoStatus {
    /// Run the repo-status probes against `workspace_root`. Spawns git/`gh`
    /// subprocesses, so callers must keep this off the latency-critical
    /// startup path (see the deferral in `run_inner_with_terminal`).
    fn detect_at(workspace_root: &std::path::Path) -> Self {
        let Ok(vcs) = GitVcs::open(workspace_root) else {
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
            .and_then(|b| probe_pull_request(workspace_root, b));
        let branch_changes = probe_branch_changes(workspace_root);
        Self {
            branch,
            changed_files: snapshot.summary.files_changed,
            operation: snapshot.vcs.operation_state,
            available: true,
            pull_request,
            branch_changes,
            pending: false,
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
            pending: false,
        }
    }

    /// Neutral placeholder shown while the real probe runs in the
    /// background. Distinct from [`none`] (which means "checked, not a git
    /// repo") so the status bar can render a quiet "…" instead of the
    /// misleading "no repo" during the sub-second probe window.
    fn pending() -> Self {
        Self {
            pending: true,
            ..Self::none()
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

/// Render a `LanguageReport` (live workspace file counts from the graph)
/// into the same shape `startup_language_summary` in `squeezy-cli` uses
/// at first paint, so the status-line value stays visually identical
/// across the initial render and subsequent watcher-driven refreshes.
/// Families are merged (jsx/tsx → JS/TS, c/cpp → C/C++) and sorted by
/// display name; zero-count families are omitted.
fn format_language_report(report: &squeezy_tools::LanguageReport) -> String {
    let entries: [(&str, usize); 7] = [
        ("C/C++", report.c_files + report.cpp_files),
        ("C#", report.csharp_files),
        ("Go", report.go_files),
        ("Java", report.java_files),
        (
            "JS/TS",
            report.javascript_files + report.jsx_files + report.typescript_files + report.tsx_files,
        ),
        ("Python", report.python_files),
        ("Rust", report.rust_files),
    ];
    let pieces: Vec<String> = entries
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(name, count)| format!("{name} {count}"))
        .collect();
    if pieces.is_empty() {
        "none".to_string()
    } else {
        pieces.join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionStatus {
    mode: String,
    shell: String,
    sandbox: String,
}

impl PermissionStatus {
    fn from_policy(policy: &PermissionPolicy) -> Self {
        Self {
            mode: policy.mode.as_str().replace('_', "-"),
            shell: policy.shell.as_str().to_string(),
            sandbox: format!(
                "{}/net={}",
                policy.shell_sandbox.mode.as_str(),
                policy.shell_sandbox.network.as_str()
            ),
        }
    }

    fn compact(&self) -> String {
        format!("perm={}", self.mode)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptAttachment {
    placeholder: String,
    payload: PromptAttachmentPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptAttachmentPayload {
    Text {
        replacement: String,
    },
    Image {
        media_type: String,
        bytes: Arc<[u8]>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) enum ConversationSource {
    #[default]
    Main,
    Subagent(SubagentId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentLifecycle {
    Running,
    Completed,
    Failed,
    /// Refused before a lease was acquired (e.g. the concurrency cap was
    /// hit). Has no real subagent id and never produces further events.
    Rejected,
}

impl SubagentLifecycle {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "done",
            Self::Failed => "failed",
            Self::Rejected => "capped",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Running => crate::render::theme::accent(),
            Self::Completed => crate::render::theme::green(),
            Self::Failed => crate::render::theme::red(),
            Self::Rejected => crate::render::theme::quiet(),
        }
    }
}

#[derive(Debug, Clone)]
struct SubagentRecord {
    id: SubagentId,
    agent: String,
    prompt: String,
    lifecycle: SubagentLifecycle,
    latest: String,
    scroll_from_bottom: u16,
    metrics: Option<TurnMetrics>,
    transcript: Vec<TranscriptEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentPaneState {
    focused: bool,
    selected: usize,
    active: ConversationSource,
    records: Vec<SubagentRecord>,
    /// Source of ids for rejected subagents, which never acquire a real
    /// lease id. Drawn from the top of the u64 range downward so they can
    /// never collide with the session's low, increasing lease ids.
    next_synthetic_id: SubagentId,
}

impl Default for SubagentPaneState {
    fn default() -> Self {
        Self {
            focused: false,
            selected: 0,
            active: ConversationSource::Main,
            records: Vec::new(),
            next_synthetic_id: u64::MAX,
        }
    }
}

pub(crate) struct TuiApp {
    pub(crate) provider_name: &'static str,
    pub(crate) version: &'static str,
    pub(crate) model: String,
    /// Mirror of `AppConfig::reasoning_effort`. `None` means "let the
    /// model choose"; the status-line `reasoning-effort` item hides
    /// itself in that case and shows the level (`low`, `medium`, …)
    /// when the user has set one explicitly.
    pub(crate) reasoning_effort: Option<squeezy_core::ReasoningEffort>,
    pub(crate) directory: String,
    pub(crate) language_summary: String,
    pub(crate) mode: SessionMode,
    pub(crate) config_sources: String,
    pub(crate) status_verbosity: StatusVerbosity,
    pub(crate) response_verbosity: ResponseVerbosity,
    pub(crate) tool_output_verbosity: ToolOutputVerbosity,
    pub(crate) transcript_default: TranscriptDefault,
    pub(crate) show_reasoning_usage: bool,
    /// Render-time grouping of adjacent same-tool same-status calls into
    /// one card. Mirrors `config.tui.coalesce_tool_runs` at startup;
    /// flipped at runtime via `/config coalesce_tool_runs = …`. Default
    /// `true`. Independent of the push-time retry coalescer
    /// ([`coalesce_tool_transcript_entry`]).
    pub(crate) coalesce_tool_runs: bool,
    /// Mirrors `config.tui.shell_diff_inline`. The active value also lives
    /// in the process-wide [`SHELL_DIFF_INLINE_OVERRIDE`] so the deep
    /// render path can consult it without a parameter cascade — this
    /// field is the source of truth for runtime mutation, the static is
    /// derived.
    pub(crate) shell_diff_inline: ShellDiffInline,
    pub(crate) repo: RepoStatus,
    pub(crate) permissions: PermissionStatus,
    pub(crate) telemetry: TelemetryStatus,
    pub(crate) input: String,
    pub(crate) input_cursor: usize,
    pub(crate) prompt_attachments: Vec<PromptAttachment>,
    pub(crate) input_history: prompt_history::PromptHistory,
    pub(crate) input_history_index: Option<usize>,
    pub(crate) input_history_draft: String,
    pub(crate) slash_menu_index: usize,
    pub(crate) mention_popup: Option<mention::MentionPopup>,
    pub(crate) workspace_file_cache: Option<mention::WorkspaceFileCache>,
    /// In-flight `@`-mention workspace walk. The walk lists up to
    /// `MAX_WORKSPACE_FILES` paths via the `ignore` crate, which is
    /// tens-to-hundreds of milliseconds of `readdir`/`stat` on a large
    /// repo. It runs in `spawn_blocking` so the composer stays responsive;
    /// the rebuilt cache lands here and is drained each frame. `Some`
    /// doubles as the in-flight guard so a new walk isn't started while
    /// one is pending.
    pub(crate) pending_mention_walk: Option<oneshot::Receiver<mention::WorkspaceFileCache>>,
    pub(crate) overlay: Option<overlay::Overlay>,
    /// Per-app generation counter for [`overlay::DialogHandle::open`]. Bumped
    /// on every open so stale handles for replaced dialogs become no-ops.
    pub(crate) overlay_next_id: u64,
    /// Generation id of the dialog currently occupying `overlay`. `None`
    /// when `overlay` is `None`. Maintained alongside `overlay` so the
    /// handle pattern can distinguish "my dialog is still open" from
    /// "someone else replaced my dialog".
    pub(crate) overlay_active_id: Option<u64>,
    /// Full-screen transcript overlay (Ctrl+T) that renders every entry
    /// in its uncapped form. `None` = closed; `Some(state)` = open with
    /// a scroll offset. Acts as the escape hatch from the aggressive
    /// default truncation.
    pub(crate) transcript_overlay: Option<TranscriptOverlayState>,
    pub(crate) transcript_overlay_scrollbar_cache:
        std::cell::Cell<Option<TranscriptOverlayScrollbarCache>>,
    pub(crate) transcript_overlay_render_cache: std::cell::RefCell<TranscriptOverlayRenderCache>,
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
    pub(crate) checkpoints_enabled: bool,
    pub(crate) transcript: Vec<TranscriptEntry>,
    pub(crate) subagent_pane: SubagentPaneState,
    pub(crate) selected_entry: Option<usize>,
    pub(crate) next_entry_id: u64,
    /// Per-app discriminator for the global entry render cache. Allocated
    /// once at `TuiApp::new`; every cache lookup is `(session, entry_id)`
    /// so two `TuiApp` instances that share the process (most notably
    /// parallel `cargo test` runs that share the static cache) cannot
    /// clobber each other's entries through the colliding `entry_id = 0`
    /// they both restart from.
    pub(crate) render_cache_session: u64,
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
    /// Tracks terminal-focus state for the host tty. The main loop
    /// freezes the animation tick driver and short-circuits
    /// [`Self::has_active_animation`] while this is `false`, so an
    /// idle background window stops repainting its spinner and the
    /// terminal-title clock. Driven by crossterm's `FocusGained` /
    /// `FocusLost` events when `EnableFocusChange` is in effect.
    /// Terminals that do not emit focus events keep this stuck at
    /// `true`, which preserves the pre-existing animation behaviour.
    pub(crate) focused: bool,
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
    /// Receives formatted log lines from the deferred plan-housekeeping
    /// task (`migrate_legacy_plans` + `git_referenced_plan_ids` +
    /// `prune_plan_dir`). Moved off the boot path so a 30-day `git log`
    /// shell-out and any plan-dir fs walks don't gate the first frame.
    pub(crate) plan_housekeeping_rx: Option<oneshot::Receiver<Vec<String>>>,
    /// Receives the result of the deferred `RepoStatus::detect` probe (git
    /// worktree snapshot + `gh pr view` + `git diff --shortstat`). Those
    /// subprocesses dominate time-to-interactive, so they run off the boot
    /// path and the status bar shows a neutral placeholder until this lands.
    pub(crate) repo_status_rx: Option<oneshot::Receiver<RepoStatus>>,
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
    /// `/diff` and, when checkpointing is enabled, `/undo` hints at
    /// end-of-turn (success or failure).
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
    /// In-flight `/diff` snapshot work. `vcs.snapshot()` shells out to
    /// blocking git subprocesses and iterates every changed file, which
    /// can take seconds on a busy worktree. We offload it to
    /// `spawn_blocking` and poll this receiver each frame so the input
    /// loop stays responsive. Drives the working spinner while set.
    pub(crate) pending_diff: Option<oneshot::Receiver<PendingDiffResult>>,
    /// Wall-clock anchor for the in-flight `/diff` snapshot. Drives the
    /// elapsed-time counter on the Working row so the user can see the
    /// command is making progress even though the foreground UI is idle.
    pub(crate) pending_diff_started_at: Option<Instant>,
    /// FIFO of prompts the user typed while a turn was running. Drained
    /// one-at-a-time as each turn completes; preserved on cancel.
    pub(crate) prompt_queue: VecDeque<String>,
    /// Open reorder overlay state. `None` when the overlay is closed;
    /// the queue itself lives on `prompt_queue` regardless.
    pub(crate) prompt_queue_overlay: Option<prompt_queue::PromptQueueState>,
    /// Set true when a turn just completed (success, cancel, or fail)
    /// and the queue is non-empty. The main loop reads this immediately
    /// after `drain_agent_events` returns, pops the next prompt, and
    /// calls `start_user_turn`. Cleared each time the main loop reads
    /// it. To stop the auto-drain, open the queue overlay and delete
    /// the entries you don't want.
    pub(crate) auto_drain_queue: bool,
    /// Pending multi-key chord (`Ctrl+X` leader). Set when the user
    /// types the leader; cleared either by the matching follow-up key
    /// or by any other keystroke (which then falls through normally).
    pub(crate) pending_chord: Option<ChordPrefix>,
    /// Per-frame click-target registry. Render fns push `Clickable`
    /// entries here (through `&TuiApp` thanks to interior mutability);
    /// `handle_mouse` iterates the Vec in reverse on left-click so
    /// the topmost (later-rendered) hit wins. Cleared at the start of
    /// every draw via `begin_frame_clickables`.
    pub(crate) clickables: std::cell::RefCell<Vec<Clickable>>,
    /// User-authored slash macros loaded from `~/.squeezy/prompts/` and
    /// `<workspace>/.squeezy/prompts/`. Consulted by
    /// [`handle_slash_command`] when the typed head isn't a built-in
    /// `DispatchCommand`; a match expands the template body and routes
    /// it through [`start_user_turn`] like any other typed prompt.
    pub(crate) prompt_templates: PromptTemplateCatalog,
    /// One-shot slash-command escape hatch for commands that intentionally
    /// leave editable text in the composer after they run.
    pub(crate) preserve_input_after_slash_command: bool,
    /// Override for the user-scope settings file that slash commands
    /// (`/theme`, `/statusline`, …) persist into. `None` ⇒ production
    /// path: `squeezy_core::default_settings_path()` (which itself
    /// honours `$SQUEEZY_SETTINGS_PATH` then `$HOME/.squeezy/settings.toml`).
    /// `Some(path)` ⇒ writes are pinned to `path`, used by the eval
    /// harness so scenario runs cannot clobber the operator's real
    /// `~/.squeezy/settings.toml`.
    pub(crate) settings_path_override: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChordPrefix {
    /// `Ctrl+X` — Emacs-style extended-command leader. Currently only
    /// used by `Ctrl+X Q` (toggle the prompt-queue overlay).
    CtrlX,
}

/// What a click on a registered `Clickable` should do. Add a variant
/// when a new button lands; add the matching arm in
/// `dispatch_click_action` next to the existing handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickAction {
    /// Open / close the prompt-queue reorder overlay.
    ToggleQueueOverlay,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Clickable {
    pub(crate) rect: Rect,
    pub(crate) action: ClickAction,
}

#[derive(Debug, Default)]
pub(crate) struct PendingDiffResult {
    pub(crate) logs: Vec<String>,
    pub(crate) card: Option<DiffCardData>,
}

/// Build the runtime [`keymap::KeymapResolver`] by layering the optional
/// user-editable `~/.squeezy/keybindings.toml` on top of the
/// `[tui.keymap]` overrides from `settings.toml`. Failures (missing
/// `$HOME`, unreadable file, malformed TOML, reserved-key violation)
/// emit a warning and fall back to the base overrides so a broken
/// keybindings file never prevents the TUI from starting.
fn build_keymap_resolver(base: &BTreeMap<String, String>) -> keymap::KeymapResolver {
    let user_path = keymap_config::default_keybindings_path();
    match keymap_config::merge_user_overrides(base.clone(), user_path.as_deref()) {
        Ok(merged) => keymap::KeymapResolver::from_overrides(&merged),
        Err(err) => {
            tracing::warn!(
                target: "squeezy_tui::keymap_config",
                error = %err,
                "ignoring ~/.squeezy/keybindings.toml; falling back to defaults"
            );
            keymap::KeymapResolver::from_overrides(base)
        }
    }
}

fn format_config_warning(warning: &ConfigWarning) -> String {
    format!(
        "ignored unknown setting {} in {}",
        warning.field, warning.source
    )
}

impl TuiApp {
    pub(crate) fn prune_prompt_attachments(&mut self) {
        self.prompt_attachments
            .retain(|attachment| self.input.contains(&attachment.placeholder));
    }

    pub(crate) fn clear_prompt_attachments(&mut self) {
        self.prompt_attachments.clear();
    }

    /// Clear the click-target registry at the start of each frame.
    /// Called from `render` / `render_inline` before any widget draws.
    pub(crate) fn begin_frame_clickables(&self) {
        self.clickables.borrow_mut().clear();
    }

    /// Record a click target for the frame currently being drawn.
    /// Render fns hold only `&TuiApp`, so the registry lives in a
    /// `RefCell` to allow `push` through a shared reference.
    pub(crate) fn register_click(&self, rect: Rect, action: ClickAction) {
        self.clickables
            .borrow_mut()
            .push(Clickable { rect, action });
    }

    /// Topmost click target containing `(column, row)`, if any.
    /// Iterates the registry in reverse so later-rendered widgets
    /// (overlays, modals) take precedence over earlier ones at the
    /// same screen cell.
    pub(crate) fn click_target_at(&self, column: u16, row: u16) -> Option<ClickAction> {
        self.clickables
            .borrow()
            .iter()
            .rev()
            .find(|c| {
                column >= c.rect.x
                    && column < c.rect.x.saturating_add(c.rect.width)
                    && row >= c.rect.y
                    && row < c.rect.y.saturating_add(c.rect.height)
            })
            .map(|c| c.action)
    }

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

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn new_with_clipboard(
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
                setup_question_count: None,
                open_config_section: None,
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
        let keymap = build_keymap_resolver(&config.tui.keymap);
        let open_config_section = startup.open_config_section;
        let language_summary = if startup.languages.trim().is_empty() {
            configured_language_summary(config)
        } else {
            startup.languages
        };
        let mut app = Self {
            provider_name,
            version: env!("CARGO_PKG_VERSION"),
            model: config.model.clone(),
            reasoning_effort: config.reasoning_effort,
            directory: compact_path(&config.workspace_root),
            language_summary,
            mode,
            config_sources: config.config_source_labels().join(","),
            status_verbosity: config.tui.status_verbosity,
            response_verbosity: config.tui.response_verbosity,
            tool_output_verbosity: config.tui.tool_output_verbosity,
            transcript_default: config.tui.transcript_default,
            show_reasoning_usage: config.tui.show_reasoning_usage,
            coalesce_tool_runs: config.tui.coalesce_tool_runs,
            shell_diff_inline: {
                set_shell_diff_inline(config.tui.shell_diff_inline);
                config.tui.shell_diff_inline
            },
            // The repo status (branch, changed files, PR number, branch
            // diff) is built from git + `gh` subprocesses that dominate
            // time-to-interactive; start neutral and let the background
            // probe spawned in `run_inner_with_terminal` fill it in.
            repo: RepoStatus::pending(),
            permissions: PermissionStatus::from_policy(&config.permissions),
            telemetry: TelemetryStatus::from_config(&config.telemetry),
            input: String::new(),
            input_cursor: 0,
            prompt_attachments: Vec::new(),
            input_history: if config.tui.persist_prompt_history {
                prompt_history::PromptHistory::with_persistence(
                    prompt_history::DEFAULT_PROMPT_HISTORY_CAPACITY,
                    squeezy_core::default_prompt_history_path(),
                )
            } else {
                prompt_history::PromptHistory::in_memory(
                    prompt_history::DEFAULT_PROMPT_HISTORY_CAPACITY,
                )
            },
            input_history_index: None,
            input_history_draft: String::new(),
            slash_menu_index: 0,
            mention_popup: None,
            workspace_file_cache: None,
            pending_mention_walk: None,
            overlay: None,
            overlay_next_id: 0,
            overlay_active_id: None,
            transcript_overlay: None,
            transcript_overlay_scrollbar_cache: std::cell::Cell::new(None),
            transcript_overlay_render_cache: std::cell::RefCell::new(
                TranscriptOverlayRenderCache::default(),
            ),
            alternate_scroll_enabled: TerminalMode::from(config.tui.alternate_screen)
                == TerminalMode::AlternateScreen,
            attachments: Vec::new(),
            context_compaction: ContextCompactionState::default(),
            context_compaction_threshold: config.context_compaction.estimated_tokens,
            context_compaction_nudge_shown: false,
            context_estimate: ContextEstimate::default(),
            checkpoints_enabled: config.checkpoints_enabled,
            transcript,
            subagent_pane: SubagentPaneState::default(),
            selected_entry: None,
            next_entry_id,
            render_cache_session: render::cache::next_session_id(),
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
            // Assume focused at startup. Crossterm only emits `FocusLost`
            // after `EnableFocusChange` lands, and terminals that do not
            // support the protocol never flip the flag — so the worst
            // case for a non-emitter is the pre-existing behaviour.
            focused: true,
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
            plan_housekeeping_rx: None,
            repo_status_rx: None,
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
            keymap,
            pending_diff: None,
            pending_diff_started_at: None,
            prompt_queue: VecDeque::new(),
            prompt_queue_overlay: None,
            auto_drain_queue: false,
            pending_chord: None,
            clickables: std::cell::RefCell::new(Vec::new()),
            prompt_templates: PromptTemplateCatalog::discover(&config.workspace_root),
            preserve_input_after_slash_command: false,
            settings_path_override: None,
        };
        if let Some(section) = open_config_section {
            app.config_screen = Some(config_screen::ConfigScreenState::new(
                config.clone(),
                Some(section),
            ));
        }
        for warning in &config.config_warnings {
            app.push_warn(format_config_warning(warning));
        }
        app
    }

    /// Mirror every config-derived field the status line and header
    /// read so external `settings.toml` edits surface live in the
    /// running TUI without a restart. Runtime-only fields (transcript,
    /// turn state, repo polling) are deliberately left alone — they
    /// are owned by the session, not the config tier.
    pub(crate) fn apply_config_change(&mut self, config: &AppConfig) {
        self.model = config.model.clone();
        self.reasoning_effort = config.reasoning_effort;
        self.directory = compact_path(&config.workspace_root);
        self.workspace_root = config.workspace_root.clone();
        self.config_sources = config.config_source_labels().join(",");
        self.status_verbosity = config.tui.status_verbosity;
        self.response_verbosity = config.tui.response_verbosity;
        self.tool_output_verbosity = config.tui.tool_output_verbosity;
        self.transcript_default = config.tui.transcript_default;
        self.show_reasoning_usage = config.tui.show_reasoning_usage;
        self.coalesce_tool_runs = config.tui.coalesce_tool_runs;
        self.permissions = PermissionStatus::from_policy(&config.permissions);
        self.telemetry = TelemetryStatus::from_config(&config.telemetry);
        self.context_compaction_threshold = config.context_compaction.estimated_tokens;
        self.checkpoints_enabled = config.checkpoints_enabled;
        self.cost_cap_usd_micros = config.max_session_cost_usd_micros.filter(|cap| *cap > 0);
        self.status_line_items = parse_status_line_items(config.tui.status_line.as_deref());
        self.status_line_use_colors = config.tui.status_line_use_colors;
        self.animation_tick_rate = config.tick_rate;
        // Languages only refresh from config when no filesystem
        // detection populated them (the fallback path). Filesystem
        // counts are owned by the startup walk and shouldn't be
        // clobbered by a settings reload.
        let summary = self.language_summary.trim();
        if summary.is_empty() || summary == "none" {
            self.language_summary = configured_language_summary(config);
        }
    }

    /// Resolve the user-scope settings path slash commands should write
    /// to. Returns the [`settings_path_override`](Self::settings_path_override)
    /// when one has been pinned (eval harness path), else the production
    /// [`squeezy_core::default_settings_path`] (which itself honours
    /// `$SQUEEZY_SETTINGS_PATH` then `$HOME/.squeezy/settings.toml`).
    pub(crate) fn user_settings_path(&self) -> PathBuf {
        self.settings_path_override
            .clone()
            .unwrap_or_else(squeezy_core::default_settings_path)
    }

    /// Pin the user-scope settings path. Used by the eval harness so
    /// `/theme` etc. cannot escape the per-run scratch directory. Has
    /// no effect on production sessions, which never call this.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn set_settings_path_override(&mut self, path: Option<PathBuf>) {
        self.settings_path_override = path;
    }

    /// Open `content` as a typed slash-command overlay and return a
    /// [`overlay::DialogHandle`] for later manipulation. Thin convenience
    /// wrapper around [`overlay::DialogHandle::open`] that threads
    /// `self.overlay`, `self.overlay_next_id`, and `self.overlay_active_id`
    /// in a single call so slash-command handlers don't need to touch
    /// the underlying state directly.
    #[allow(dead_code)]
    pub(crate) fn open_overlay(
        &mut self,
        content: overlay::Overlay,
        prior_focus: overlay::PriorFocus,
    ) -> overlay::DialogHandle {
        overlay::DialogHandle::open(
            &mut self.overlay,
            &mut self.overlay_next_id,
            &mut self.overlay_active_id,
            content,
            prior_focus,
        )
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
    ///
    /// Returns `false` for unfocused idle animation, but keeps active turns
    /// moving. That preserves the first-prompt spinner during transient focus
    /// changes without repainting purely decorative idle motion in the
    /// background.
    pub(crate) fn has_active_animation(&self) -> bool {
        if !self.focused && !turn_in_progress(self) {
            return false;
        }
        matches!(self.turn_visual, TurnVisualState::Running)
            || self.terminal_title_state == TerminalTitleState::Working
            || self.pending_diff.is_some()
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

    pub(crate) fn push_info(&mut self, message: String) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::log_with_kind(
            id,
            message,
            LogKind::Info,
            self.transcript_default,
        ));
    }

    /// Push a warning log (⚠ prefix). Suppresses the push when
    /// the most recent transcript entry is already a `⚠ Cancelled` / `⚠ Denied`
    /// tool card — the card already communicates the turn-end at a glance,
    /// and a trailing `⚠ turn cancelled` line is just noise. Returns whether
    /// the entry was actually pushed.
    pub(crate) fn push_warn(&mut self, message: String) -> bool {
        if let Some(last) = self.transcript.last()
            && last.is_cancel_terminated_tool_card()
        {
            return false;
        }
        let id = self.next_id();
        self.push_entry(TranscriptEntry::log_with_kind(
            id,
            message,
            LogKind::Warn,
            self.transcript_default,
        ));
        true
    }

    /// Append a transcript entry tagged as operational chrome — used for
    /// turn-complete markers, compaction notices, and plan-handoff state
    /// that should fade to the periphery rather than read as content.
    pub(crate) fn push_status(&mut self, message: String) {
        let id = self.next_id();
        self.push_entry(TranscriptEntry::log_with_kind(
            id,
            message,
            LogKind::Operational,
            self.transcript_default,
        ));
    }

    pub(crate) fn note_subagent_started(&mut self, id: SubagentId, agent: String, prompt: String) {
        let prompt_entry_id = self.next_id();
        let start_entry_id = self.next_id();
        let transcript = vec![
            TranscriptEntry::message(
                prompt_entry_id,
                TranscriptItem::user(prompt.clone()),
                self.transcript_default,
            ),
            TranscriptEntry::log_with_kind(
                start_entry_id,
                format!("{agent} subagent started"),
                LogKind::Operational,
                self.transcript_default,
            ),
        ];
        if let Some(record) = self.subagent_pane.records.iter_mut().find(|r| r.id == id) {
            record.agent = agent;
            record.prompt = prompt;
            record.lifecycle = SubagentLifecycle::Running;
            record.latest = "starting".to_string();
            record.scroll_from_bottom = 0;
            record.metrics = None;
            record.transcript = transcript;
        } else {
            self.subagent_pane.records.push(SubagentRecord {
                id,
                agent,
                prompt,
                lifecycle: SubagentLifecycle::Running,
                latest: "starting".to_string(),
                scroll_from_bottom: 0,
                metrics: None,
                transcript,
            });
        }
        self.prune_subagent_records();
        self.clamp_subagent_selection();
    }

    pub(crate) fn note_subagent_activity(
        &mut self,
        id: SubagentId,
        agent: String,
        message: String,
    ) {
        let entry_id = self.next_id();
        let entry = TranscriptEntry::log_with_kind(
            entry_id,
            message.clone(),
            LogKind::Operational,
            self.transcript_default,
        );
        if let Some(record) = self.subagent_pane.records.iter_mut().find(|r| r.id == id) {
            record.agent = agent;
            record.latest = compact_text(&message, 120);
            record.transcript.push(entry);
            // Keep the seed prompt + "started" log (the first two entries);
            // drop the oldest activity beyond the cap so a long-running
            // subagent's stored transcript stays bounded.
            const MAX_SUBAGENT_TRANSCRIPT: usize = 256;
            if record.transcript.len() > MAX_SUBAGENT_TRANSCRIPT {
                let overflow = record.transcript.len() - MAX_SUBAGENT_TRANSCRIPT;
                record.transcript.drain(2..2 + overflow);
            }
        }
    }

    pub(crate) fn note_subagent_completed(
        &mut self,
        id: SubagentId,
        agent: String,
        summary: String,
        metrics: TurnMetrics,
    ) {
        let entry_id = self.next_id();
        let entry = TranscriptEntry::message(
            entry_id,
            TranscriptItem::assistant(summary.clone()),
            self.transcript_default,
        );
        if let Some(record) = self.subagent_pane.records.iter_mut().find(|r| r.id == id) {
            record.agent = agent;
            record.lifecycle = SubagentLifecycle::Completed;
            record.latest = compact_text(&summary, 140);
            record.metrics = Some(metrics);
            record.transcript.push(entry);
        }
    }

    pub(crate) fn note_subagent_failed(
        &mut self,
        id: SubagentId,
        agent: String,
        error: String,
        metrics: TurnMetrics,
    ) {
        let entry_id = self.next_id();
        let entry = TranscriptEntry::log_with_kind(
            entry_id,
            format!("subagent failed: {error}"),
            LogKind::Warn,
            self.transcript_default,
        );
        if let Some(record) = self.subagent_pane.records.iter_mut().find(|r| r.id == id) {
            record.agent = agent;
            record.lifecycle = SubagentLifecycle::Failed;
            record.latest = compact_text(&error, 140);
            record.metrics = Some(metrics);
            record.transcript.push(entry);
        }
    }

    /// Record a subagent that was refused before it ever ran (e.g. the
    /// concurrency cap was hit). Rejections carry no lease id, so they get a
    /// synthetic one and a single-line transcript explaining the cap; the
    /// row is otherwise a normal (finished) pane entry that Del can clear.
    pub(crate) fn note_subagent_rejected(
        &mut self,
        agent: String,
        reason: String,
        limit: usize,
        active: usize,
    ) {
        let id = self.subagent_pane.next_synthetic_id;
        self.subagent_pane.next_synthetic_id =
            self.subagent_pane.next_synthetic_id.saturating_sub(1);
        let detail = format!("{reason} ({active}/{limit} already running)");
        let entry_id = self.next_id();
        let transcript = vec![TranscriptEntry::log_with_kind(
            entry_id,
            format!("{agent} subagent capped: {detail}"),
            LogKind::Warn,
            self.transcript_default,
        )];
        self.subagent_pane.records.push(SubagentRecord {
            id,
            agent,
            prompt: detail.clone(),
            lifecycle: SubagentLifecycle::Rejected,
            latest: compact_text(&detail, 120),
            scroll_from_bottom: 0,
            metrics: None,
            transcript,
        });
        self.prune_subagent_records();
        self.clamp_subagent_selection();
    }

    fn clamp_subagent_selection(&mut self) {
        let max = self.subagent_pane.records.len();
        self.subagent_pane.selected = self.subagent_pane.selected.min(max);
        if let ConversationSource::Subagent(id) = self.subagent_pane.active
            && !self
                .subagent_pane
                .records
                .iter()
                .any(|record| record.id == id)
        {
            self.subagent_pane.active = ConversationSource::Main;
        }
    }

    /// Bound the retained subagent records so a long session can't grow the
    /// pane (and its cloned transcripts) without limit. Oldest *finished*
    /// records are dropped first; the actively-viewed record and any still-
    /// running subagents are always kept.
    fn prune_subagent_records(&mut self) {
        const MAX_SUBAGENT_RECORDS: usize = 32;
        while self.subagent_pane.records.len() > MAX_SUBAGENT_RECORDS {
            let active_id = match self.subagent_pane.active {
                ConversationSource::Subagent(id) => Some(id),
                ConversationSource::Main => None,
            };
            let Some(pos) = self.subagent_pane.records.iter().position(|record| {
                !matches!(record.lifecycle, SubagentLifecycle::Running)
                    && Some(record.id) != active_id
            }) else {
                break;
            };
            self.subagent_pane.records.remove(pos);
        }
    }

    /// Drop every completed/failed subagent row, leaving only those still
    /// running. When nothing remains the pane disappears and focus returns
    /// to the main conversation. Bound to Del/Backspace while the pane is
    /// focused.
    pub(crate) fn clear_finished_subagents(&mut self) {
        self.subagent_pane
            .records
            .retain(|record| matches!(record.lifecycle, SubagentLifecycle::Running));
        if self.subagent_pane.records.is_empty() {
            self.subagent_pane.focused = false;
            self.subagent_pane.active = ConversationSource::Main;
            self.subagent_pane.selected = 0;
        } else {
            self.subagent_pane.selected = self
                .subagent_pane
                .selected
                .min(self.subagent_pane.records.len());
        }
        self.clamp_subagent_selection();
        self.status = "cleared finished subagents".to_string();
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

    /// Echo the slash command the user just ran into the transcript as a
    /// styled banner. Used by commands that produce in-transcript output
    /// (cards, logs) so the history retains the invocation that triggered
    /// them. Skipped for commands that open their own UI overlay — the
    /// overlay itself is the affordance.
    pub(crate) fn push_slash_command_echo(&mut self, raw: &str) {
        let trimmed = raw.trim_start();
        if trimmed.is_empty() {
            return;
        }
        let (cmd, args) = match trimmed.split_once(char::is_whitespace) {
            Some((cmd, args)) => (cmd.to_string(), args.trim().to_string()),
            None => (trimmed.to_string(), String::new()),
        };
        let id = self.next_id();
        self.push_entry(TranscriptEntry::slash_echo(id, SlashEchoData { cmd, args }));
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
    /// Per-entry monotonic content revision. Starts at 0 on creation
    /// and bumps via [`Self::bump_revision`] every time the entry's
    /// payload or collapsed state mutates. The transcript render-line
    /// cache (`render::cache::get_or_compute_entry`) uses this value as
    /// the entry's invalidation tag, so streaming chunks, tool-call
    /// coalescing, and collapse toggles all transparently invalidate
    /// the entry's cached lines without forcing a full clear.
    ///
    /// All mutations of `TranscriptEntry` past initial construction
    /// MUST call `bump_revision`; the cache's correctness contract
    /// depends on it (F09 finding).
    revision: u64,
}

impl TranscriptEntry {
    /// Bump the per-entry revision. Wrapping is fine because the cache
    /// only ever compares for equality against a previously stored
    /// snapshot — the live counter overlapping with a long-ago value
    /// after 2^64 mutations is not a realistic concern in a TUI
    /// session and would, in any case, produce only a stale-cache
    /// false-hit, not a memory-safety issue.
    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn message(id: u64, item: TranscriptItem, transcript_default: TranscriptDefault) -> Self {
        let collapsed = transcript_default == TranscriptDefault::Compact
            && item.role != Role::Assistant
            && system_message_can_collapse(&item)
            && item.content.chars().count() > LONG_ASSISTANT_CHARS;
        Self {
            id,
            kind: TranscriptEntryKind::Message(item),
            collapsed,
            revision: 0,
        }
    }

    fn tool_result(
        id: u64,
        result: ToolResult,
        call: Option<ToolCall>,
        _transcript_default: TranscriptDefault,
    ) -> Self {
        // Tool results are uniformly collapsed-by-default for the happy
        // path: the head-tail preview caps each card at ~5 lines (50 for
        // direct `!`-shell).
        //
        // Failed tool calls are the exception. The preview hides the
        // actual error message under "Ctrl-O to expand", which is
        // exactly the failure mode the user complained about — a row
        // of red ✖ "Failed X" with no visible reason. Auto-expand on
        // failure so the diagnostic is inline, without forcing a
        // keypress per error to read it.
        let collapsed_default = !matches!(
            result.status,
            ToolStatus::Error | ToolStatus::Denied | ToolStatus::Cancelled
        );
        Self {
            id,
            kind: TranscriptEntryKind::ToolResult(Box::new(ToolTranscript {
                call,
                result,
                repeat_count: 1,
            })),
            collapsed: collapsed_default,
            revision: 0,
        }
    }

    fn log(id: u64, message: String, transcript_default: TranscriptDefault) -> Self {
        Self::log_with_kind(id, message, LogKind::Normal, transcript_default)
    }

    fn log_with_kind(
        id: u64,
        message: String,
        kind: LogKind,
        transcript_default: TranscriptDefault,
    ) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::Log(LogEntry { message, kind }),
            // Operational chrome never benefits from being collapsed —
            // it's already a single dim line.
            collapsed: kind != LogKind::Operational
                && transcript_default == TranscriptDefault::Compact,
            revision: 0,
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
            revision: 0,
        }
    }

    fn diff_card(id: u64, data: DiffCardData) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::Diff(Box::new(data)),
            // `/diff` is never truncated by default. The user can still
            // Ctrl-E to fold the body if it's huge.
            collapsed: false,
            revision: 0,
        }
    }

    fn slash_echo(id: u64, data: SlashEchoData) -> Self {
        Self {
            id,
            kind: TranscriptEntryKind::SlashEcho(data),
            collapsed: false,
            revision: 0,
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
            // Reasoning is visible by default when `show_reasoning_usage`
            // is enabled, but compact transcript mode keeps the body under
            // a concise chip. Ctrl-T opens the full transcript with details.
            collapsed: transcript_default == TranscriptDefault::Compact,
            revision: 0,
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

    /// True when the entry is a tool-result card whose status already
    /// communicates a turn-ending cancellation (rendered with `⚠`).
    /// Used to suppress the redundant `⚠ turn cancelled` log that the
    /// `AgentEvent::Cancelled` arm would otherwise push.
    fn is_cancel_terminated_tool_card(&self) -> bool {
        let TranscriptEntryKind::ToolResult(tool) = &self.kind else {
            return false;
        };
        matches!(
            tool.result.status,
            ToolStatus::Cancelled | ToolStatus::Denied,
        )
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
            TranscriptEntryKind::Log(entry) => (
                "log entry".to_string(),
                entry.message.clone(),
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
            TranscriptEntryKind::SlashEcho(data) => {
                let body = if data.args.is_empty() {
                    data.cmd.clone()
                } else {
                    format!("{} {}", data.cmd, data.args)
                };
                (
                    "slash command".to_string(),
                    body,
                    format!("transcript:{}", self.id),
                )
            }
        }
    }
}

fn system_message_can_collapse(item: &TranscriptItem) -> bool {
    item.role != Role::System
        || item
            .content
            .lines()
            .next()
            .is_none_or(|header| header != "Context window")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogKind {
    /// Standard log line — rendered with `└` and a status-color marker.
    Normal,
    /// Non-error operational event with useful payload. Rendered neutral and
    /// left untruncated so lifecycle details stay inspectable.
    Info,
    /// Operational chrome (turn-complete markers, compaction notices,
    /// plan-handoff state) — rendered dim/italic with no bullet so it
    /// fades to the periphery instead of looking like a content event.
    Operational,
    /// Warning chrome — turn cancellations, turn failures. Rendered with
    /// a `⚠ ` prefix so the user can spot turn-ending events at a glance.
    Warn,
}

#[derive(Debug, Clone)]
pub(crate) struct LogEntry {
    pub(crate) message: String,
    pub(crate) kind: LogKind,
}

impl LogEntry {
    fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug, Clone)]
enum TranscriptEntryKind {
    Message(TranscriptItem),
    ToolResult(Box<ToolTranscript>),
    Log(LogEntry),
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
    /// Echo of the slash command the user typed (e.g. `/diff`, `/plan rewrite foo`).
    /// Renders as a one-line styled banner so the transcript history shows
    /// the invocation that produced the next card / log entry. Inserted only
    /// for commands that don't open their own UI overlay.
    SlashEcho(SlashEchoData),
}

#[derive(Debug, Clone)]
pub(crate) struct SlashEchoData {
    pub(crate) cmd: String,
    pub(crate) args: String,
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
        // Coalesce mutates the visible payload (repeat_count badge + the
        // most recent result body), so the cached line list for this
        // entry is now stale. The revision bump invalidates it on the
        // next render cycle.
        existing.bump_revision();
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
    /// Free-form answer typed inside the modal. Kept separate from
    /// `TuiApp::input` so the user's pending next-prompt draft is not
    /// hijacked by the modal — the previous design routed every char
    /// into the composer, which then looked like a leaked next prompt.
    pub(crate) answer: String,
    /// Byte cursor into `answer`.
    pub(crate) answer_cursor: usize,
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
    /// `Option` so we can drop and rebuild the ratatui `Terminal`
    /// Primary terminal in the user-configured mode. For inline
    /// mode this is a `Viewport::Inline(N)` terminal pinned to the
    /// bottom N rows of the main buffer; for alt-screen mode it's
    /// a fullscreen terminal. Always `Some` after `enter`.
    terminal: Option<Terminal<CrosstermBackend<TerminalWriter>>>,
    /// Secondary fullscreen terminal used only when the configured
    /// mode is inline and the transcript overlay is open. Built
    /// lazily on the first overlay open so a session that never
    /// opens the overlay never pays for it. Once constructed it
    /// stays alive for the rest of the session — re-using it
    /// avoids ratatui's `Terminal::with_options(Inline(N))`
    /// `append_lines` scroll-up that fires on every fresh inline
    /// Terminal construction (the bug that ghosted the pre-overlay
    /// viewport above the new one on overlay close).
    overlay_terminal: Option<Terminal<CrosstermBackend<TerminalWriter>>>,
    /// User-configured mode (`Inline` or `AlternateScreen`). Stays
    /// constant for the session — overlay-screen swaps only flip
    /// `overlay_screen_active`, not `mode`.
    mode: TerminalMode,
    /// True while the inline guard is currently routing draws to
    /// the alt-screen overlay terminal. Always `false` when
    /// `mode == AlternateScreen` (that mode is already fullscreen —
    /// no swap needed).
    overlay_screen_active: bool,
    /// True after we have applied the transcript overlay's mouse policy
    /// to the terminal. Needed because the overlay temporarily overrides
    /// the main screen's opt-in mouse capture even in alternate-screen mode.
    overlay_mouse_override_active: bool,
    /// Whether the overlay currently has drag capture enabled for its
    /// right-side scrollbar. When false, native terminal selection/copy is
    /// left available inside the overlay.
    overlay_mouse_capture: bool,
    /// Whether the user opted into mouse capture for the main screen.
    /// The transcript overlay disables mouse reporting so normal text
    /// selection/copy works there, then restores this setting when it closes.
    mouse_capture: bool,
    exit_hint: Option<String>,
    startup_flushed: bool,
    transcript_flushed_len: usize,
    /// Resolved DEC 2026 synchronized-output flag. Computed once at
    /// startup from the user's [`TuiSynchronizedOutput`] policy plus
    /// terminal-capability detection; consulted around every frame
    /// draw to wrap output in Begin/End Synchronized Update sequences.
    synchronized_output: bool,
}

impl TerminalGuard {
    /// The terminal that should receive the next draw. Routes to
    /// `overlay_terminal` while the alt-screen overlay is up, back
    /// to the primary `terminal` otherwise.
    fn term(&mut self) -> &mut Terminal<CrosstermBackend<TerminalWriter>> {
        if self.overlay_screen_active {
            self.overlay_terminal
                .as_mut()
                .expect("overlay terminal must be built when overlay-screen is active")
        } else {
            self.terminal
                .as_mut()
                .expect("primary terminal lost — unreachable after `enter`")
        }
    }
}

impl TerminalGuard {
    fn enter(
        alternate_screen: TuiAlternateScreen,
        synchronized_output: TuiSynchronizedOutput,
    ) -> Result<Self> {
        let mode = TerminalMode::from(alternate_screen);
        let synchronized_output = resolve_synchronized_output(synchronized_output);
        enable_raw_mode().map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        // Wrap stdout in the env-gated debug-tap writer so every
        // subsequent ANSI sequence — startup setup, draw bytes, and
        // teardown — is mirrored to the log when
        // `SQUEEZY_TUI_WRITE_LOG` is set. When unset the wrapper is a
        // thin pass-through.
        let mut writer = TerminalWriter::from_env(io::stdout());
        let _ = execute!(
            writer,
            DisableModifyOtherKeys,
            PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
        );
        // Mouse capture is opt-in: it hijacks native text selection
        // and terminal scrollback (Shift+drag / Shift+wheel become the
        // escape hatch, which is friction users shouldn't pay by
        // default). When off, the click registry sleeps and `>`/`v`
        // disclosure buttons are still reachable via the keyboard
        // chord (`Ctrl+X Q` for the queue overlay).
        // Opt in with `SQUEEZY_MOUSE_CAPTURE=1`.
        let mouse_capture = std::env::var_os("SQUEEZY_MOUSE_CAPTURE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        match mode {
            TerminalMode::Inline => {
                execute!(
                    writer,
                    Print(CLEAR_SCROLLBACK_AND_VISIBLE),
                    Print(DISABLE_MOUSE_MODES),
                    DisableAlternateScroll,
                    EnableBracketedPaste,
                    EnableFocusChange
                )
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
                if mouse_capture {
                    execute!(writer, Print(ENABLE_MOUSE_CLICK_CAPTURE))
                        .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
                }
            }
            TerminalMode::AlternateScreen => {
                execute!(
                    writer,
                    EnterAlternateScreen,
                    Print(DISABLE_MOUSE_MODES),
                    EnableAlternateScroll,
                    Clear(ClearType::All),
                    MoveTo(0, 0),
                    EnableBracketedPaste,
                    EnableFocusChange
                )
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
                if mouse_capture {
                    execute!(writer, Print(ENABLE_MOUSE_CLICK_CAPTURE))
                        .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
                }
            }
        }
        let backend = CrosstermBackend::new(writer);
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
            terminal: Some(terminal),
            overlay_terminal: None,
            mode,
            overlay_screen_active: false,
            overlay_mouse_override_active: false,
            overlay_mouse_capture: false,
            mouse_capture,
            exit_hint: None,
            startup_flushed: false,
            transcript_flushed_len: 0,
            synchronized_output,
        })
    }

    fn set_exit_hint(&mut self, exit_hint: Option<String>) {
        self.exit_hint = exit_hint;
    }

    /// Paint a single centered status line, flushed before the first real
    /// `draw_app` frame. The picker exits into a blank viewport while
    /// `Agent::resume`/`Agent::build` walk the workspace; without this the
    /// user just stares at empty space until the main loop starts.
    fn draw_startup_placeholder(&mut self, message: &str) -> Result<()> {
        let message = message.to_string();
        self.term()
            .draw(|frame| {
                let area = frame.area();
                if area.width == 0 || area.height == 0 {
                    return;
                }
                let line = Line::from(vec![
                    Span::styled(
                        "● ",
                        Style::default()
                            .fg(crate::render::theme::accent())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        message,
                        Style::default()
                            .fg(crate::render::theme::secondary())
                            .add_modifier(Modifier::BOLD),
                    ),
                ]);
                let paragraph =
                    Paragraph::new(vec![line]).alignment(ratatui::layout::Alignment::Center);
                let row = area.y + area.height / 2;
                let target = Rect {
                    x: area.x,
                    y: row,
                    width: area.width,
                    height: 1,
                };
                frame.render_widget(paragraph, target);
            })
            .map(|_| ())
            .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        let _ = self.term().backend_mut().flush();
        Ok(())
    }

    fn draw_app(&mut self, app: &mut TuiApp) -> Result<()> {
        let overlay_frame = app.transcript_overlay.is_some() || self.overlay_screen_active;
        if app.pending_resize {
            app.pending_resize = false;
            self.wipe_inline_viewport_for_resize()?;
        }
        self.apply_terminal_title(app)?;
        // Promote into / demote out of the alt-screen buffer based on
        // whether the user has the transcript overlay open. No-op when
        // `mode == AlternateScreen` (the whole terminal is already
        // fullscreen there). Must run BEFORE we borrow the terminal
        // for the draw, because it swaps `self.terminal`.
        self.sync_overlay_screen(app.transcript_overlay.is_some())?;
        self.sync_overlay_mouse_mode(
            app.transcript_overlay
                .as_ref()
                .map(|state| state.mode.mouse_capture()),
        )?;
        // After the swap, decide which render path to take. Inline
        // mode + no overlay = `render_inline` (small viewport painted
        // over scrollback). Alt-screen mode OR overlay-screen swap =
        // `render` (fullscreen draw, with the overlay branch picking
        // the right widget).
        let use_fullscreen_render =
            self.mode == TerminalMode::AlternateScreen || self.overlay_screen_active;
        if !use_fullscreen_render {
            self.flush_history(app)?;
        }
        let synchronized = self.synchronized_output && !overlay_frame;
        let terminal = self.term();
        // DEC 2026 Begin Synchronized Update bracket. Writing it through
        // the backend buffer puts it ahead of the cell-diff bytes that
        // `terminal.draw` is about to emit; capable terminals start
        // buffering at parse time and commit the whole frame when they
        // see the matching End Synchronized Update written below.
        // Unsupported terminals silently ignore both sequences.
        let begin_outcome = if synchronized {
            terminal
                .backend_mut()
                .write_all(BEGIN_SYNCHRONIZED_UPDATE.as_bytes())
        } else {
            Ok(())
        };
        // `terminal.draw` returns a value that borrows the terminal,
        // which would block the post-draw `backend_mut` reborrow below.
        // Collapse to `Result<(), io::Error>` immediately so the borrow
        // ends before we reach for the backend again.
        let draw_outcome: io::Result<()> = if let Err(err) = begin_outcome {
            Err(err)
        } else if use_fullscreen_render {
            terminal.draw(|frame| render(frame, app)).map(|_| ())
        } else {
            terminal.draw(|frame| render_inline(frame, app)).map(|_| ())
        };
        // Always emit ESU (even when draw fails) so the terminal does
        // not stay parked in a buffered-update state — the spec lets a
        // capable terminal time the bracket out on its own, but closing
        // it promptly keeps the visible frame in sync with our state.
        let end_outcome = if synchronized {
            let backend = terminal.backend_mut();
            backend
                .write_all(END_SYNCHRONIZED_UPDATE.as_bytes())
                .and_then(|()| backend.flush())
        } else {
            Ok(())
        };
        match draw_outcome.and(end_outcome) {
            Ok(()) => Ok(()),
            Err(err) => Err(SqueezyError::Terminal(err.to_string())),
        }
    }

    /// Reconcile the alt-screen-for-overlay swap state with the
    /// caller's request. No-op when we're already in alt-screen mode
    /// for the whole session, or when the requested state already
    /// matches. Otherwise:
    ///
    /// - **Inline → overlay**: drop the inline `Terminal` so its
    ///   writer releases stdout, send `EnterAlternateScreen`
    ///   directly, then build a fresh fullscreen `Terminal` over a
    ///   new stdout writer. The original inline-mode terminal
    ///   scrollback is preserved by the terminal emulator's main
    ///   buffer; the overlay paints into the separate alt-screen
    ///   buffer.
    /// - **Overlay → inline**: reverse — drop the alt-screen
    ///   `Terminal`, send `LeaveAlternateScreen` (which restores the
    ///   main buffer with all the pre-overlay scrollback intact),
    ///   then build a fresh `Viewport::Inline` `Terminal` to resume
    ///   painting the bottom-anchored TUI viewport.
    fn sync_overlay_screen(&mut self, want_overlay_full: bool) -> Result<()> {
        if self.mode != TerminalMode::Inline {
            return Ok(());
        }
        match (self.overlay_screen_active, want_overlay_full) {
            (false, true) => self.enter_overlay_screen(),
            (true, false) => self.leave_overlay_screen(),
            _ => Ok(()),
        }
    }

    fn sync_overlay_mouse_mode(&mut self, overlay_mouse_capture: Option<bool>) -> Result<()> {
        match overlay_mouse_capture {
            Some(scrollbar_drag) => {
                if self.overlay_mouse_override_active
                    && self.overlay_mouse_capture == scrollbar_drag
                {
                    return Ok(());
                }
                let terminal = self.term();
                set_transcript_overlay_mouse_mode(terminal.backend_mut(), scrollbar_drag, false)
                    .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
                self.overlay_mouse_override_active = true;
                self.overlay_mouse_capture = scrollbar_drag;
                Ok(())
            }
            None => {
                if !self.overlay_mouse_override_active {
                    return Ok(());
                }
                let restore_main_mouse_capture = self.mouse_capture;
                let terminal = self.term();
                set_transcript_overlay_mouse_mode(
                    terminal.backend_mut(),
                    false,
                    restore_main_mouse_capture,
                )
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
                self.overlay_mouse_override_active = false;
                self.overlay_mouse_capture = false;
                Ok(())
            }
        }
    }

    fn enter_overlay_screen(&mut self) -> Result<()> {
        // Lazily build the alt-screen ratatui `Terminal` the first
        // time the overlay opens. `Viewport::Fullscreen` is the
        // important detail: ratatui's fullscreen construction does
        // NOT call `append_lines`, so it never scrolls main-buffer
        // content. The terminal stays alive for the rest of the
        // session — subsequent opens just re-enter alt-screen via
        // ANSI without rebuilding the ratatui state.
        if self.overlay_terminal.is_none() {
            let writer = TerminalWriter::from_env(io::stdout());
            let backend = CrosstermBackend::new(writer);
            self.overlay_terminal = Some(
                Terminal::new(backend).map_err(|err| SqueezyError::Terminal(err.to_string()))?,
            );
        }
        // Switch the terminal emulator into the alt-screen buffer.
        // Write through the inline Terminal's backend so the bytes
        // serialize with any pending inline output.
        {
            let inline = self
                .terminal
                .as_mut()
                .expect("primary inline terminal lost — unreachable after `enter`");
            enter_transcript_overlay_screen(inline.backend_mut())
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        }
        // Force the overlay terminal to paint from scratch — its
        // internal "previously drawn" buffer is stale (from the
        // last overlay open, or empty on first use) but the
        // alt-screen buffer we're about to draw into is blank.
        self.overlay_terminal
            .as_mut()
            .expect("just built / persistent")
            .clear()
            .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        self.overlay_screen_active = true;
        Ok(())
    }

    fn leave_overlay_screen(&mut self) -> Result<()> {
        // Last write through the overlay Terminal's backend before
        // we leave alt-screen — same reasoning as
        // `enter_overlay_screen`: serialize the escape sequence
        // with any final overlay-render bytes.
        {
            let overlay = self
                .overlay_terminal
                .as_mut()
                .expect("overlay terminal must be built when overlay-screen is active");
            leave_transcript_overlay_screen(overlay.backend_mut(), self.mouse_capture)
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        }
        self.overlay_mouse_override_active = false;
        self.overlay_mouse_capture = false;
        // The terminal emulator has restored the main buffer. The
        // pre-overlay inline TUI viewport content (input row,
        // status, hint) is still sitting in the bottom rows where
        // it was when alt-screen entered, because we never touched
        // the main buffer during the overlay. The inline Terminal's
        // internal "last drawn" state still matches that visible
        // content, so the next `draw` diffs against the correct
        // baseline and only emits cells that genuinely changed.
        //
        // What CAN have changed during the overlay: new transcript
        // entries (committed reasoning, assistant message) that
        // arrived while we were suppressing `flush_history`. The
        // next `draw_app` call will pick them up and push them
        // into scrollback via `insert_before`, scrolling the
        // pre-overlay viewport down into its new position
        // naturally.
        self.overlay_screen_active = false;
        Ok(())
    }

    fn apply_terminal_title(&mut self, app: &mut TuiApp) -> Result<()> {
        let elapsed_ms = prompt_elapsed_ms(app);
        let desired = terminal_title_for(app.terminal_title_state, &app.directory, elapsed_ms);
        if desired == app.last_terminal_title {
            return Ok(());
        }
        let backend = self.term().backend_mut();
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
        if self.mode != TerminalMode::Inline || self.overlay_screen_active {
            return Ok(());
        }
        let viewport_top = self.term().get_frame().area().y;
        let terminal = self.term();
        execute!(
            terminal.backend_mut(),
            MoveTo(0, viewport_top),
            Clear(ClearType::FromCursorDown)
        )
        .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        terminal
            .clear()
            .map_err(|err| SqueezyError::Terminal(err.to_string()))
    }

    fn flush_history(&mut self, app: &TuiApp) -> Result<()> {
        if self.mode != TerminalMode::Inline || self.overlay_screen_active {
            return Ok(());
        }
        let width = self
            .term()
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
        self.term()
            .insert_before(height, |buffer| render_lines_to_buffer(buffer, lines))
            .map_err(|err| SqueezyError::Terminal(err.to_string()))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        // If we exited while the overlay-screen swap was active,
        // leave the alt-screen buffer first so the user's main-buffer
        // scrollback (the inline conversation history) is what stays
        // visible after the process tears down. The overlay terminal
        // owns the writer that's currently routed to alt-screen.
        if self.overlay_screen_active
            && let Some(overlay) = self.overlay_terminal.as_mut()
        {
            let _ = leave_transcript_overlay_screen(overlay.backend_mut(), false);
            self.overlay_screen_active = false;
            self.overlay_mouse_override_active = false;
            self.overlay_mouse_capture = false;
        }
        // Drop the overlay terminal explicitly so its writer
        // releases stdout before the primary terminal does its
        // teardown writes below.
        drop(self.overlay_terminal.take());
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        match self.mode {
            TerminalMode::Inline => {
                let _ = execute!(
                    terminal.backend_mut(),
                    PopKeyboardEnhancementFlags,
                    Print(RESET_KEYBOARD_ENHANCEMENT_FLAGS),
                    DisableModifyOtherKeys,
                    DisableBracketedPaste,
                    DisableFocusChange,
                    DisableAlternateScroll,
                    Print(DISABLE_MOUSE_MODES),
                    Print("\x1b]0;\x07"),
                    Print(CLEAR_SCROLLBACK_AND_VISIBLE)
                );
            }
            TerminalMode::AlternateScreen => {
                let _ = execute!(
                    terminal.backend_mut(),
                    PopKeyboardEnhancementFlags,
                    Print(RESET_KEYBOARD_ENHANCEMENT_FLAGS),
                    DisableModifyOtherKeys,
                    DisableBracketedPaste,
                    DisableFocusChange,
                    DisableAlternateScroll,
                    Print(DISABLE_MOUSE_MODES),
                    Print("\x1b]0;\x07"),
                    Clear(ClearType::All),
                    MoveTo(0, 0),
                    LeaveAlternateScreen
                );
            }
        }
        let _ = terminal.show_cursor();
        if let Some(hint) = &self.exit_hint {
            let _ = writeln!(terminal.backend_mut(), "{hint}");
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
        match reasoning_run_info(&app.transcript, index) {
            Some(ReasoningRun::Suppressed) => continue,
            Some(ReasoningRun::Lead { extras }) => {
                if app.show_reasoning_usage
                    && let TranscriptEntryKind::Reasoning(snapshot) = &item.kind
                {
                    lines.extend(reasoning_block_lines_with_extras(
                        &snapshot.display_text,
                        item.collapsed,
                        false,
                        extras,
                    ));
                    lines.push(Line::from(""));
                }
                continue;
            }
            None => {}
        }
        match tool_run_info(&app.transcript, index, app.coalesce_tool_runs) {
            Some(ToolRun::Suppressed) => continue,
            Some(ToolRun::Lead { extras }) => {
                let members = collect_tool_run_members(&app.transcript, index, extras);
                lines.extend(format_grouped_tool_result_entry(
                    &members,
                    item.collapsed,
                    false,
                    app.tool_output_verbosity,
                    Some(width),
                    ToolCardSurface::Tinted,
                ));
                continue;
            }
            None => {}
        }
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
