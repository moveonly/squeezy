//! The fullscreen terminal lifecycle guard (extracted from `lib.rs`, deep-review
//! #52 MOVE 1). [`TerminalGuard`] owns the alt-screen ratatui terminal, drives
//! every frame draw, and performs clean / emergency teardown. This is a pure
//! code-motion of the `TerminalGuard` `struct` and its `impl`/`Drop` blocks; the
//! free helpers it calls (`emit_*`, `resolve_*`, `refresh_*`, the clean-exit
//! mirror, `render`) remain single-sourced in the crate root and are imported
//! below.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::Instant;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};
use squeezy_core::{Result, SqueezyError, TuiSynchronizedOutput};

use crate::terminal_writer::TerminalWriter;
use crate::{
    BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE, MIRROR_FALLBACK_WIDTH, TuiApp,
    apply_startup_terminal_profile, close_subagent_compare, compensate_main_scroll_for_append,
    dogfood, emit_finish_fullscreen_mirror_streamed, emit_finish_fullscreen_restore,
    emit_terminal_emergency_teardown, emit_terminal_enter_setup, main_render_cache,
    mark_full_redraw_after_resume, metrics, precheck_terminal_environment, prompt_elapsed_ms,
    refresh_attention_route, refresh_change_summary, refresh_duplicate_folds, refresh_error_lenses,
    refresh_health_markers, refresh_lane_fold, refresh_related_links, refresh_review_board,
    refresh_session_timeline, refresh_subagent_timeline, refresh_transcript_index,
    refresh_turn_outline, render, resolve_synchronized_output, review_board_reconcile_cursor,
    signal_teardown, subagent_compare_records_present, terminal_restore, terminal_title_for, toast,
    transcript_lines_for_render,
};
// Editor handoff (suspend → external $EDITOR) is Unix-only; these are used solely by the
// #[cfg(unix)] suspend_and_resume / run_pending_editor_handoff methods below, so the import
// must be cfg-gated too or Windows clippy trips `-D unused-imports` (deep-review #52 CI follow-up).
#[cfg(unix)]
use crate::{apply_editor_handoff_outcome, editor_handoff, report_editor_handoff_error};

pub(crate) struct TerminalGuard {
    /// `Option` so we can drop and rebuild the ratatui `Terminal`, and
    /// so `Drop` can `take` it for emergency teardown. Always the
    /// fullscreen alt-screen terminal (`Viewport::Fullscreen`), `Some`
    /// after `enter`.
    terminal: Option<Terminal<CrosstermBackend<TerminalWriter>>>,
    /// True after `enter` emits `EnterAlternateScreen`; cleared by `Drop`
    /// (and by Phase 2's `finish_fullscreen`). Idempotence guard so the
    /// alt-screen is left exactly once.
    pub(crate) alt_screen_active: bool,
    /// Whether mouse capture is enabled for the main screen. Defaults on in
    /// fullscreen (alt-screen norm) unless `SQUEEZY_MOUSE_CAPTURE=0`. When on,
    /// the click registry is live; Shift+drag / Shift+wheel are the native
    /// selection / scrollback escape hatch. Resolved once at `enter`, used there
    /// to emit the click-capture enable sequence, and re-read by
    /// `suspend_and_resume` / `run_pending_editor_handoff` (Unix) when they
    /// re-enter the alternate screen, so mouse capture is re-armed to the same
    /// policy on resume. Off Unix there is no suspend/resume reader, so the field
    /// is dead there — hence the `cfg`-gated `dead_code` allow.
    #[cfg_attr(not(unix), allow(dead_code))]
    mouse_capture: bool,
    exit_hint: Option<String>,
    /// Resolved DEC 2026 synchronized-output flag. Computed once at
    /// startup from the user's [`TuiSynchronizedOutput`] policy plus
    /// terminal-capability detection; consulted around every frame
    /// draw to wrap output in Begin/End Synchronized Update sequences.
    synchronized_output: bool,
    /// Source of the current terminal dimensions. Production uses
    /// [`crate::size_source::RealSize`] (delegates to `crossterm::terminal::size`);
    /// tests inject a [`crate::size_source::FixedSize`] so the clean-exit mirror
    /// width can be driven at a deterministic size with no real TTY.
    size_source: Box<dyn crate::size_source::SizeSource>,
    /// Shared per-frame byte counter installed into the terminal writer. The
    /// writer bumps it on every write; `draw_app` resets it at frame begin and
    /// reads the delta at frame end into [`metrics::RenderMetrics::bytes_emitted`].
    byte_counter: metrics::ByteCounter,
}

impl TerminalGuard {
    /// The single terminal that receives every draw. There is no overlay
    /// terminal swap in fullscreen: Ctrl+T / config / status-line are rendered
    /// states on this same terminal, selected inside `render`.
    pub(crate) fn term(&mut self) -> &mut Terminal<CrosstermBackend<TerminalWriter>> {
        self.terminal
            .as_mut()
            .expect("primary terminal lost — unreachable after `enter`")
    }
}

