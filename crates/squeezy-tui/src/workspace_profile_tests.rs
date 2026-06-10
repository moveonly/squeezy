//! Unit tests for the pure Per-Workspace UI Profile model + persistence
//! (§12.7.4). These exercise the field navigation, the capture/value rules, and
//! the TOML round-trip / path resolution directly, with no terminal — the
//! overlay's keyboard/mouse/render integration through the real `render()` is
//! covered by the capture-sink suite in `lib_tests.rs`.

use std::sync::{Mutex, MutexGuard};

use super::*;

/// The profile-dir override env var is process-global; serialize the tests that
/// set it so they don't clobber each other under the test runner's threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Pin [`PROFILE_DIR_ENV`] to a fresh scratch dir for the duration of the guard,
/// restoring the prior value on drop so no test leaks the override into another.
struct ScopedProfileDir {
    _guard: MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
    dir: PathBuf,
}

impl ScopedProfileDir {
    fn new(name: &str) -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os(PROFILE_DIR_ENV);
        let dir = std::env::temp_dir().join(format!(
            "squeezy-ui-profile-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        // SAFETY: serialized by `ENV_LOCK`; no other thread reads the var while held.
        unsafe {
            std::env::set_var(PROFILE_DIR_ENV, &dir);
        }
        Self {
            _guard: guard,
            prior,
            dir,
        }
    }
}

impl Drop for ScopedProfileDir {
    fn drop(&mut self) {
        // SAFETY: serialized by `ENV_LOCK`; restoring the prior value on drop.
        unsafe {
            match &self.prior {
                Some(prev) => std::env::set_var(PROFILE_DIR_ENV, prev),
                None => std::env::remove_var(PROFILE_DIR_ENV),
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn fields_are_non_empty_and_each_has_a_label() {
    assert!(
        !FIELDS.is_empty(),
        "the overlay must list at least one field"
    );
    for field in FIELDS {
        assert!(!field.label().is_empty(), "every field needs a label");
    }
}

#[test]
fn a_fresh_state_focuses_the_first_field() {
    let state = WorkspaceProfileState::new("projects/x/ui_profile.toml".to_string());
    assert_eq!(state.field_index(), 0);
    assert_eq!(state.current_field(), FIELDS[0]);
    assert_eq!(state.source_label(), "projects/x/ui_profile.toml");
}

#[test]
fn focus_next_advances_and_clamps_at_the_bottom() {
    let mut state = WorkspaceProfileState::new(String::new());
    let mut moves = 0;
    while state.focus_next() {
        moves += 1;
        assert!(moves < 100, "focus_next must terminate");
    }
    assert_eq!(state.field_index(), FIELDS.len() - 1);
    // Already at the bottom: another move is a no-op.
    assert!(!state.focus_next());
    assert_eq!(state.field_index(), FIELDS.len() - 1);
}

#[test]
fn focus_prev_at_the_top_is_a_no_op() {
    let mut state = WorkspaceProfileState::new(String::new());
    assert!(!state.focus_prev());
    assert_eq!(state.field_index(), 0);
}

#[test]
fn focus_field_jumps_directly_and_ignores_out_of_range() {
    let mut state = WorkspaceProfileState::new(String::new());
    assert!(state.focus_field(FIELDS.len() - 1));
    assert_eq!(state.field_index(), FIELDS.len() - 1);
    // Re-focusing the current row is a no-op.
    assert!(!state.focus_field(FIELDS.len() - 1));
    // Out-of-range index is ignored, leaving the focus put.
    assert!(!state.focus_field(FIELDS.len()));
    assert_eq!(state.field_index(), FIELDS.len() - 1);
}

#[test]
fn an_empty_profile_reports_empty_and_dashes_every_value() {
    let profile = UiProfile::default();
    assert!(profile.is_empty());
    for field in FIELDS {
        assert_eq!(
            field.value_str(&profile),
            "\u{2014}",
            "an unremembered {} reads as a dash",
            field.label()
        );
    }
}

#[test]
fn a_captured_profile_is_not_empty_and_renders_each_value() {
    let profile = UiProfile::new(
        ToolOutputVerbosity::Verbose,
        TranscriptDefault::Expanded,
        true,
        "catppuccin".to_string(),
    );
    assert!(!profile.is_empty());
    assert_eq!(profile.version, PROFILE_SCHEMA_VERSION);
    assert_eq!(ProfileField::Density.value_str(&profile), "verbose");
    assert_eq!(
        ProfileField::TranscriptDetail.value_str(&profile),
        "expanded"
    );
    assert_eq!(ProfileField::Minimap.value_str(&profile), "shown");
    assert_eq!(ProfileField::Theme.value_str(&profile), "catppuccin");
}

#[test]
fn minimap_false_renders_as_hidden() {
    let profile = UiProfile {
        minimap: Some(false),
        ..UiProfile::default()
    };
    assert_eq!(ProfileField::Minimap.value_str(&profile), "hidden");
}

#[test]
fn profile_path_lands_under_the_overridden_dir_and_not_in_the_repo() {
    let _scope = ScopedProfileDir::new("path");
    let root = std::env::temp_dir().join("squeezy-workspace-path-sample");
    let path = profile_path(&root);
    // The file is the workspace-id-keyed ui_profile.toml under the override dir.
    assert!(
        path.ends_with("ui_profile.toml"),
        "profile is a ui_profile.toml file: {}",
        path.display()
    );
    assert!(
        !path.starts_with(&root),
        "profile must NOT live inside the repo root: {}",
        path.display()
    );
    // The parent directory is the workspace identity hash.
    let id = squeezy_core::repo_settings_id(&root);
    assert!(
        path.to_string_lossy().contains(&id),
        "profile path is keyed by the workspace id {id}: {}",
        path.display()
    );
}

#[test]
fn load_of_a_missing_profile_is_empty_not_an_error() {
    let _scope = ScopedProfileDir::new("missing");
    let root = std::env::temp_dir().join("squeezy-workspace-missing");
    let profile = load(&root);
    assert!(
        profile.is_empty(),
        "a never-saved workspace remembers nothing"
    );
}

#[test]
fn save_then_load_round_trips_every_field() {
    let _scope = ScopedProfileDir::new("roundtrip");
    let root = std::env::temp_dir().join("squeezy-workspace-roundtrip");
    let original = UiProfile::new(
        ToolOutputVerbosity::Normal,
        TranscriptDefault::Compact,
        false,
        "high-contrast".to_string(),
    );
    let written = save(&root, &original).expect("profile saves");
    assert!(written.exists(), "the profile file was written to disk");
    // The file lives outside the repo root.
    assert!(!written.starts_with(&root));

    let loaded = load(&root);
    assert_eq!(loaded, original, "every field survived the TOML round-trip");
}

#[test]
fn save_does_not_serialize_unremembered_fields() {
    let _scope = ScopedProfileDir::new("partial");
    let root = std::env::temp_dir().join("squeezy-workspace-partial");
    let partial = UiProfile {
        version: PROFILE_SCHEMA_VERSION,
        theme: Some("fun".to_string()),
        ..UiProfile::default()
    };
    let written = save(&root, &partial).expect("partial profile saves");
    let text = std::fs::read_to_string(&written).expect("read back");
    assert!(
        text.contains("theme"),
        "the set field is serialized: {text}"
    );
    assert!(
        !text.contains("density"),
        "an unset field is skipped, not written as null: {text}"
    );
    // It still round-trips: the absent fields load as None.
    let loaded = load(&root);
    assert_eq!(loaded.theme.as_deref(), Some("fun"));
    assert!(loaded.density.is_none());
}

#[test]
fn a_future_schema_version_is_ignored_on_load() {
    let _scope = ScopedProfileDir::new("future");
    let root = std::env::temp_dir().join("squeezy-workspace-future");
    let path = profile_path(&root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Hand-write a file claiming a newer schema than this build understands.
    std::fs::write(
        &path,
        format!(
            "version = {}\ntheme = \"fun\"\n",
            PROFILE_SCHEMA_VERSION + 9
        ),
    )
    .unwrap();
    let loaded = load(&root);
    assert!(
        loaded.is_empty(),
        "a newer-schema profile is ignored rather than misread"
    );
}

#[test]
fn a_malformed_profile_loads_as_empty_rather_than_panicking() {
    let _scope = ScopedProfileDir::new("malformed");
    let root = std::env::temp_dir().join("squeezy-workspace-malformed");
    let path = profile_path(&root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "this is = not valid toml [[[").unwrap();
    let loaded = load(&root);
    assert!(loaded.is_empty(), "a corrupt profile never blocks launch");
}

#[test]
fn clear_removes_the_profile_and_is_idempotent() {
    let _scope = ScopedProfileDir::new("clear");
    let root = std::env::temp_dir().join("squeezy-workspace-clear");
    let profile = UiProfile::new(
        ToolOutputVerbosity::Compact,
        TranscriptDefault::Compact,
        true,
        "default".to_string(),
    );
    let written = save(&root, &profile).expect("save");
    assert!(written.exists());
    clear(&root).expect("clear removes the file");
    assert!(!written.exists(), "the profile file is gone after clear");
    // A second clear on the now-missing file is a harmless success.
    clear(&root).expect("a double clear is idempotent");
}

#[test]
fn source_label_omits_the_absolute_parent_to_avoid_leaking_local_paths() {
    let root = std::env::temp_dir().join("squeezy-workspace-label");
    let label = source_label(&root);
    assert!(
        label.starts_with("projects/") && label.ends_with("ui_profile.toml"),
        "the label is a repo-relative source hint: {label}"
    );
    assert!(
        !label.contains(std::path::MAIN_SEPARATOR) || !label.contains(".."),
        "the label must not expose a traversable absolute path: {label}"
    );
}
