//! Headless tests for the Phase 9 crash-safety plumbing.
//!
//! The panic hook and signal handlers write to the real `stdout`, which a
//! headless test cannot capture. Instead these tests pin the load-bearing
//! contracts that ARE observable without a TTY:
//!
//!   * The emergency-teardown byte sequence the panic hook / signal handlers
//!     reuse ([`crate::emit_terminal_emergency_teardown`]) emits the disable
//!     sequence in the right order, and never the scrollback purge.
//!   * That emitter is best-effort: a writer that fails mid-stream does not
//!     panic (so it can never panic inside the panic hook).
//!   * The emitter is idempotent: running it twice (e.g. a clean exit then a
//!     late signal, or two crash paths racing) is safe and leaves the alternate
//!     screen exactly once when driven through the shared `alt_screen_active`
//!     flag.
//!   * The shared alt-screen flag gates the `LeaveAlternateScreen`.
//!
//! `super::*` is the `signal_teardown` module; lib internals are reached via
//! `crate::`.

use std::io::{self, Write};

use crate::{DISABLE_MOUSE_MODES, RESET_KEYBOARD_ENHANCEMENT_FLAGS};

/// A writer that fails every write after `ok_writes` successful ones, to prove
/// the teardown emitters are best-effort: a dying/closed stdout (SIGHUP, broken
/// SSH pipe) must never make the panic hook or a signal handler panic.
struct FailingWriter {
    ok_writes: usize,
    writes_done: usize,
}

impl FailingWriter {
    fn new(ok_writes: usize) -> Self {
        Self {
            ok_writes,
            writes_done: 0,
        }
    }
}

impl Write for FailingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.writes_done >= self.ok_writes {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer closed"));
        }
        self.writes_done += 1;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.writes_done >= self.ok_writes {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer closed"));
        }
        Ok(())
    }
}

/// The teardown sequence the panic hook / signal handlers reuse leaves the
/// alternate screen and restores terminal modes (disable mouse modes, bracketed
/// paste, focus reporting, keyboard-enhancement flags) — in the right order:
/// the mode restores land AFTER `LeaveAlternateScreen` so they apply to the
/// restored normal buffer. This is the exact sequence the crash paths emit.
#[test]
fn panic_hook_teardown_emits_disable_sequence_in_order() {
    let mut bytes = Vec::new();
    crate::emit_terminal_emergency_teardown(&mut bytes, /* alt_screen_active = */ true)
        .expect("emergency teardown emits");
    let ansi = String::from_utf8(bytes).expect("ansi");

    // Leaves the alternate screen.
    let leave_pos = ansi
        .find("\x1b[?1049l")
        .expect("teardown must leave the alternate screen");
    // Disables mouse modes, bracketed paste, focus reporting; resets keyboard
    // enhancement flags. (Focus reporting off is `\x1b[?1004l`; bracketed paste
    // off is `\x1b[?2004l`.)
    assert!(
        ansi.contains(DISABLE_MOUSE_MODES),
        "teardown must disable mouse modes"
    );
    assert!(
        ansi.contains("\x1b[?2004l"),
        "teardown must disable bracketed paste"
    );
    assert!(
        ansi.contains("\x1b[?1004l"),
        "teardown must disable focus reporting"
    );
    let reset_pos = ansi
        .find(RESET_KEYBOARD_ENHANCEMENT_FLAGS)
        .expect("teardown must reset keyboard enhancement flags");
    assert!(
        reset_pos > leave_pos,
        "mode restores must come AFTER LeaveAlternateScreen so they land on the \
         restored normal buffer"
    );

    // Crash-path teardown must NEVER purge the user's pre-launch scrollback.
    assert!(
        !ansi.contains("\x1b[3J"),
        "teardown must never purge scrollback (\\x1b[3J)"
    );

    // H1: the crash path runs ONLY these bytes (the panic hook / SIGTERM / SIGHUP
    // handlers never call the separate `show_cursor()` the clean paths use), so
    // the emitter itself must re-show the hardware cursor `enter` hid — otherwise
    // a panic/kill leaves the cursor invisible until `reset` / `tput cnorm`. The
    // show must land AFTER LeaveAlternateScreen so it applies to the normal buffer.
    let show_pos = ansi
        .find("\x1b[?25h")
        .expect("crash-path teardown must re-show the hardware cursor");
    assert!(
        show_pos > leave_pos,
        "cursor show must come AFTER LeaveAlternateScreen so it lands on the \
         restored normal buffer"
    );
}

/// Best-effort: a writer that fails partway through the teardown must not panic.
/// `emit_terminal_emergency_teardown` returns an `Err` rather than unwinding, and
/// the panic hook / signal handler swallow it with `let _ = …`. Here we drive the
/// failing writer directly and assert no panic (the `Result` may be `Err`).
#[test]
fn panic_hook_teardown_is_best_effort_on_failing_writer() {
    // Fails on the very first write — the worst case for a panic hook.
    let mut writer = FailingWriter::new(0);
    let result = crate::emit_terminal_emergency_teardown(&mut writer, /* alt = */ true);
    // It must not panic; returning an error is fine (the caller discards it).
    assert!(
        result.is_err(),
        "a writer failing on the first write should surface an Err, not panic"
    );

    // Also exercise a mid-stream failure (some writes succeed, then the pipe
    // breaks). Still no panic.
    let mut writer = FailingWriter::new(3);
    let _ = crate::emit_terminal_emergency_teardown(&mut writer, /* alt = */ true);
}