impl TerminalGuard {
    pub(crate) fn enter(synchronized_output: TuiSynchronizedOutput) -> Result<Self> {
        // Refuse interactive TUI startup when stdout is not a real terminal
        // or when $TERM signals a minimal sink (CI, serial, dumb). Runs
        // before `enable_raw_mode()` and any ANSI byte so a redirected
        // stdout never gets raw VT bytes written into it.
        if let Some(message) = precheck_terminal_environment(
            || io::stdout().is_terminal(),
            |key: &str| env::var_os(key),
        ) {
            return Err(SqueezyError::Terminal(message));
        }
        let synchronized_output = resolve_synchronized_output(synchronized_output);
        enable_raw_mode().map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        // Wrap stdout in the env-gated debug-tap writer so every
        // subsequent ANSI sequence — startup setup, draw bytes, and
        // teardown — is mirrored to the log when
        // `SQUEEZY_TUI_WRITE_LOG` is set. When unset the wrapper is a
        // thin pass-through.
        let mut writer = TerminalWriter::from_env(io::stdout());
        // Install the per-frame byte counter so the render-budget HUD can
        // report bytes-emitted-per-frame. The guard keeps the other end of the
        // `Arc` to reset/read it across each `draw_app`.
        let byte_counter: metrics::ByteCounter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        writer.set_byte_counter(Arc::clone(&byte_counter));
        // Apply the persisted terminal profile (§12.7.3) before the first paint:
        // pin the saved colour-depth override into the palette and resolve mouse
        // capture from the profile. Mouse capture defaults ON in fullscreen (the
        // alt-screen norm: click-to-focus / scroll / drag are part of the UI) and
        // hijacks native text selection / scrollback, so Shift+drag / Shift+wheel
        // are the escape hatch. The literal `SQUEEZY_MOUSE_CAPTURE=0` (and
        // `$NO_COLOR` for colour) still win over the profile.
        let mouse_capture = apply_startup_terminal_profile();
        // Emit the whole startup setup sequence (keyboard flags, alt-screen
        // entry, bracketed paste / focus, mouse capture) through the shared free
        // helper so a test can drive the exact same bytes into a `Capture`
        // writer with no real TTY.
        emit_terminal_enter_setup(&mut writer, mouse_capture)
            .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        // Crash safety (Phase 9): install the panic hook now that the terminal is
        // in raw mode / the alternate screen, and publish the alt-screen state so
        // the hook (and the Unix signal handlers) leave it exactly once. Both are
        // idempotent across the process and additive — the clean lifecycle is
        // unchanged; these only cover the paths where `Drop` cannot run (a panic
        // message printing pre-unwind, or a SIGTERM/SIGHUP killing the process).
        // The panic hook is restored in `TerminalGuard::Drop` — which runs on
        // every exit (clean, inline, or an error-path `?`), so the install is
        // unconditional and the restore is symmetric.
        signal_teardown::set_alt_screen_active(true);
        signal_teardown::install_panic_hook();
        let backend = CrosstermBackend::new(writer);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        Ok(Self {
            terminal: Some(terminal),
            alt_screen_active: true,
            mouse_capture,
            exit_hint: None,
            synchronized_output,
            size_source: Box::new(crate::size_source::RealSize),
            byte_counter,
        })
    }

    /// Build a guard wired to a headless [`TerminalWriter::Capture`] backend at a
    /// deterministic `(w, h)` (via [`crate::size_source::FixedSize`]), so
    /// [`Self::draw_app`] and the clean-exit mirror — and the bytes each emits —
    /// can be asserted with no real TTY. Returns the guard plus the shared sink
    /// the caller reads the emitted ANSI from.
    ///
    /// Uses a fixed-area viewport so the fullscreen `render()` never hits
    /// ratatui's autoresize (which queries the real terminal); the clean-exit
    /// mirror reads its width from the injected `FixedSize`. No startup setup
    /// bytes are emitted — the enter sequence is covered directly against
    /// [`emit_terminal_enter_setup`].
    #[cfg(test)]
    pub(crate) fn for_capture_test(w: u16, h: u16) -> (Self, Arc<std::sync::Mutex<Vec<u8>>>) {
        let sink: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let byte_counter: metrics::ByteCounter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut writer = TerminalWriter::capture(sink.clone());
        writer.set_byte_counter(Arc::clone(&byte_counter));
        let backend = CrosstermBackend::new(writer);
        let viewport = Viewport::Fixed(Rect::new(0, 0, w.max(1), h.max(1)));
        let terminal = Terminal::with_options(backend, TerminalOptions { viewport })
            .expect("capture terminal builds with a fixed viewport");
        // Publish the shared crash-path alt-screen flag exactly as the real
        // `enter()` does (which sets the local field AND calls
        // `set_alt_screen_active(true)`). `Drop` now consults the shared flag to
        // decide whether to leave the alternate screen (deep-review #27), so a
        // capture guard that only set the local field would not match production
        // — its `Drop` would skip the leave whenever a prior test left the shared
        // flag clear. Keeping the two in sync makes the capture guard faithful.
        signal_teardown::set_alt_screen_active(true);
        let guard = Self {
            terminal: Some(terminal),
            alt_screen_active: true,
            mouse_capture: true,
            exit_hint: None,
            synchronized_output: false,
            size_source: Box::new(crate::size_source::FixedSize(w, h)),
            byte_counter,
        };
        (guard, sink)
    }

    /// Test-only sibling of [`Self::for_capture_test`] whose capture writer also
    /// fires `on_write` after every emitted chunk, letting a test observe shared
    /// state (e.g. the crash-path alt-screen flag) at the exact byte boundary it
    /// is emitted. Used to pin that [`Self::finish_fullscreen`] clears the
    /// alt-screen flag BEFORE the first transcript-mirror row is written, not
    /// after the whole mirror (deep-review #93).
    #[cfg(test)]
    pub(crate) fn for_probe_test(
        w: u16,
        h: u16,
        on_write: crate::terminal_writer::ProbeCallback,
    ) -> (Self, Arc<std::sync::Mutex<Vec<u8>>>) {
        let sink: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let byte_counter: metrics::ByteCounter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut writer = TerminalWriter::capture_probe(sink.clone(), on_write);
        writer.set_byte_counter(Arc::clone(&byte_counter));
        let backend = CrosstermBackend::new(writer);
        let viewport = Viewport::Fixed(Rect::new(0, 0, w.max(1), h.max(1)));
        let terminal = Terminal::with_options(backend, TerminalOptions { viewport })
            .expect("probe capture terminal builds with a fixed viewport");
        signal_teardown::set_alt_screen_active(true);
        let guard = Self {
            terminal: Some(terminal),
            alt_screen_active: true,
            mouse_capture: true,
            exit_hint: None,
            synchronized_output: false,
            size_source: Box::new(crate::size_source::FixedSize(w, h)),
            byte_counter,
        };
        (guard, sink)
    }

    /// Test-only: force the resolved DEC 2026 synchronized-output flag on (the
    /// `for_capture_test` constructor defaults it off). Lets a capture test
    /// drive a frame with synchronized output enabled and assert the begin/end
    /// brackets without depending on terminal-capability env detection.
    #[cfg(test)]
    pub(crate) fn set_synchronized_output(&mut self, enabled: bool) {
        self.synchronized_output = enabled;
    }

