use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

/// A fresh, unique temp directory for a `run_handoff` test so concurrent test
/// threads never share the same temp file. Created eagerly so the handoff can
/// write into it.
fn unique_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("squeezy_edit_test_{name}_{nonce}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// An `env_get` that maps a fixed table of var → value, returning `None` for
/// anything else. Lets the resolver tests pin precedence without touching real
/// process env (racy across the test thread pool).
fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
    move |key| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| OsString::from(*v))
    }
}

/// Serializes every test that drives `run_handoff`: on Unix the handoff resets
/// the process-global `SIGTSTP` disposition to `SIG_DFL` for the editor spawn and
/// restores it afterward (deep-review #5), so two `run_handoff` calls racing in
/// the parallel test pool would clobber each other's saved disposition. Holding
/// this lock for the whole of each such test keeps the save/restore exclusive.
static SIGTSTP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire [`SIGTSTP_TEST_LOCK`], recovering a poisoned lock so one failing test
/// does not cascade into every other `run_handoff` test.
fn sigtstp_test_guard() -> std::sync::MutexGuard<'static, ()> {
    SIGTSTP_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── resolve_editor ────────────────────────────────────────────────────────

#[test]
fn resolve_editor_prefers_visual_over_editor() {
    let command = resolve_editor(env_from(&[("VISUAL", "nvim"), ("EDITOR", "vi")]))
        .expect("an editor resolves");
    assert_eq!(command.program, "nvim");
    assert!(command.args.is_empty());
}

#[test]
fn resolve_editor_falls_back_to_editor_when_visual_unset() {
    let command = resolve_editor(env_from(&[("EDITOR", "vim")])).expect("falls back to EDITOR");
    assert_eq!(command.program, "vim");
}

#[test]
fn resolve_editor_parses_args_after_program() {
    let command = resolve_editor(env_from(&[("EDITOR", "code --wait")])).expect("resolves");
    assert_eq!(command.program, "code");
    assert_eq!(command.args, vec!["--wait".to_string()]);
    assert_eq!(command.display(), "code --wait");
}

#[test]
fn resolve_editor_none_when_unset() {
    // The safe fallback: no $VISUAL / $EDITOR ⇒ the caller degrades to a hint.
    assert!(resolve_editor(env_from(&[])).is_none());
}

#[test]
fn resolve_editor_treats_blank_value_as_unset() {
    // A stray `EDITOR=` (or whitespace-only) must not resolve to an empty
    // program — that would spawn nothing and hang.
    assert!(resolve_editor(env_from(&[("VISUAL", "   "), ("EDITOR", "")])).is_none());
}

#[test]
fn resolve_editor_skips_blank_visual_and_uses_editor() {
    // Blank $VISUAL is treated as unset, so resolution continues to $EDITOR.
    let command = resolve_editor(env_from(&[("VISUAL", ""), ("EDITOR", "vim")])).expect("resolves");
    assert_eq!(command.program, "vim");
}

// ── temp_file_name ────────────────────────────────────────────────────────

#[test]
fn temp_file_name_is_self_describing_and_extensioned() {
    let name = temp_file_name(EditorTarget::Composer, 4242, 7);
    assert!(name.starts_with("squeezy_edit_composer_4242_7"), "{name}");
    assert!(name.ends_with(".md"), "{name}");
}

#[test]
fn temp_file_name_distinct_per_seq() {
    let a = temp_file_name(EditorTarget::Composer, 1, 1);
    let b = temp_file_name(EditorTarget::Composer, 1, 2);
    assert!(a != b, "a different seq yields a different leaf");
}

// ── classify_result ───────────────────────────────────────────────────────

#[test]
fn classify_result_ignores_a_trailing_newline_added_by_the_editor() {
    // Most editors append a final newline on save; that alone is not a change.
    assert_eq!(
        classify_result("hello", "hello\n"),
        HandoffOutcome::Unchanged
    );
}

#[test]
fn classify_result_reports_a_real_change() {
    match classify_result("hello", "hello world\n") {
        HandoffOutcome::Changed(text) => assert_eq!(text, "hello world"),
        other => panic!("expected a change, got {other:?}"),
    }
}

#[test]
fn classify_result_unchanged_on_identical_text() {
    assert_eq!(classify_result("same", "same"), HandoffOutcome::Unchanged);
}

#[test]
fn classify_result_interior_newlines_preserved() {
    // Only the single trailing newline is trimmed; interior structure stays.
    match classify_result("a", "a\n\nb\n") {
        HandoffOutcome::Changed(text) => assert_eq!(text, "a\n\nb"),
        other => panic!("expected a change, got {other:?}"),
    }
}

// ── run_handoff (fake editor) ─────────────────────────────────────────────

