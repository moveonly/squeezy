//! Crash-safety plumbing for the fullscreen TUI (Phase 9): a panic hook and
//! OS-signal handlers that restore the terminal to a sane state when the normal
//! `TerminalGuard::Drop` path will NOT run.
//!
//! `TerminalGuard::Drop` already restores terminal modes on a normal unwind, but
//! it does NOT help in two crash cases:
//!
//!   * A `panic!` prints its message and backtrace BEFORE the stack unwinds far
//!     enough to drop the guard, so the panic text lands on a still-fullscreen,
//!     still-raw terminal (garbled, no cursor). The panic hook here runs the
//!     emergency teardown FIRST, then chains to the previous hook so the
//!     backtrace prints to a now-sane terminal.
//!   * A `SIGTERM` / `SIGHUP` terminates the process without unwinding at all
//!     (Drop never runs), leaving the terminal in raw mode / the alternate
//!     screen. The signal handlers here run the same emergency teardown and then
//!     exit.
//!
//! Everything is ADDITIVE and reuses the existing single-sourced byte emitter
//! [`crate::emit_terminal_emergency_teardown`] plus `disable_raw_mode`, so the
//! crash paths emit exactly the same restore sequence the clean lifecycle does.
//! Every emit is best-effort (`let _ = …`): a failing escape on a dying or
//! legacy-Windows terminal must never panic inside a panic hook or abort a
//! signal handler mid-restore.

use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::terminal::disable_raw_mode;