    pub(crate) fn set_exit_hint(&mut self, exit_hint: Option<String>) {
        self.exit_hint = exit_hint;
    }

    /// Clean, successful shutdown of the fullscreen renderer: leave the alternate
    /// screen and mirror the session's collapsed transcript into the user's real
    /// terminal scrollback, then restore terminal modes — so closing squeezy
    /// leaves a durable, self-describing record (the collapsed history plus a
    /// `squeezy sessions resume …` pointer) instead of a blank prompt.
    ///
    /// Called once, on the normal main-loop exit, BEFORE the guard drops; `Drop`
    /// stays the emergency-only fallback for panics/signals and never mirrors.
    /// The two paths share the `alt_screen_active` idempotence flag, so the
    /// alternate screen is left EXACTLY once:
    ///   * On a clean exit this runs first, performs the full shutdown, and
    ///     clears the flag; `Drop` then sees `alt_screen_active == false` and
    ///     skips its own `LeaveAlternateScreen`, re-emitting only the idempotent
    ///     mode restores.
    ///   * A second call short-circuits the same way (the flag is already clear).
    ///
    /// Best-effort: the IO `Result` exists so a `Capture` test can assert the
    /// byte order and so the method composes; the caller discards it with
    /// `let _ =` so a teardown IO error never masks a completed session (and
    /// `Drop`'s idempotent emergency teardown still runs).
    pub(crate) fn finish_fullscreen(&mut self, app: &TuiApp) -> io::Result<()> {
        // Idempotence gate: nothing to mirror if the alternate screen has already
        // been left (prior call / Drop).
        if !self.alt_screen_active {
            return Ok(());
        }
        // Dogfood telemetry (§12.10.3): on a clean exit, append the session's
        // final counter snapshot as one JSONL line when persistence is opted in.
        // Best-effort: a disk error here never blocks the terminal restore.
        if app.dogfood_metrics.jsonl_enabled() {
            let _ = app.dogfood_metrics.flush_jsonl();
        }
        // Width from the live terminal, with a conservative 80-column fallback
        // when the size is unavailable (piped stdout / detached TTY). Reusing
        // `size_source` keeps this testable with no real TTY.
        let width = match self.size_source.size() {
            Ok((w, _)) if w > 0 => w,
            _ => MIRROR_FALLBACK_WIDTH,
        };
        let exit_hint = self.exit_hint.clone();
        // Snapshot the effective hyperlink capability before borrowing the
        // backend (the call needs `&mut self` for `term()`, so the read can't
        // straddle that borrow).
        let links = app.effective_hyperlink_caps();
        // RESTORE the terminal FIRST — leave the alternate screen (never
        // `\x1b[3J`) and put every mode back — then flush, all BEFORE allocating
        // the heavy collapsed-transcript mirror below. If that allocation aborts
        // (alloc-error / OOM-kill), the user is left in a restored normal buffer
        // rather than stranded in a raw alternate screen. Raw mode stays on so
        // the CRLF mirror rows that follow do not stair-step; it is disabled last.
        {
            let backend = self.term().backend_mut();
            emit_finish_fullscreen_restore(backend)?;
            backend.flush()?;
        }
        // The alternate screen has now been LEFT (and flushed). Clear both flags
        // immediately — BEFORE building/emitting the heavy transcript mirror —
        // rather than after the whole mirror is written. This shrinks the window
        // in which a panic hook / SIGTERM firing mid-shutdown would see the shared
        // flag still `true` and re-leave an alternate screen we already left: from
        // the entire mirror emission down to the handful of restore bytes above.
        // Clearing the guard-local flag too keeps `Drop` from re-leaving. Set
        // BEFORE the fallible mirror/flush below so an error there still prevents a
        // double-leave. (deep-review #93)
        self.alt_screen_active = false;
        signal_teardown::set_alt_screen_active(false);
        // Collapsed-by-default mirror over the WHOLE transcript, opened by the
        // startup/session card, built through the same fullscreen line pipeline
        // `render()` draws — so the mirrored rows match what the user saw. Built
        // AFTER the restore above so an allocation failure here cannot strand the
        // terminal.
        //
        // Wrap at the session's PAINTED text width (the live frame reserves a
        // scrollbar gutter, and a minimap rail when shown, so the text column is
        // narrower than the raw terminal width). Wrapping at the raw width here
        // re-flows long lines at a different column than every painted frame and
        // misses the warm Main/EntryWrap caches. `main_text_width` is stamped each
        // frame; fall back to the raw terminal width before the first paint (it
        // returns 0 then, which `main_text_width` maps to 80). (deep-review #79)
        let painted_width = app.main_text_width.get();
        let mirror_width = if painted_width > 0 {
            painted_width
        } else {
            width
        };
        let lines = transcript_lines_for_render(app, Some(mirror_width), true);
        let backend = self.term().backend_mut();
        // Emit the mirror into the now-restored normal buffer: the CRLF mirror
        // rows (with OSC 8 hyperlinks when capable) and the resume hint, all
        // becoming native scrollback. The rows are ALREADY wrapped to `mirror_width`
        // by the cache-hitting line pipeline above, so stream them a bounded chunk
        // at a time instead of allocating one full-transcript buffer + re-wrapping
        // the whole session a second time (deep-review #69). Emit at the painted
        // width so the per-row column scan matches the rows it built.
        emit_finish_fullscreen_mirror_streamed(
            backend,
            &lines,
            mirror_width,
            exit_hint.as_deref(),
            links,
        )?;
        // NOTE: the alt-screen flags were already cleared right after the restore
        // above (deep-review #93), shrinking the crash re-leave race to the
        // restore bytes rather than the whole mirror emission below.
        // NOTE: the previous panic hook is restored in `Drop` (which always runs
        // after this clean exit), so it is NOT restored here — that keeps the
        // restore symmetric across clean / inline / error-path exits instead of
        // only the alt-screen clean-exit path. See `Drop`.
        // Raw mode is the very last thing disabled — after every CRLF write above
        // — so bare-`\n` stair-stepping can't occur mid-mirror. Then show the
        // hardware cursor `enter` hid, and flush so everything is emitted before
        // the guard drops.
        let _ = disable_raw_mode();
        let _ = self.term().show_cursor();
        self.term().backend_mut().flush()
    }