/// Idempotence: running the teardown twice is safe, and when the alt-screen flag
/// is cleared between calls (as the shared flag does), the second call does NOT
/// re-leave the alternate screen — it only re-emits the idempotent mode restores.
/// This models a clean exit (which clears the flag) followed by a late signal.
#[test]
fn teardown_is_idempotent_and_leaves_alt_screen_once() {
    // First teardown with the alternate screen active: leaves it.
    let mut first = Vec::new();
    crate::emit_terminal_emergency_teardown(&mut first, /* alt = */ true).expect("first teardown");
    let first = String::from_utf8(first).expect("ansi");
    assert!(
        first.contains("\x1b[?1049l"),
        "first teardown (alt active) must leave the alternate screen"
    );

    // Second teardown with the flag now cleared (as the clean exit / a prior
    // teardown would have done): must NOT leave the alternate screen again, but
    // still restores modes harmlessly.
    let mut second = Vec::new();
    crate::emit_terminal_emergency_teardown(&mut second, /* alt = */ false)
        .expect("second teardown");
    let second = String::from_utf8(second).expect("ansi");
    assert!(
        !second.contains("\x1b[?1049l"),
        "second teardown (alt already left) must NOT re-leave the alternate screen"
    );
    assert!(
        second.contains(DISABLE_MOUSE_MODES),
        "second teardown still re-emits the idempotent mode restores"
    );
}

/// The shared alt-screen flag round-trips and read-and-clears via the public
/// module API, which is the contract the panic hook / signal handlers rely on to
/// leave the alternate screen exactly once.
#[test]
fn set_alt_screen_active_flag_round_trips() {
    // Save and restore the process-global flag so this test does not perturb a
    // concurrently running fullscreen guard's expectation in the same process.
    let saved = super::ALT_SCREEN_ACTIVE.load(std::sync::atomic::Ordering::SeqCst);

    super::set_alt_screen_active(true);
    assert!(super::ALT_SCREEN_ACTIVE.load(std::sync::atomic::Ordering::SeqCst));
    // `run_emergency_teardown` read-and-clears, so a second crash-path call sees
    // `false`. Emulate just the swap (the real fn also writes to stdout).
    let was = super::ALT_SCREEN_ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst);
    assert!(was, "swap returns the previous (active) value");
    assert!(
        !super::ALT_SCREEN_ACTIVE.load(std::sync::atomic::Ordering::SeqCst),
        "flag is cleared after the read-and-clear swap"
    );

    super::set_alt_screen_active(saved);
}

/// Unix-only: registering the SIGTERM/SIGHUP/SIGTSTP handlers inside a tokio
/// runtime must not panic and must not block (the listeners are spawned, no
/// signal is sent).
#[cfg(unix)]
#[tokio::test]
async fn install_signal_handlers_registers_without_blocking() {
    // Just installing must be infallible and non-blocking; we never deliver a
    // signal, so the spawned listeners stay parked and the test returns promptly.
    super::install_signal_handlers();
}

/// Unix-only (deep-review #115): the emergency-teardown signal set must include
/// SIGINT, so an external `kill -INT <pid>` runs the terminal restore and exits
/// 130 (`128 + SIGINT`) — matching the SIGTERM (143) / SIGHUP (129) handling
/// rather than leaving SIGINT on its default disposition (which would terminate
/// the process with the alternate screen still up). Pins the set + exit codes
/// without raising real signals.
#[cfg(unix)]
#[test]
fn emergency_teardown_set_covers_sigint_with_exit_130() {
    let signals = super::emergency_teardown_signals();
    let signos: Vec<libc::c_int> = signals.iter().map(|(_, signo)| *signo).collect();

    assert!(
        signos.contains(&libc::SIGINT),
        "SIGINT must trigger emergency teardown so `kill -INT` restores the \
         terminal; set = {signos:?}"
    );
    // The other polite-kill signals stay covered (no regression).
    assert!(signos.contains(&libc::SIGTERM), "SIGTERM stays covered");
    assert!(signos.contains(&libc::SIGHUP), "SIGHUP stays covered");

    // The handler exits with `128 + signo`; SIGINT maps to the conventional 130.
    const {
        assert!(
            128 + libc::SIGINT == 130,
            "an external SIGINT teardown must exit 130"
        );
    }
}

/// Unix-only: the SIGTSTP suspend request is a read-and-clear flag. The handler
/// arms it (here via the test setter, since raising a real SIGTSTP would stop the
/// test process), the loop drains it exactly once, and a second drain sees
/// nothing — so suspend runs once per Ctrl+Z, never on a stale flag.
#[cfg(unix)]
#[test]
fn suspend_request_is_read_and_cleared_once() {
    // Clear any state a concurrent test might have left, then arm and drain.
    let _ = super::take_suspend_request();
    super::request_suspend_for_test();
    assert!(
        super::take_suspend_request(),
        "an armed SIGTSTP request must be observed by the loop exactly once"
    );
    assert!(
        !super::take_suspend_request(),
        "a drained suspend request must not fire again (no stale re-suspend)"
    );
}