#[test]
fn run_handoff_modify_reads_back_the_edit_and_cleans_up() {
    let _lock = sigtstp_test_guard();
    let dir = unique_dir("modify");
    let command = EditorCommand {
        program: "fake".to_string(),
        args: Vec::new(),
    };
    let outcome = run_handoff(
        &command,
        EditorTarget::Composer,
        "before",
        &dir,
        1234,
        1,
        |_command, path| {
            // The fake editor sees the seeded text, then saves a new buffer.
            assert_eq!(std::fs::read_to_string(path).unwrap(), "before");
            std::fs::write(path, b"after\n")
        },
    )
    .expect("handoff runs");
    match outcome {
        HandoffOutcome::Changed(text) => assert_eq!(text, "after"),
        other => panic!("expected a change, got {other:?}"),
    }
    // The temp file is always cleaned up.
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "temp file must be removed after the handoff"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_handoff_unchanged_when_editor_saves_nothing_new() {
    let _lock = sigtstp_test_guard();
    let dir = unique_dir("unchanged");
    let command = EditorCommand {
        program: "fake".to_string(),
        args: Vec::new(),
    };
    let outcome = run_handoff(
        &command,
        EditorTarget::Composer,
        "keep me",
        &dir,
        1,
        2,
        // Editor quits without writing — the seeded text is read back as-is.
        |_command, _path| Ok(()),
    )
    .expect("handoff runs");
    assert_eq!(outcome, HandoffOutcome::Unchanged);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_handoff_propagates_editor_failure_and_still_cleans_up() {
    let _lock = sigtstp_test_guard();
    let dir = unique_dir("fail");
    let command = EditorCommand {
        program: "fake".to_string(),
        args: Vec::new(),
    };
    let result = run_handoff(
        &command,
        EditorTarget::Composer,
        "buffer",
        &dir,
        1,
        3,
        |_command, _path| Err(std::io::Error::other("spawn failed")),
    );
    assert!(result.is_err(), "an editor failure bubbles up");
    // Even on failure the temp file is removed so a failed handoff leaves no
    // litter behind.
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "temp file must be removed even when the editor fails"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_handoff_slow_editor_completes_after_the_closure_returns() {
    let _lock = sigtstp_test_guard();
    // "Sleep" coverage: the runner blocks (here, a short sleep) before saving;
    // run_handoff only reads back once the closure returns, so the edit lands.
    let dir = unique_dir("slow");
    let command = EditorCommand {
        program: "fake".to_string(),
        args: Vec::new(),
    };
    let outcome = run_handoff(
        &command,
        EditorTarget::Composer,
        "x",
        &dir,
        1,
        4,
        |_command, path| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            std::fs::write(path, b"y\n")
        },
    )
    .expect("handoff runs");
    assert_eq!(outcome, HandoffOutcome::Changed("y".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// deep-review #54: when read-back fails (the editor saved non-UTF-8 bytes),
/// `run_handoff` must PRESERVE the user's edits on disk and surface the path —
/// not delete the temp file and destroy the session. The old code removed the
/// file before the `?` on the read result propagated.
#[test]
fn run_handoff_preserves_the_temp_file_when_read_back_fails() {
    let _lock = sigtstp_test_guard();
    let dir = unique_dir("nonutf8");
    let pid = 1234u32;
    let seq = 11u64;
    let expected_path = dir.join(temp_file_name(EditorTarget::Composer, pid, seq));
    let command = EditorCommand {
        program: "fake".to_string(),
        args: Vec::new(),
    };
    let result = run_handoff(
        &command,
        EditorTarget::Composer,
        "before",
        &dir,
        pid,
        seq,
        |_command, path| {
            // The "editor" saves bytes that are not valid UTF-8.
            std::fs::write(path, [0xff, 0xfe, 0x00])
        },
    );

    // (a) The handoff errors rather than silently dropping the edit.
    let error = result.expect_err("a non-UTF-8 save surfaces as an error");
    // (c) The error message carries the path so the user can recover.
    assert!(
        error
            .to_string()
            .contains(&expected_path.display().to_string()),
        "the error must surface the preserved path, got: {error}"
    );
    // (b) The temp file STILL EXISTS and holds the saved bytes.
    let bytes = std::fs::read(&expected_path).expect("the edits are preserved on disk");
    assert_eq!(
        bytes,
        [0xff, 0xfe, 0x00],
        "the preserved file holds the editor's saved bytes"
    );

    let _ = std::fs::remove_file(&expected_path);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Security (deep-review #23/#53, CWE-59 symlink + CWE-377 mode): `run_handoff`
/// must create its temp file exclusively. If a symlink is pre-planted at the
/// predictable temp path pointing at a sentinel outside `dir`, the old
/// `fs::write` would follow it and overwrite the sentinel; `create_new(true)`
/// refuses the pre-existing path with an error and never touches the sentinel.
#[cfg(unix)]
#[test]
fn run_handoff_refuses_to_follow_a_preplanted_symlink() {
    let _lock = sigtstp_test_guard();
    let dir = unique_dir("symlink");
    let pid = 1234u32;
    let seq = 99u64;
    let target_path = dir.join(temp_file_name(EditorTarget::Composer, pid, seq));

    // A sentinel file OUTSIDE dir that must remain intact.
    let sentinel = unique_dir("symlink_sentinel").join("sentinel.txt");
    std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    std::fs::write(&sentinel, b"SENTINEL").unwrap();

    // Plant a symlink at the predictable temp path pointing at the sentinel.
    std::os::unix::fs::symlink(&sentinel, &target_path).expect("plant symlink");

    let command = EditorCommand {
        program: "fake".to_string(),
        args: Vec::new(),
    };
    let editor_ran = std::cell::Cell::new(false);
    let result = run_handoff(
        &command,
        EditorTarget::Composer,
        "payload that must NOT reach the sentinel",
        &dir,
        pid,
        seq,
        |_command, _path| {
            editor_ran.set(true);
            Ok(())
        },
    );

    assert!(
        result.is_err(),
        "create_new must refuse the pre-planted path"
    );
    assert!(
        !editor_ran.get(),
        "the editor must never spawn on a refused write"
    );
    assert_eq!(
        std::fs::read(&sentinel).expect("sentinel still readable"),
        b"SENTINEL",
        "the symlink target must not be followed/overwritten"
    );

    let _ = std::fs::remove_file(&target_path);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(sentinel.parent().unwrap());
}

/// Security (deep-review #23/#53, CWE-377): the temp file `run_handoff` creates
/// must be private (0o600), not the umask-derived 0o644 the old `fs::write`
/// produced, so the composer buffer is not readable by other local users.
#[cfg(unix)]
#[test]
fn run_handoff_creates_a_0600_temp_file() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    let _lock = sigtstp_test_guard();
    let dir = unique_dir("mode");
    let command = EditorCommand {
        program: "fake".to_string(),
        args: Vec::new(),
    };
    // Capture the file's mode WHILE it still exists (run_handoff deletes it on
    // return), from inside the editor closure.
    let observed: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let observed_in = Arc::clone(&observed);
    let outcome = run_handoff(
        &command,
        EditorTarget::Composer,
        "buffer",
        &dir,
        1234,
        7,
        move |_command, path| {
            let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
            *observed_in.lock().unwrap() = Some(mode);
            Ok(())
        },
    )
    .expect("handoff runs");
    assert_eq!(outcome, HandoffOutcome::Unchanged);
    let mode = observed.lock().unwrap().expect("mode observed");
    assert_eq!(mode, 0o600, "temp file must be 0o600, got {mode:o}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── EditorHandoffReview ───────────────────────────────────────────────────

#[test]
fn review_summary_counts_lines_and_starts_on_accept() {
    let review = EditorHandoffReview::new(EditorTarget::Composer, "a\nb", "a\nb\nc".to_string());
    assert_eq!(review.original_lines, 2);
    assert_eq!(review.edited_lines, 3);
    assert_eq!(review.summary(), "composer · 2 → 3 lines");
    // Accept is the default selection so Enter re-imports the edit.
    assert_eq!(review.selected_action(), ReviewAction::Accept);
    assert_eq!(review.selected_index(), 0);
}

#[test]
fn review_cursor_moves_and_saturates() {
    let mut review = EditorHandoffReview::new(EditorTarget::Composer, "x", "y".to_string());
    review.move_up(); // already at the top — no wrap
    assert_eq!(review.selected_action(), ReviewAction::Accept);
    review.move_down();
    assert_eq!(review.selected_action(), ReviewAction::Reopen);
    review.move_down();
    assert_eq!(review.selected_action(), ReviewAction::Discard);
    review.move_down(); // already at the bottom — no wrap
    assert_eq!(review.selected_action(), ReviewAction::Discard);
}

#[test]
fn review_select_clamps_out_of_range_clicks() {
    let mut review = EditorHandoffReview::new(EditorTarget::Composer, "x", "y".to_string());
    review.select(2);
    assert_eq!(review.selected_action(), ReviewAction::Discard);
    // An out-of-range click index is ignored rather than panicking.
    review.select(99);
    assert_eq!(review.selected_action(), ReviewAction::Discard);
}

#[test]
fn review_summary_singular_line_label() {
    let review = EditorHandoffReview::new(EditorTarget::Composer, "", "only".to_string());
    // Empty original counts as 0 lines; a one-line edit uses the singular label.
    assert_eq!(review.original_lines, 0);
    assert_eq!(review.summary(), "composer · 0 → 1 line");
}

#[test]
fn review_action_all_is_the_rendered_order() {
    assert_eq!(
        ReviewAction::ALL,
        [
            ReviewAction::Accept,
            ReviewAction::Reopen,
            ReviewAction::Discard
        ]
    );
    assert_eq!(ReviewAction::Accept.label(), "Accept");
    assert_eq!(ReviewAction::Reopen.label(), "Reopen");
    assert_eq!(ReviewAction::Discard.label(), "Discard");
}

// ── SIGTSTP handling around the editor spawn (deep-review #5) ──────────────

/// A no-op `SIGTSTP` handler used to stand in for tokio's notify-only listener:
/// a NON-default disposition, so the test can prove `run_handoff` reset it to
/// `SIG_DFL` for the editor spawn.
#[cfg(unix)]
extern "C" fn noop_sigtstp_handler(_signo: libc::c_int) {}

/// (deep-review #5) A Ctrl+Z INSIDE the external editor must NOT hang the
/// session. The fix runs the editor spawn with the `SIGTSTP` disposition reset to
/// `SIG_DFL`, so the editor job is parked/continued by the shell's job control
/// instead of squeezy's notify-only tokio listener swallowing the stop while
/// `Command::status()` (a `waitpid` without `WUNTRACED`) blocks forever.
///
/// We install a non-default `SIGTSTP` handler (standing in for tokio's listener),
/// then drive `run_handoff` with a fake editor that records the live `SIGTSTP`
/// disposition during the spawn. It must observe `SIG_DFL` (the wrap reset it),
/// and the non-default handler must be RESTORED after the handoff returns. With
/// the pre-fix code (no disposition change) the observed handler is the installed
/// non-default one and the `SIG_DFL` assertion fails.
#[cfg(unix)]
#[test]
fn run_handoff_resets_sigtstp_to_default_during_the_editor_spawn() {
    let _lock = sigtstp_test_guard();

    // Install a non-default SIGTSTP handler, saving whatever was there so we can
    // put it back at the end (do not perturb the rest of the test process).
    let mut original: libc::sigaction = unsafe { std::mem::zeroed() };
    let mut installed: libc::sigaction = unsafe { std::mem::zeroed() };
    // Cast the fn item through a thin pointer before `usize` (a direct
    // `fn as usize` trips the fn-to-numeric-cast lint).
    let installed_disposition = noop_sigtstp_handler as *const () as usize;
    installed.sa_sigaction = installed_disposition;
    let install_rc = unsafe { libc::sigaction(libc::SIGTSTP, &installed, &mut original) };
    assert_eq!(install_rc, 0, "test setup: installing the stand-in handler");

    let dir = unique_dir("sigtstp");
    let command = EditorCommand {
        program: "fake".to_string(),
        args: Vec::new(),
    };

    // The fake editor records the live SIGTSTP disposition the moment it runs.
    let observed_during_spawn = std::cell::Cell::new(usize::MAX);
    let outcome = run_handoff(
        &command,
        EditorTarget::Composer,
        "before",
        &dir,
        4242,
        1,
        |_command, _path| {
            let mut observed: libc::sigaction = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::sigaction(libc::SIGTSTP, std::ptr::null(), &mut observed) };
            assert_eq!(rc, 0, "observing the SIGTSTP disposition inside the spawn");
            observed_during_spawn.set(observed.sa_sigaction);
            Ok(())
        },
    )
    .expect("handoff runs");
    assert_eq!(outcome, HandoffOutcome::Unchanged);

    // Capture the disposition AFTER the handoff returned (to prove the restore),
    // then put the process's original SIGTSTP disposition back regardless.
    let mut after: libc::sigaction = unsafe { std::mem::zeroed() };
    let after_rc = unsafe { libc::sigaction(libc::SIGTSTP, &original, &mut after) };
    assert_eq!(after_rc, 0, "test teardown: restoring the original handler");

    // During the spawn the disposition must have been SIG_DFL (the wrap reset it).
    assert_eq!(
        observed_during_spawn.get(),
        libc::SIG_DFL,
        "the editor spawn must run with SIGTSTP reset to SIG_DFL so a Ctrl+Z in \
         the editor is handled by job control, not squeezy's notify-only listener",
    );
    // After the handoff, the non-default (tokio-stand-in) handler must be back, so
    // the next Ctrl+Z at the squeezy prompt is routed to cooperative suspend.
    assert_eq!(
        after.sa_sigaction, installed_disposition,
        "the handoff must restore the prior SIGTSTP handler after the editor exits",
    );

    let _ = std::fs::remove_dir_all(&dir);
}