    /// Cooperative SIGTSTP (Ctrl+Z) suspend/resume, driven from the main loop
    /// when [`signal_teardown::take_suspend_request`] returns `true`.
    ///
    /// A signal handler cannot do this work safely: leaving the alternate screen
    /// cleanly and redrawing from model state both need the guard's writer and
    /// the live `TuiApp`, and they must not race the renderer's own writes or run
    /// on a half-mutated model. So the SIGTSTP handler only flips a flag and the
    /// loop calls this at a safe point. Suspend during a running turn is safe:
    /// the turn's async work and `app.cancel` token are untouched — we only park
    /// the process and restore the terminal around the stop. On resume the turn
    /// keeps streaming and the forced full redraw repaints whatever it produced.
    ///
    /// Order (this is the load-bearing contract the tests pin):
    ///   1. RESTORE the terminal to a sane shell state BEFORE the process stops:
    ///      leave the alternate screen, disable mouse / bracketed-paste / focus /
    ///      alternate-scroll, reset keyboard-enhancement flags + title, show the
    ///      hardware cursor, and disable raw mode LAST. This reuses the exact
    ///      single-sourced [`emit_terminal_emergency_teardown`] bytes the panic /
    ///      signal / Drop paths emit (no transcript mirror — that is the heavy
    ///      clean-exit), so the shell that takes over sees a normal terminal.
    ///   2. Re-raise `SIGTSTP` with the default disposition so job control
    ///      actually stops squeezy (the shell reclaims the terminal). Execution
    ///      blocks here until `SIGCONT` (e.g. `fg`).
    ///   3. On resume, RE-ENTER: re-enable raw mode, replay
    ///      [`emit_terminal_enter_setup`] (alt-screen, clear, home, bracketed
    ///      paste, focus, mouse capture, hidden cursor), re-publish the crash-path
    ///      alt-screen flag, and mark the app dirty so the very next loop frame
    ///      repaints everything from model state.
    ///
    /// Best-effort throughout: every emit is `let _ =` so a flaky terminal cannot
    /// abort the suspend mid-restore (which would leave a corrupted screen). A
    /// no-op when the alternate screen has already been left (a Ctrl+Z racing a
    /// clean exit), where there is nothing to restore.
    #[cfg(unix)]
    pub(crate) fn suspend_and_resume(&mut self, app: &mut TuiApp) {
        // Nothing to manage if the alternate screen has already been left (a
        // Ctrl+Z arriving after the clean-exit path already tore down).
        if !self.alt_screen_active {
            return;
        }

        // 1. Restore the terminal BEFORE stopping. Reuse the emergency-teardown
        //    bytes (leave alt-screen + restore modes, NO transcript mirror), then
        //    show the cursor and disable raw mode last — exactly the clean
        //    crash-path restore, so the shell inherits a normal terminal.
        {
            let backend = self.term().backend_mut();
            let _ = emit_terminal_emergency_teardown(backend, /* alt_screen_active = */ true);
            let _ = backend.flush();
        }
        let _ = self.term().show_cursor();
        let _ = disable_raw_mode();
        // The alternate screen is left; keep both the guard flag and the shared
        // crash-path flag in sync so a panic/signal firing WHILE we are stopped
        // does not try to leave it again.
        self.alt_screen_active = false;
        signal_teardown::set_alt_screen_active(false);

        // 2. Actually stop (job control). Blocks until SIGCONT (`fg`). The async
        //    turn, if any, is untouched — only this thread parks.
        signal_teardown::reraise_sigtstp_default();

        // 3. Resumed (SIGCONT). Re-enter the alternate screen and clear, then
        //    force a full repaint from model state on the next frame.
        let _ = enable_raw_mode();
        {
            let mouse_capture = self.mouse_capture;
            let backend = self.term().backend_mut();
            let _ = emit_terminal_enter_setup(backend, mouse_capture);
            let _ = backend.flush();
        }
        self.alt_screen_active = true;
        signal_teardown::set_alt_screen_active(true);
        // Force a full redraw from model state: clear ratatui's diff baseline so
        // the next `draw` repaints every cell (the alternate screen we just
        // re-entered is blank), then mark the app so the loop draws on its very
        // next turn regardless of the idle-skip gate.
        let _ = self.term().clear();
        mark_full_redraw_after_resume(app);
    }