/// On non-Unix there is no job-control suspend, so the request drain is always
/// `false` — the loop's call site is `cfg(unix)`-gated and never suspends here.
#[cfg(not(unix))]
#[test]
fn suspend_request_is_always_false_off_unix() {
    assert!(
        !super::take_suspend_request(),
        "no SIGTSTP suspend exists off Unix; the drain must always be false"
    );
}

/// Serializes every test that mutates the process-global panic hook, so the
/// parallel test pool cannot interleave an install/restore round-trip with a
/// concurrent `TerminalGuard::Drop` (which now also restores the hook).
static PANIC_HOOK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// H2 — panic-hook restore symmetry. `TerminalGuard::enter` installs the
/// emergency-teardown panic hook UNCONDITIONALLY, but the restore used to live
/// only inside `finish_fullscreen`, AFTER its `if !alt_screen_active` early
/// return. So an inline (non-alt) guard or any error-path `?` exit that drops the
/// guard WITHOUT a clean `finish_fullscreen` leaked the hook (`HOOKS_INSTALLED`
/// stuck `true`), breaking the second-TUI / non-TUI contract. The fix moves the
/// restore into `Drop`, which runs on EVERY exit. This pins that: with a guard
/// whose `alt_screen_active` is `false` (the inline / already-finished state),
/// dropping it still restores the previous hook.
#[test]
fn drop_restores_panic_hook_on_non_alt_exit() {
    let _guard = PANIC_HOOK_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Start from a known-clean hook state: if a prior test left it installed,
    // restore first so this test owns the round-trip.
    super::restore_previous_panic_hook();
    assert!(
        !super::panic_hook_installed_for_test(),
        "precondition: no panic hook installed before this test"
    );

    // Install our emergency-teardown hook (records the previous one).
    super::install_panic_hook();
    assert!(
        super::panic_hook_installed_for_test(),
        "install_panic_hook must mark the hook installed"
    );

    // Build a fullscreen capture guard and force the non-alt / already-finished
    // state — exactly what an inline guard or an error-path exit looks like when
    // `finish_fullscreen` was never reached (or short-circuited its early return).
    let (mut guard, _sink) = crate::TerminalGuard::for_capture_test(80, 24);
    guard.alt_screen_active = false;
    crate::signal_teardown::set_alt_screen_active(false);

    // Dropping the guard must restore the previous hook even though no clean
    // `finish_fullscreen` ran on this (non-alt) guard.
    drop(guard);
    assert!(
        !super::panic_hook_installed_for_test(),
        "Drop must restore the previous panic hook on a non-alt / error-path exit"
    );
}

/// (deep-review #116) Pins the real panic-hook-then-`Drop` interleaving that no
/// existing test exercised. A `panic!` runs the panic hook's
/// `run_emergency_teardown` FIRST — which read-and-clears the shared
/// `ALT_SCREEN_ACTIVE` static (the `swap(false)`) and leaves the alternate screen
/// once on the real stdout — and THEN the stack unwinds into `TerminalGuard::Drop`,
/// whose guard-LOCAL `alt_screen_active` is still `true`. Before the
/// deep-review #27 fix, `Drop` left the screen off its local field and so emitted
/// a SECOND `LeaveAlternateScreen`. Now `Drop` consults the shared read-and-clear,
/// so the captured `Drop` stream contains NO `\x1b[?1049l` (the hook already left
/// it). This test drives the real `run_emergency_teardown`, not a hand-fed flag.
#[test]
fn drop_after_emergency_teardown_does_not_re_leave_alt_screen() {
    // Serialize against the process-global alt-screen flag: a concurrent test
    // building/dropping a capture guard would otherwise perturb the swap.
    let _flag_guard = super::ALT_SCREEN_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // The capture guard publishes the shared flag (as the real `enter` does); make
    // the precondition explicit to match the production interleaving.
    super::set_alt_screen_active(true);
    let (guard, sink) = crate::TerminalGuard::for_capture_test(80, 24);

    // Panic-hook equivalent: read-and-clears the shared static and leaves the
    // alternate screen exactly once — on the REAL stdout, not the capture sink.
    super::run_emergency_teardown();

    // Now the guard drops. With the #27 fix it sees the shared flag already
    // cleared and must NOT re-leave the alternate screen into the capture sink.
    drop(guard);
    let bytes = sink.lock().expect("capture sink lock").clone();
    let captured = String::from_utf8(bytes).expect("captured ANSI is valid utf8");
    let leave_count = captured.matches("\x1b[?1049l").count();
    assert_eq!(
        leave_count, 0,
        "after the panic hook already left the alternate screen (shared flag \
         cleared), Drop must not re-leave it — expected 0 LeaveAlternateScreen in \
         the Drop stream, got {leave_count}: {captured:?}"
    );
}
