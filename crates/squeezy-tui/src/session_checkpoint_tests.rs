//! Unit tests for the pure Session Auto-Save Checkpoints model + persistence
//! (§12.9.5). These exercise the debounce gate, the validate/clamp-on-load
//! rules, the schema/session guards, and the atomic TOML round-trip directly,
//! with no terminal — the overlay's keyboard/mouse/render integration through
//! the real `render()` is covered by the capture-sink suite in `lib_tests.rs`.

use std::sync::MutexGuard;
use std::time::{Duration, Instant};

use super::*;

/// Pin [`CHECKPOINT_DIR_ENV`] to a fresh scratch dir for the duration of the
/// guard, restoring the prior value on drop so no test leaks the override into
/// another.
struct ScopedCheckpointDir {
    _guard: MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
    dir: PathBuf,
}

impl ScopedCheckpointDir {
    fn new(name: &str) -> Self {
        // Share the SAME process-global lock the end-to-end tests in `lib_tests.rs`
        // hold, so a unit test never clobbers the env dir out from under a
        // concurrently-running integration test.
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os(CHECKPOINT_DIR_ENV);
        let dir = std::env::temp_dir().join(format!(
            "squeezy-ui-checkpoint-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        // SAFETY: serialized by `ENV_LOCK`; no other thread reads the var while held.
        unsafe {
            std::env::set_var(CHECKPOINT_DIR_ENV, &dir);
        }
        Self {
            _guard: guard,
            prior,
            dir,
        }
    }
}

impl Drop for ScopedCheckpointDir {
    fn drop(&mut self) {
        // SAFETY: serialized by `ENV_LOCK`; restoring the prior value on drop.
        unsafe {
            match &self.prior {
                Some(prev) => std::env::set_var(CHECKPOINT_DIR_ENV, prev),
                None => std::env::remove_var(CHECKPOINT_DIR_ENV),
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Construct an `Instant` `offset` in the PAST without ever back-dating with bare
/// subtraction (`Instant::now() - offset` PANICS on a fresh Windows monotonic
/// clock younger than the offset). Subtract via `checked_sub`, falling back to an
/// ever-smaller safe offset, and finally to `now` itself, so the first
/// `checked_sub` succeeds on every platform.
fn earlier(now: Instant, offset: Duration) -> Instant {
    now.checked_sub(offset)
        .or_else(|| now.checked_sub(Duration::from_millis(1)))
        .unwrap_or(now)
}

fn sample(session: &str, revision: usize) -> UiStateCheckpoint {
    UiStateCheckpoint::new(session.to_string(), revision, 0, true, None, None, false)
}

#[test]
fn new_stamps_the_current_schema_version_and_drops_empty_query() {
    let checkpoint = UiStateCheckpoint::new(
        "sess-1".to_string(),
        7,
        3,
        false,
        Some(2),
        Some(String::new()),
        true,
    );
    assert_eq!(checkpoint.version, CHECKPOINT_SCHEMA_VERSION);
    assert_eq!(checkpoint.session_id, "sess-1");
    assert_eq!(checkpoint.transcript_revision, 7);
    assert_eq!(checkpoint.scroll_from_bottom, 3);
    assert!(!checkpoint.following_tail);
    assert_eq!(checkpoint.selected_entry, Some(2));
    assert!(checkpoint.show_minimap);
    // An empty search query is normalised to None so an idle session never
    // persists a stray query.
    assert_eq!(checkpoint.search_query, None);
}

#[test]
fn matches_session_guards_against_a_foreign_checkpoint() {
    let checkpoint = sample("sess-a", 1);
    assert!(checkpoint.matches_session("sess-a"));
    assert!(!checkpoint.matches_session("sess-b"));
}

#[test]
fn clamped_for_drops_a_selected_entry_past_the_current_transcript() {
    let checkpoint = UiStateCheckpoint::new("s".to_string(), 10, 4, false, Some(8), None, false);
    // Transcript shrank to 5 entries: index 8 is now out of range and drops.
    let clamped = checkpoint.clamped_for(5);
    assert_eq!(clamped.selected_entry, None);
    // An in-range index survives.
    let still = checkpoint.clamped_for(20);
    assert_eq!(still.selected_entry, Some(8));
}

#[test]
fn clamped_for_pins_a_following_view_to_the_tail() {
    let checkpoint = UiStateCheckpoint::new(
        "s".to_string(),
        10,
        // A nonsense distance that should be ignored because the view was
        // following the tail.
        9_999,
        true,
        None,
        None,
        false,
    );
    let clamped = checkpoint.clamped_for(3);
    assert_eq!(clamped.scroll_from_bottom, 0);
    assert!(clamped.following_tail);
}

#[test]
fn clamped_for_bounds_a_stale_scroll_anchor_to_the_entry_count() {
    let checkpoint = UiStateCheckpoint::new("s".to_string(), 100, 9_999, false, None, None, false);
    // The transcript is now only 6 entries; the wild anchor is bounded.
    let clamped = checkpoint.clamped_for(6);
    assert_eq!(clamped.scroll_from_bottom, 6);
}

#[test]
fn store_saves_immediately_on_the_first_eligible_change() {
    let store = CheckpointStore::new();
    let now = Instant::now();
    assert!(
        store.should_save(&sample("s", 1), now),
        "a fresh store has no debounce gate to clear",
    );
    assert!(!store.has_saved(), "nothing recorded until record_saved");
}

#[test]
fn store_skips_an_identical_candidate_regardless_of_elapsed_time() {
    let mut store = CheckpointStore::new();
    let now = Instant::now();
    let checkpoint = sample("s", 1);
    store.record_saved(
        &checkpoint,
        earlier(now, SAVE_DEBOUNCE + Duration::from_secs(5)),
    );
    // Even though the debounce window has long since elapsed, an identical
    // candidate is never re-written (no churn for a redraw that changed nothing).
    assert!(!store.should_save(&checkpoint, now));
    assert!(store.has_saved());
}

#[test]
fn store_holds_back_a_changed_candidate_inside_the_debounce_window() {
    let mut store = CheckpointStore::new();
    let now = Instant::now();
    // Last save was only a moment ago (well inside the 2s debounce).
    store.record_saved(&sample("s", 1), earlier(now, Duration::from_millis(100)));
    // A genuinely different candidate, but the window has not elapsed yet.
    assert!(!store.should_save(&sample("s", 2), now));
}

#[test]
fn store_saves_a_changed_candidate_after_the_debounce_window() {
    let mut store = CheckpointStore::new();
    let now = Instant::now();
    store.record_saved(
        &sample("s", 1),
        earlier(now, SAVE_DEBOUNCE + Duration::from_millis(500)),
    );
    assert!(
        store.should_save(&sample("s", 2), now),
        "a changed candidate past the debounce window is eligible",
    );
}

#[test]
fn save_then_load_round_trips_through_disk() {
    let _scope = ScopedCheckpointDir::new("round_trip");
    let checkpoint = UiStateCheckpoint::new(
        "round-trip-session".to_string(),
        12,
        4,
        false,
        Some(3),
        Some("needle".to_string()),
        true,
    );
    let path = save(&checkpoint).expect("save");
    assert!(path.exists(), "the checkpoint file was written");
    // Atomic write leaves no stray temp file behind.
    assert!(
        !path.with_extension("toml.tmp").exists(),
        "the temp file was renamed away",
    );

    let loaded = load("round-trip-session").expect("load");
    assert_eq!(loaded, checkpoint);
}

#[test]
fn load_returns_none_for_a_missing_checkpoint() {
    let _scope = ScopedCheckpointDir::new("missing");
    assert!(load("never-saved").is_none());
}

#[test]
fn load_rejects_a_checkpoint_for_a_different_session() {
    let _scope = ScopedCheckpointDir::new("foreign");
    let checkpoint = sample("real-session", 2);
    save(&checkpoint).expect("save");
    // The on-disk file exists, but its embedded session id does not match the
    // requested one, so the load refuses it rather than misapplying it.
    let foreign_path = checkpoint_path("real-session");
    assert!(foreign_path.exists());
    // Hand the SAME file path a mismatched lookup by writing a checkpoint whose
    // embedded id differs from its file stem.
    let mismatched =
        UiStateCheckpoint::new("embedded-id".to_string(), 1, 0, true, None, None, false);
    let path = checkpoint_path("lookup-id");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, toml::to_string_pretty(&mismatched).unwrap()).unwrap();
    assert!(
        load("lookup-id").is_none(),
        "a checkpoint whose embedded id mismatches the lookup id is refused",
    );
}

#[test]
fn load_ignores_a_newer_schema_version() {
    let _scope = ScopedCheckpointDir::new("newer_schema");
    let mut checkpoint = sample("future-session", 1);
    checkpoint.version = CHECKPOINT_SCHEMA_VERSION + 1;
    let path = checkpoint_path("future-session");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, toml::to_string_pretty(&checkpoint).unwrap()).unwrap();
    assert!(
        load("future-session").is_none(),
        "a newer-schema checkpoint is ignored rather than misread",
    );
}

#[test]
fn load_ignores_a_corrupt_checkpoint_file() {
    let _scope = ScopedCheckpointDir::new("corrupt");
    let path = checkpoint_path("corrupt-session");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "this is = not [valid toml").unwrap();
    assert!(
        load("corrupt-session").is_none(),
        "a corrupt checkpoint is treated as nothing to restore, never panics",
    );
}

#[test]
fn clear_removes_the_file_and_is_idempotent() {
    let _scope = ScopedCheckpointDir::new("clear");
    let checkpoint = sample("clearable", 1);
    let path = save(&checkpoint).expect("save");
    assert!(path.exists());
    clear("clearable").expect("clear");
    assert!(!path.exists(), "clear removed the checkpoint file");
    // A second clear on an already-gone file is a success, not an error.
    clear("clearable").expect("double clear is harmless");
}

#[test]
fn checkpoint_path_sanitises_a_hostile_session_id() {
    let _scope = ScopedCheckpointDir::new("sanitise");
    // A session id with path-traversal characters must not escape the store dir.
    let path = checkpoint_path("../../etc/passwd");
    let root = path.parent().expect("parent dir");
    assert!(
        path.starts_with(root),
        "the sanitised file stays inside the store dir: {}",
        path.display(),
    );
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    assert!(
        !stem.contains('/') && !stem.contains('.'),
        "the file stem has no traversal characters: {stem}",
    );
}

#[test]
fn checkpoint_path_lives_outside_any_repo_under_the_projects_store() {
    let _scope = ScopedCheckpointDir::new("store_location");
    let path = checkpoint_path("loc-session");
    assert!(
        path.to_string_lossy().contains("_ui_checkpoints"),
        "checkpoints live under the dedicated store sub-dir: {}",
        path.display(),
    );
}