    /// Execute a queued External Editor Handoff (§12.6.5) if one is pending.
    ///
    /// The `Alt+e` dispatch only resolves the editor and stamps
    /// [`TuiApp::pending_editor_handoff`]; the run loop owns this guard, so it is
    /// the right place to suspend the alt-screen around the spawn (running a
    /// full-screen editor *inside* the alt-screen would fight it — leaving the
    /// alt-screen first is the safe option the spec calls for).
    ///
    /// Order mirrors [`Self::suspend_and_resume`]:
    ///   1. RESTORE the terminal to a sane shell state (reuse the single-sourced
    ///      emergency-teardown bytes: leave alt-screen, disable mouse / paste /
    ///      focus, reset title, show cursor, disable raw mode LAST) so the editor
    ///      inherits a normal terminal.
    ///   2. Run the editor on a temp file via [`editor_handoff::run_handoff`],
    ///      blocking until it exits.
    ///   3. RE-ENTER the alt-screen (raw mode, enter setup, hidden cursor) and
    ///      force a full repaint, exactly like resume.
    ///   4. Apply the outcome (open the accept/reopen/discard overlay on a real
    ///      change, status-only when unchanged, error on failure).
    ///
    /// Best-effort terminal restoration throughout: every emit is `let _ =` so a
    /// flaky terminal cannot abort the re-enter mid-restore. A no-op when no
    /// handoff is queued or the alternate screen has already been left.
    #[cfg(unix)]
    pub(crate) fn run_pending_editor_handoff(&mut self, app: &mut TuiApp) {
        let Some(request) = app.pending_editor_handoff.take() else {
            return;
        };
        if !self.alt_screen_active {
            // The screen is already torn down (a clean exit raced the handoff);
            // there is nothing safe to suspend/restore around, so drop it.
            return;
        }

        // 1. Restore the terminal BEFORE spawning the editor.
        {
            let backend = self.term().backend_mut();
            let _ = emit_terminal_emergency_teardown(backend, /* alt_screen_active = */ true);
            let _ = backend.flush();
        }
        let _ = self.term().show_cursor();
        let _ = disable_raw_mode();
        self.alt_screen_active = false;
        signal_teardown::set_alt_screen_active(false);

        // 2. Run the editor on a temp file under the platform temp dir (never a
        //    hardcoded /tmp). A dedicated monotonic counter + the pid keep the
        //    leaf unique; it is bumped per handoff so back-to-back editor runs do
        //    not collide (the queue id counter only advances on enqueue/sync).
        let seq = app.editor_handoff_temp_nonce;
        app.editor_handoff_temp_nonce += 1;
        let dir = std::env::temp_dir();
        let outcome = editor_handoff::run_handoff(
            &request.command,
            request.target,
            &request.initial_text,
            &dir,
            std::process::id(),
            seq,
            |command, path| {
                let status = std::process::Command::new(&command.program)
                    .args(&command.args)
                    .arg(path)
                    .status()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "editor exited with status {status}"
                    )))
                }
            },
        );

        // 3. RE-ENTER the alternate screen and force a full repaint.
        let _ = enable_raw_mode();
        {
            let mouse_capture = self.mouse_capture;
            let backend = self.term().backend_mut();
            let _ = emit_terminal_enter_setup(backend, mouse_capture);
            let _ = backend.flush();
        }
        self.alt_screen_active = true;
        signal_teardown::set_alt_screen_active(true);
        let _ = self.term().clear();
        mark_full_redraw_after_resume(app);

        // 4. Apply the outcome (terminal is restored, so a failure only reports).
        match outcome {
            Ok(outcome) => {
                apply_editor_handoff_outcome(app, request.target, &request.initial_text, outcome);
            }
            Err(error) => report_editor_handoff_error(app, &error),
        }
    }

    /// Terminal Restore Command (§12.9.2): drain a pending forcible terminal
    /// restore and recover a wedged terminal in place. Unlike the crash paths,
    /// this runs from a LIVE session — the user pressed `Ctrl+Alt+,` (or typed
    /// `/terminal-reset`) because the screen looks corrupted but the app is still
    /// running — so it restore-then-RE-ENTERs rather than tearing down for good.
    ///
    /// Order mirrors [`Self::suspend_and_resume`] minus the job-control stop, and
    /// reuses the exact single-sourced byte machinery so the recovery emits the
    /// same proven sequence as every other restore path:
    ///   1. RESTORE: replay [`emit_terminal_emergency_teardown`] (leave the
    ///      alternate screen, disable mouse / bracketed paste / focus / alternate
    ///      scroll, reset keyboard-enhancement flags + title, show the hardware
    ///      cursor), then disable raw mode LAST — exactly the wedged-terminal
    ///      symptoms §12.9.2 enumerates. Deliberately NEVER purges scrollback.
    ///   2. RE-ENTER: re-enable raw mode, replay [`emit_terminal_enter_setup`]
    ///      (alt-screen, clear, home, bracketed paste, focus, mouse capture, hidden
    ///      cursor), re-publish the crash-path alt-screen flag, clear ratatui's
    ///      diff baseline, and mark the app so the next frame fully repaints from
    ///      model state.
    ///
    /// Cross-platform: the emit functions and the raw-mode toggles are crossterm
    /// calls that work on both POSIX terminals and Windows console / ConPTY (where
    /// the ANSI resets are honoured by Windows Terminal), so — unlike the Unix-only
    /// suspend / editor-handoff paths — this is NOT `cfg`-gated. Best-effort
    /// throughout (`let _ =`): a flaky terminal must not abort the recovery
    /// mid-restore. A no-op (status-only) when the alternate screen has already been
    /// left, so there is nothing to re-enter.
    pub(crate) fn run_pending_terminal_restore(&mut self, app: &mut TuiApp) {
        if !app.pending_terminal_restore.take() {
            return;
        }
        // Nothing to restore if the alternate screen has already been left (a
        // clean exit raced the request). Consume the flag and report honestly.
        if !self.alt_screen_active {
            app.status = terminal_restore::restore_status(false).to_string();
            app.needs_redraw = true;
            return;
        }

        // 1. RESTORE the terminal to a sane state. Reuse the emergency-teardown
        //    bytes (leave alt-screen + restore every mode, NO transcript mirror),
        //    then show the cursor and disable raw mode last.
        {
            let backend = self.term().backend_mut();
            let _ = emit_terminal_emergency_teardown(backend, /* alt_screen_active = */ true);
            let _ = backend.flush();
        }
        let _ = self.term().show_cursor();
        let _ = disable_raw_mode();
        self.alt_screen_active = false;
        signal_teardown::set_alt_screen_active(false);

        // 2. RE-ENTER the alternate screen and force a full repaint from model
        //    state — the user lands back in a clean fullscreen surface.
        let _ = enable_raw_mode();
        {
            let mouse_capture = self.mouse_capture;
            let backend = self.term().backend_mut();
            let _ = emit_terminal_enter_setup(backend, mouse_capture);
            let _ = backend.flush();
        }
        self.alt_screen_active = true;
        signal_teardown::set_alt_screen_active(true);
        let _ = self.term().clear();
        mark_full_redraw_after_resume(app);

        app.status = terminal_restore::restore_status(true).to_string();
        // On-surface confirmation: the freshly re-entered screen is blank, so a
        // toast (painted over the repainted frame) tells the user the recovery
        // worked. Terse so it survives the toast width clamp.
        app.toasts.push(
            terminal_restore::RESTORE_DONE_TOAST,
            toast::ToastVariant::Info,
        );
    }

    /// Paint a single centered status line, flushed before the first real
    /// `draw_app` frame. The picker exits into a blank viewport while
    /// `Agent::resume`/`Agent::build` walk the workspace; without this the
    /// user just stares at empty space until the main loop starts.
    pub(crate) fn draw_startup_placeholder(&mut self, message: &str) -> Result<()> {
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

    pub(crate) fn draw_app(&mut self, app: &mut TuiApp) -> Result<()> {
        self.apply_terminal_title(app)?;
        if app.pending_resize {
            // Ratatui autoresizes the fullscreen terminal cleanly on the next
            // `draw`, so just clear the flag.
            app.pending_resize = false;
        }
        // ---- Frame-budget instrumentation: begin ----
        // This whole method is the single per-frame chokepoint for the paint, so
        // it is the one place to bracket render time, byte count,
        // rows built, cache hit/miss, and the longest entry wrap. Sample the
        // per-frame accumulators at frame begin so the deltas below isolate
        // THIS frame's work from every prior frame's.
        let frame_start = Instant::now();
        self.byte_counter
            .store(0, std::sync::atomic::Ordering::Relaxed);
        metrics::reset_rows_built();
        metrics::reset_longest_entry_wrap();
        let (hits_before, misses_before) = {
            let (mh, mm, eh, em) = main_render_cache::cache_stats();
            (mh + eh, mm + em)
        };

        // Keep the Local Transcript Index (§12.5.1) current while its overlay is
        // open: the refresh is a no-op rebuild when the transcript fingerprint is
        // unchanged, so an open-but-idle overlay costs one `u64` comparison and a
        // closed overlay costs nothing (this branch is skipped entirely).
        if app.transcript_index_open {
            refresh_transcript_index(app);
        }

        // Keep the Related-Entry Links graph (§12.5.3) current while its overlay
        // is open, on the same no-op-when-unchanged terms as the index above: an
        // open-but-idle overlay costs one `u64` comparison, a closed overlay
        // costs nothing.
        if app.related_links_open {
            refresh_related_links(app);
        }

        // Keep the Duplicate-Output Folds model (§12.5.4) current while its
        // overlay is open, on the same no-op-when-unchanged terms: an
        // open-but-idle overlay costs one `u64` comparison and a closed overlay
        // costs nothing (this branch is skipped entirely).
        if app.duplicate_folds_open {
            refresh_duplicate_folds(app);
        }

        // Keep the Error Lenses model (§12.5.6) current while its overlay is
        // open, on the same no-op-when-unchanged terms: an open-but-idle overlay
        // costs one `u64` comparison and a closed overlay costs nothing (this
        // branch is skipped entirely).
        if app.error_lens_open {
            refresh_error_lenses(app);
        }

        // Keep the Transcript Health Markers model (§12.5.7) current while its
        // overlay is open, on the same no-op-when-unchanged terms: an open-but-idle
        // overlay costs one `u64` comparison and a closed overlay costs nothing
        // (this branch is skipped entirely).
        if app.health_markers_open {
            refresh_health_markers(app);
        }

        // Keep the Semantic Turn Outline (§12.2.1) current while its overlay is
        // open, on the same no-op-when-unchanged terms: an open-but-idle overlay
        // costs one `u64` comparison and a closed overlay costs nothing (this
        // branch is skipped entirely).
        if app.turn_outline_open {
            refresh_turn_outline(app);
        }

        // Keep the Collapsible Reasoning/Tool Lanes panel (§12.2.2) current while
        // its overlay is open, on the same no-op-when-unchanged terms: an
        // open-but-idle overlay costs one `u64` comparison and a closed overlay
        // costs nothing (this branch is skipped entirely).
        if app.lane_fold_open {
            refresh_lane_fold(app);
        }

        // Keep the Session Timeline (§12.2.6) current while its overlay is open,
        // on the same no-op-when-unchanged terms: an open-but-idle overlay costs
        // one `u64` comparison and a closed overlay costs nothing (this branch is
        // skipped entirely).
        if app.session_timeline_open {
            refresh_session_timeline(app);
        }

        // Keep the Subagent Timeline Panel (§12.8.1) current while its panel is
        // open, on the same no-op-when-unchanged terms: an open-but-idle (all
        // subagents finished) panel costs one `u64` comparison and a closed panel
        // costs nothing (this branch is skipped entirely). A running subagent's
        // elapsed column advances at most once a wall-clock second, so the panel
        // re-paints at most once a second while a subagent runs — never per frame.
        if app.subagent_timeline_open {
            refresh_subagent_timeline(app);
        }

        // Keep the Live Review Board (§12.8.5) current while it is open, on the same
        // no-op-when-unchanged terms: an open-but-idle (all workers finished) board
        // costs one `u64` comparison and a closed board costs nothing (this branch
        // is skipped entirely). A running worker's elapsed column advances at most
        // once a wall-clock second, so the board re-paints at most once a second
        // while a worker runs — never per frame. The cursor is healed to a surviving
        // worker right after the rebuild so a pruned record never strands it.
        if app.review_board_open {
            refresh_review_board(app);
            review_board_reconcile_cursor(app);
        }

        // Keep the Attention Routing model (§12.8.6) current so the status-line
        // attention indicator and the `Ctrl+Alt+Z` quick-jump always reflect the
        // live subagents. Refreshed only while the session actually has subagent
        // records — a session that never spawned one pays nothing (this branch is
        // skipped entirely), and once it has some, the fingerprint only moves on a
        // real subagent event (a lifecycle flip, a fresh activity line, a pin
        // toggle), so an all-calm fanout costs one `u64` comparison per loop.
        if !app.subagent_pane.records.is_empty() {
            refresh_attention_route(app);
        }

        // Heal the Compare Subagent Outputs view (§12.8.3) when one of its two
        // marked subagents has vanished from the record list (pruned / cleared):
        // close it rather than leave it open over an empty pane. Costs nothing when
        // the view is closed (this branch is skipped).
        if app.subagent_compare.is_some() && !subagent_compare_records_present(app) {
            close_subagent_compare(app);
        }

        // Keep the What Changed Since Here? delta (§12.2.7) current while its
        // overlay is open, on the same no-op-when-unchanged terms: an open-but-idle
        // overlay costs one `u64` comparison and a closed overlay costs nothing
        // (this branch is skipped entirely).
        if app.changes_since_open {
            refresh_change_summary(app);
        }

        // Keep an unpinned main view anchored to the content the user is reading
        // when new transcript content has been appended since the last paint, so
        // a streamed answer below the fold doesn't yank the viewport forward.
        compensate_main_scroll_for_append(app);

        let paint = self.paint_one_frame(app);

        // ---- Frame-budget instrumentation: end ----
        // Stamp the snapshot ONLY on a painted frame (this method runs only when
        // the loop's redraw gate fired), so an idle frame churns nothing.
        let (hits_after, misses_after) = {
            let (mh, mm, eh, em) = main_render_cache::cache_stats();
            (mh + eh, mm + em)
        };
        let prev = app.render_metrics.get();
        let snapshot = metrics::RenderMetrics {
            render_time: frame_start.elapsed(),
            bytes_emitted: self.byte_counter.load(std::sync::atomic::Ordering::Relaxed),
            rows_built: metrics::rows_built() as usize,
            cache_hits: hits_after.wrapping_sub(hits_before),
            cache_misses: misses_after.wrapping_sub(misses_before),
            longest_entry_wrap: metrics::longest_entry_wrap(),
            frame: prev.frame.wrapping_add(1),
        };
        app.render_metrics.set(snapshot);
        if app.show_render_metrics {
            // At most one line per painted frame, gated on the HUD flag so a
            // normal session logs nothing.
            tracing::debug!(target: "squeezy_tui::render", "{}", snapshot.trace_summary());
        }

        // ---- UX latency budgets (§12.10.1) ----
        // Feed THIS painted frame's render time into the per-interaction budget
        // tracker, tagged with whatever interaction woke it. `take` clears the
        // tag so the next (possibly idle / animation) frame records nothing —
        // keeping the zero-idle-work contract. A detected violation is logged
        // once, gated on the overlay flag so a normal session stays silent.
        if let Some(kind) = app.pending_interaction.take()
            && let Some(violation) = app
                .latency
                .record(kind, snapshot.render_time, snapshot.frame)
            && app.show_latency_overlay
        {
            tracing::debug!(
                target: "squeezy_tui::latency",
                "budget violation: {} p{} {:?} > {:?} @frame {}",
                violation.kind.label().trim_end(),
                violation.percentile,
                violation.observed,
                violation.budget,
                violation.frame,
            );
        }

        // ---- Dogfood telemetry counters (§12.10.3) ----
        // Accumulate THIS painted frame's budget into the session-long dogfood
        // collector, reusing the very numbers the render-budget HUD just
        // stamped. This only runs on a painted frame (this method is the redraw-
        // gated chokepoint), so an idle frame records nothing — preserving the
        // zero-idle-work contract. Every field is a plain counter; no payload.
        app.dogfood_metrics.record_frame(dogfood::FrameSample {
            render_time: snapshot.render_time,
            bytes: snapshot.bytes_emitted,
            cache_hits: snapshot.cache_hits,
            cache_misses: snapshot.cache_misses,
            longest_wrap: snapshot.longest_entry_wrap,
            // The redraw gate coalesces multiple requests into one paint upstream
            // of this chokepoint, so a painted frame is never itself a skip; the
            // loop reports coalesced skips separately via `record_skipped_frame`.
            coalesced_skip: false,
        });

        paint
    }

    /// Stuck-Render Watchdog recovery (§12.9.1): force one clean full redraw
    /// after the watchdog has decided the render is stuck.
    ///
    /// A wedged frame can leave ratatui's internal "previous buffer" out of sync
    /// with what is actually on the terminal — its cell-diffing then emits only
    /// the (empty) delta and the screen stays frozen. So this:
    ///
    /// 1. Invalidates the diff baseline with `Terminal::clear`, which marks the
    ///    whole buffer dirty so the next `draw` repaints **every** cell (no diff
    ///    shortcut that could re-skip the stuck region).
    /// 2. Writes `Clear(All)` + `MoveTo(0,0)` straight to the backend and flushes
    ///    them, scrubbing whatever stale pixels the terminal is showing and
    ///    homing the cursor before the replacement frame lands.
    /// 3. Runs a full `draw_app` (the normal instrumented paint) to commit a
    ///    fresh frame, and flushes the backend so the bytes leave immediately.
    ///
    /// Recovery stays inside the alternate screen — it never leaves/re-enters or
    /// purges scrollback (that is the terminal guard's teardown responsibility).
    /// Best-effort: callers treat a returned error as "retry next iteration"
    /// rather than a fatal loop error.
    pub(crate) fn force_full_redraw(&mut self, app: &mut TuiApp) -> Result<()> {
        // (1) Drop ratatui's diff baseline so the next draw is a full repaint.
        self.term()
            .clear()
            .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        // (2) Scrub the visible screen and home the cursor at the backend layer.
        {
            let backend = self.term().backend_mut();
            queue!(backend, Clear(ClearType::All), MoveTo(0, 0))
                .and_then(|_| backend.flush())
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        }
        // (3) Commit a fresh frame through the normal instrumented paint, then
        //     flush so the recovery bytes are not held back behind the gate.
        app.pending_resize = true;
        let result = self.draw_app(app);
        let _ = self.term().backend_mut().flush();
        result
    }

    /// Paint exactly one frame through the fullscreen `render()` path. Factored
    /// out of [`Self::draw_app`] so the instrumentation there brackets the whole
    /// paint without duplicating it in both the timed and untimed positions.
    ///
    /// The `Terminal::draw` call is wrapped in DEC 2026 synchronized-output
    /// brackets when [`Self::synchronized_output`] is set, so a capable terminal
    /// commits the frame's cells atomically (no partial repaint / tearing).
    fn paint_one_frame(&mut self, app: &mut TuiApp) -> Result<()> {
        // Fullscreen: one full-frame draw. `render` dispatches main view vs.
        // transcript-overlay / config / status-line surfaces purely from `app`
        // state, so Ctrl+T / config / status-line render on this same terminal
        // — no alt-screen swap.
        if app.terminal_clear_pending {
            // `/clear` in fullscreen: ratatui owns the whole alt-screen buffer
            // and the next `draw` repaints it; just clear the flag.
            app.terminal_clear_pending = false;
        }
        self.draw_fullscreen_synchronized(app)
    }

    /// Run one fullscreen `Terminal::draw`, bracketed by DEC 2026
    /// synchronized-output begin/end when enabled.
    ///
    /// Ordering matters: ratatui's `Terminal::draw` flushes the backend at frame
    /// end, so BEGIN must be written-and-flushed BEFORE `draw` (otherwise the
    /// terminal could commit the cells before it saw the begin) and END
    /// written-and-flushed AFTER `draw` returns. We write begin/end directly to
    /// the backend and flush each, so the bracket reliably surrounds the whole
    /// committed frame. END is best-effort on the close side so a stray begin can
    /// never be left open: it is emitted whether or not the inner draw succeeded.
    fn draw_fullscreen_synchronized(&mut self, app: &mut TuiApp) -> Result<()> {
        let synchronized = self.synchronized_output;
        if synchronized {
            let backend = self.term().backend_mut();
            backend
                .write_all(BEGIN_SYNCHRONIZED_UPDATE.as_bytes())
                .and_then(|_| backend.flush())
                .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        }
        let drawn = self
            .term()
            .draw(|frame| render(frame, app))
            .map(|_| ())
            .map_err(|err| SqueezyError::Terminal(err.to_string()));
        if synchronized {
            // Close the synchronized update even if the draw failed, so a
            // capable terminal is never left buffering. Best-effort flush.
            let backend = self.term().backend_mut();
            let _ = backend
                .write_all(END_SYNCHRONIZED_UPDATE.as_bytes())
                .and_then(|_| backend.flush());
        }
        drawn
    }

    fn apply_terminal_title(&mut self, app: &mut TuiApp) -> Result<()> {
        let elapsed_ms = prompt_elapsed_ms(app);
        let desired = terminal_title_for(
            app.terminal_title_state,
            &app.directory,
            elapsed_ms,
            app.glyph_mode,
        );
        if desired == app.last_terminal_title {
            return Ok(());
        }
        let backend = self.term().backend_mut();
        match &desired {
            // Scrub the title before placing it inside the OSC 0 string. An OSC
            // string is terminated by BEL (0x07) or ST (ESC `\`), so a raw BEL or
            // ESC in the workspace path (POSIX-legal) would terminate/escape the
            // sequence early and emit the trailing path bytes as raw control
            // sequences. Mirrors `notification::sanitized_message`, which strips
            // exactly these bytes before its OSC 9 write for the same reason.
            Some(title) => write!(backend, "\x1b]0;{}\x07", sanitize_osc_text(title)),
            None => write!(backend, "\x1b]0;\x07"),
        }
        .and_then(|_| backend.flush())
        .map_err(|err| SqueezyError::Terminal(err.to_string()))?;
        app.last_terminal_title = desired;
        Ok(())
    }
}