/// Whether the fullscreen guard believes the alternate screen is currently
/// active. Mirrors `TerminalGuard::alt_screen_active` so the panic hook and the
/// signal handlers — which do NOT own the guard — can pass the right value into
/// [`crate::emit_terminal_emergency_teardown`] and avoid leaving an alternate
/// screen that was never entered (or was already left by a clean exit).
///
/// `true` once a fullscreen guard enters the alternate screen; cleared by the
/// clean-exit / Drop paths so a second crash-path teardown is a no-op for the
/// alt-screen leave (the mode restores are themselves idempotent).
static ALT_SCREEN_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set by the `SIGTSTP` (Ctrl+Z) handler to ask the main loop to suspend at the
/// next safe point. The loop owns the actual suspend/resume because it holds the
/// `TerminalGuard` writer and the app model: a signal handler cannot leave the
/// alternate screen cleanly or redraw from model state. The handler only flips
/// this flag; [`take_suspend_request`] read-and-clears it once per loop turn.
///
/// We deliberately do NOT tear the terminal down inside the async signal task,
/// because that races the renderer's own writes and the model is mid-mutation.
/// Cooperative suspend at the top of the loop keeps every byte ordered and lets
/// us re-enter + force a full redraw deterministically on `SIGCONT`.
#[cfg(unix)]
static SUSPEND_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Guards single installation of the panic hook across the process (the OS-signal
/// handlers have their own [`SIGNAL_HANDLERS_INSTALLED`] guard). A second
/// fullscreen TUI in the same process reuses the already installed hook (it reads
/// the shared [`ALT_SCREEN_ACTIVE`] flag, so it stays correct for whichever guard
/// is live). Also gates [`restore_previous_panic_hook`], so the restore is
/// idempotent: only the swap-from-`true` caller actually puts the old hook back.
static HOOKS_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Guards single installation of the OS-signal handlers across the process,
/// mirroring [`HOOKS_INSTALLED`]. The CLI BackToSetup / model-switch loop
/// re-invokes `run_inner_with_terminal` in the same process, which would
/// otherwise spawn a fresh SIGTERM/SIGHUP/SIGTSTP listener task on every
/// re-entry — orphaned tasks accumulating for the process lifetime. Installing
/// exactly once keeps those listeners correct for every later in-process session
/// (they read the shared [`ALT_SCREEN_ACTIVE`] / [`SUSPEND_REQUESTED`] statics);
/// deliberately never reset on clean exit.
#[cfg(unix)]
static SIGNAL_HANDLERS_INSTALLED: AtomicBool = AtomicBool::new(false);

/// The boxed panic hook type `std::panic::set_hook` accepts / `take_hook`
/// returns. Aliased so [`PREVIOUS_PANIC_HOOK`]'s `Mutex<Option<…>>` stays
/// readable (and clippy's `type_complexity` lint is satisfied).
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Slot for the panic hook that was in place before we installed ours, so the
/// emergency-teardown hook can chain to it (preserving the default backtrace) and
/// so [`restore_previous_panic_hook`] can put it back on a clean exit.
static PREVIOUS_PANIC_HOOK: Mutex<Option<PanicHook>> = Mutex::new(None);

/// Record whether the fullscreen alternate screen is active, so the crash-path
/// teardown leaves it exactly once. Called by `TerminalGuard::enter` (sets
/// `true`) and by the clean-exit / Drop paths (set `false`).
pub(crate) fn set_alt_screen_active(active: bool) {
    ALT_SCREEN_ACTIVE.store(active, Ordering::SeqCst);
}

/// Run the idempotent emergency teardown against the real `stdout`, then disable
/// raw mode. This is the crash-path equivalent of `TerminalGuard::Drop`'s body,
/// but it owns its own `stdout` handle because the panic hook and signal handlers
/// do not have access to the guard's wrapped writer.
///
/// Idempotent and best-effort:
///   * The shared [`ALT_SCREEN_ACTIVE`] flag is cleared first, so a second call
///     skips the `LeaveAlternateScreen` (the mode restores re-emit harmlessly).
///   * Every emit is swallowed (`let _ = …`) so a write to a closed/redirected
///     stdout (SIGHUP, broken SSH pipe) or an `Unsupported` return on the legacy
///     Windows console can never panic or abort the restore midway.
pub(crate) fn run_emergency_teardown() {
    // Read-and-clear so a re-entrant or second invocation leaves the alternate
    // screen exactly once; the remaining mode restores are themselves idempotent.
    let alt_screen_active = ALT_SCREEN_ACTIVE.swap(false, Ordering::SeqCst);
    let mut out = io::stdout();
    let _ = crate::emit_terminal_emergency_teardown(&mut out, alt_screen_active);
    let _ = out.flush();
    // Raw mode is a crossterm mode call, not a byte; disable it last (best-effort)
    // so any preceding CRLF-free restore writes are unaffected.
    let _ = disable_raw_mode();
}

/// Install the panic hook that restores the terminal BEFORE the default panic
/// message prints. Idempotent across the process: only the first fullscreen guard
/// installs it; later guards reuse it (it reads the shared alt-screen flag).
///
/// The hook runs [`run_emergency_teardown`] (leave alt-screen, disable mouse
/// modes, show cursor via the restore sequence, disable bracketed paste / focus
/// change, restore keyboard-enhancement flags, disable raw mode) and then chains
/// to the previously installed hook so the panic message + backtrace still print
/// — now to a sane terminal. Restore the original hook on a clean exit via
/// [`restore_previous_panic_hook`] so a second TUI in the same process is
/// unaffected.
pub(crate) fn install_panic_hook() {
    if HOOKS_INSTALLED.swap(true, Ordering::SeqCst) {
        // Already installed by an earlier guard in this process. The existing
        // hook reads the shared `ALT_SCREEN_ACTIVE` flag, so it stays correct.
        return;
    }
    let previous = std::panic::take_hook();
    if let Ok(mut slot) = PREVIOUS_PANIC_HOOK.lock() {
        *slot = Some(previous);
    }
    std::panic::set_hook(Box::new(|info| {
        // Restore the terminal FIRST so the panic text lands on a sane screen.
        run_emergency_teardown();
        // Then chain to the previous hook (the default printer / a test hook) so
        // the backtrace still prints.
        if let Ok(slot) = PREVIOUS_PANIC_HOOK.lock()
            && let Some(previous) = slot.as_ref()
        {
            previous(info);
        }
    }));
}

/// Restore the panic hook that was in place before [`install_panic_hook`], called
/// on a clean exit so a subsequent TUI (or non-TUI code) in the same process is
/// not affected by our emergency-teardown hook. A no-op if no hook was installed.
pub(crate) fn restore_previous_panic_hook() {
    if !HOOKS_INSTALLED.swap(false, Ordering::SeqCst) {
        return;
    }
    let previous = PREVIOUS_PANIC_HOOK
        .lock()
        .ok()
        .and_then(|mut slot| slot.take());
    if let Some(previous) = previous {
        std::panic::set_hook(previous);
    }
}

/// Install the OS-signal handlers that run the emergency teardown when the
/// process is asked to terminate without unwinding (so `TerminalGuard::Drop`
/// would otherwise never run).
///
/// On Unix this spawns a task per signal:
///
///   * `SIGTERM` / `SIGHUP`: on first delivery, run [`run_emergency_teardown`]
///     and exit the process — a killed squeezy must not leave the terminal in
///     raw mode / the alternate screen. `SIGKILL` is unhandleable by design and
///     is intentionally not covered.
///   * `SIGTSTP` (Ctrl+Z): set [`SUSPEND_REQUESTED`] so the main loop suspends
///     cooperatively at its next turn (clean terminal restore → re-raise the
///     stop with the default disposition → re-enter + full redraw on resume).
///     The handler does NOT touch the terminal itself — that would race the
///     renderer and run on a half-mutated model.
///
/// On non-Unix platforms terminal recovery on forced termination is left to the
/// console host and `Drop`; there is no reachable equivalent of `SIGTERM`/
/// `SIGHUP`/`SIGTSTP` to hook here, so this is a no-op (the panic hook still
/// applies). Job-control suspend (Ctrl+Z) is a Unix concept and has no Windows
/// analogue.
#[cfg(unix)]
pub(crate) fn install_signal_handlers() {
    use tokio::signal::unix::{SignalKind, signal};

    // Install exactly once per process, mirroring the panic hook's
    // `HOOKS_INSTALLED` guard. The CLI BackToSetup / model-switch loop re-invokes
    // `run_inner_with_terminal` in the same process; without this each re-entry
    // would spawn fresh listener tasks that leak for the process lifetime. The
    // existing listeners read the shared statics, so they stay correct for a later
    // in-process session — install once, never reset on clean exit.
    if SIGNAL_HANDLERS_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    // Best-effort: if the runtime cannot register a listener (e.g. another
    // library already took an incompatible handler), we simply fall back to the
    // panic hook + Drop. A failed registration must never abort startup.
    for (kind, signo) in [
        (SignalKind::terminate(), libc::SIGTERM),
        (SignalKind::hangup(), libc::SIGHUP),
    ] {
        let Ok(mut stream) = signal(kind) else {
            continue;
        };
        tokio::spawn(async move {
            if stream.recv().await.is_some() {
                run_emergency_teardown();
                // Re-raising the default disposition would require resetting the
                // handler; exiting with the conventional `128 + signo` status
                // (143 for SIGTERM, 129 for SIGHUP) is sufficient, keeps the
                // restore deterministic, and reports the originating signal to the
                // parent shell instead of masquerading as SIGINT (130).
                std::process::exit(128 + signo);
            }
        });
    }

    // SIGTSTP (Ctrl+Z): cooperative suspend. The handler only flips the request
    // flag; the main loop performs the clean restore + re-raise + redraw so the
    // terminal bytes stay ordered and the resume repaints from model state. The
    // task loops (not a one-shot) because the user can Ctrl+Z repeatedly across
    // the session — each delivery re-arms the request.
    // `SignalKind` has no named `tstp()`; build it from the raw signal number.
    if let Ok(mut stream) = signal(SignalKind::from_raw(libc::SIGTSTP)) {
        tokio::spawn(async move {
            while stream.recv().await.is_some() {
                SUSPEND_REQUESTED.store(true, Ordering::SeqCst);
            }
        });
    }
}

/// Read-and-clear the pending SIGTSTP suspend request. The main loop calls this
/// once per turn; a `true` return means it should run the cooperative
/// suspend/resume cycle (clean terminal restore, re-raise SIGTSTP with the
/// default disposition, then re-enter + force a full redraw). Always `false` on
/// non-Unix (no job-control suspend).
//
// The only non-test caller (`lib.rs`) is itself `#[cfg(unix)]`-gated — it must
// be, because `suspend_and_resume` is `#[cfg(unix)]`-only — so on non-Unix the
// lib never references this function and it is reached only by the
// `#[cfg(not(unix))]` test. We keep the single function (not a `cfg`-split) and
// silence the lib-target dead-code lint off Unix, where the `false` arm exists
// purely so that off-Unix test can run.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn take_suspend_request() -> bool {
    #[cfg(unix)]
    {
        SUSPEND_REQUESTED.swap(false, Ordering::SeqCst)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Test-only: arm the suspend request as if a `SIGTSTP` had been delivered, so a
/// headless test can drive [`take_suspend_request`] without raising a real signal
/// (which would actually stop the test process). Unix-only — the flag does not
/// exist on other platforms.
#[cfg(all(test, unix))]
pub(crate) fn request_suspend_for_test() {
    SUSPEND_REQUESTED.store(true, Ordering::SeqCst);
}

/// Re-raise `SIGTSTP` with the DEFAULT disposition so the process actually stops
/// (job control), after the terminal has already been restored to a sane state.
///
/// tokio's `unix::signal(SIGTSTP)` listener (installed in
/// [`install_signal_handlers`]) replaced the kernel's default "stop" disposition
/// with a notify-only handler — so a plain `raise(SIGTSTP)` would just re-set our
/// cooperative-suspend flag instead of stopping the process. To actually stop:
///
///   1. Save the current `SIGTSTP` `sigaction` (tokio's handler).
///   2. Install `SIG_DFL` so the next delivery stops the process group.
///   3. `raise(SIGTSTP)` — the kernel stops us HERE. Execution resumes on the
///      next line once the shell sends `SIGCONT` (`fg`).
///   4. Restore tokio's saved `sigaction`, so the NEXT Ctrl+Z is again delivered
///      to tokio's listener and re-arms cooperative suspend.
///
/// Best-effort: any `libc` failure is swallowed; the worst case is that Ctrl+Z
/// does not stop the process (the terminal is already restored either way),
/// which is strictly better than corrupting the terminal. Unix-only.
#[cfg(unix)]
pub(crate) fn reraise_sigtstp_default() {
    // Safety: `sigaction`/`raise` are async-signal-safe libc calls. We save
    // tokio's current handler, set SIG_DFL so the kernel stops the process group
    // (job control), raise the stop, and on resume restore tokio's handler so
    // later Ctrl+Z presses are seen by the cooperative-suspend listener again.
    unsafe {
        let mut saved: libc::sigaction = std::mem::zeroed();
        let mut dfl: libc::sigaction = std::mem::zeroed();
        dfl.sa_sigaction = libc::SIG_DFL;
        // Capture tokio's handler and install the default "stop" disposition.
        if libc::sigaction(libc::SIGTSTP, &dfl, &mut saved) != 0 {
            // Could not swap the disposition; fall back to a plain raise so at
            // least the cooperative flag re-arms (no terminal corruption).
            libc::raise(libc::SIGTSTP);
            return;
        }
        // Stop here. Resumes after SIGCONT (e.g. `fg`).
        libc::raise(libc::SIGTSTP);
        // Restore tokio's notify-only handler for the next Ctrl+Z.
        libc::sigaction(libc::SIGTSTP, &saved, std::ptr::null_mut());
    }
}

/// Non-Unix stub: no reachable `SIGTERM`/`SIGHUP` equivalent to hook. The panic
/// hook (cross-platform) and `Drop` remain the recovery paths. Kept as a named
/// no-op so the call site in the event loop is unconditional and `cfg`-free.
#[cfg(not(unix))]
pub(crate) fn install_signal_handlers() {}

/// Test-only: read whether the panic hook is currently installed (the
/// `HOOKS_INSTALLED` guard). Lets a test prove that a guard's `Drop` restores the
/// previous hook on a non-alt / error-path exit — i.e. the install/restore are
/// symmetric, not leaked behind `finish_fullscreen`'s alt-screen early-return.
#[cfg(test)]
pub(crate) fn panic_hook_installed_for_test() -> bool {
    HOOKS_INSTALLED.load(Ordering::SeqCst)
}

#[cfg(test)]
#[path = "signal_teardown_tests.rs"]
mod tests;