/// Strip bytes that would either terminate the OSC 0 title string early (BEL,
/// `ESC \\`) or break out into raw escape sequences, mirroring
/// [`crate::notification`]'s OSC 9 scrubber. The title is built from the
/// user's workspace path, which is POSIX-legal to contain a raw BEL (0x07) or
/// ESC (0x1b); without this scrub such a byte would close the OSC string early
/// and emit the trailing path bytes as a raw control stream every spinner tick.
/// Control characters below 0x20 are dropped (newline / tab map to a space) so
/// no C0 byte can leak into the title sequence.
fn sanitize_osc_text(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for ch in title.chars() {
        match ch {
            '\u{07}' | '\u{1b}' => {}
            '\n' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Emergency-only teardown: restore the terminal modes and (if still
        // active) leave the alternate screen, but write NO user-facing content
        // and run NO render/mirror. The clean exit + transcript mirror is owned
        // by Phase 2's `finish_fullscreen`; `alt_screen_active` is the
        // idempotence contract so the alt-screen is left exactly once even when
        // both paths run.
        let _ = disable_raw_mode();
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        {
            // Dogfood telemetry (§12.10.3): a Drop that still finds the alternate
            // screen active means the clean-exit `finish_fullscreen` never ran —
            // i.e. an emergency (panic / abnormal) teardown. A clean exit clears
            // the flag first, so this never double-counts a normal shutdown. This
            // reads the guard-LOCAL field deliberately: it is the "was this a
            // clean exit?" telemetry signal, independent of who left the screen.
            if self.alt_screen_active {
                dogfood::record_emergency_teardown();
            }
            // Decide whether to leave the alternate screen from the SHARED
            // crash-path flag, read-and-cleared, ANDed with the guard-local field
            // — NOT the local field alone. A `panic!` runs the panic hook's
            // `run_emergency_teardown` BEFORE the stack unwinds into this `Drop`;
            // that hook already swapped the shared flag to `false` and emitted one
            // `LeaveAlternateScreen`. The guard-local `alt_screen_active` is still
            // `true` here, so leaving on the local field alone double-emits the
            // leave. Consulting the shared read-and-clear suppresses the second
            // leave (the mode restores re-emit harmlessly). (deep-review #27)
            let still_in_alt_screen =
                self.alt_screen_active && signal_teardown::take_alt_screen_active();
            let backend = terminal.backend_mut();
            // Single-source the teardown bytes through the shared free helper so
            // a test asserts the exact `LeaveAlternateScreen` + mode-restore
            // stream (and the absence of a transcript mirror / `\x1b[3J`) against
            // a `Capture` sink. The shared-flag AND above is the idempotence
            // contract so the alt-screen is left exactly once even when a panic
            // hook or clean-exit path already left it.
            let _ = emit_terminal_emergency_teardown(backend, still_in_alt_screen);
            self.alt_screen_active = false;
            // Keep the crash-path flag in sync: the alternate screen (if any) has
            // now been left, so a panic hook / signal handler firing during or
            // after this Drop must not leave it a second time.
            signal_teardown::set_alt_screen_active(false);
        }
        let _ = terminal.show_cursor();
        // Restore the panic hook that was in place before `enter` installed the
        // emergency-teardown one. Done HERE (not only in `finish_fullscreen`) so it
        // runs on EVERY exit — clean, inline (`alt_screen_active == false`), or an
        // error-path `?` early-return that never reaches `finish_fullscreen` —
        // keeping the second-TUI / non-TUI contract intact. Idempotent via the
        // `HOOKS_INSTALLED` swap, so it is a no-op if a clean exit already restored.
        signal_teardown::restore_previous_panic_hook();
    }
}

#[cfg(test)]
#[path = "terminal_guard_tests.rs"]
mod tests;
