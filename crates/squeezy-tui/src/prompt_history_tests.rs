use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

fn temp_history_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("squeezy_prompt_history_{name}_{nonce}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir.join("prompt_history")
}

#[test]
fn in_memory_collapses_consecutive_duplicates() {
    let mut history = PromptHistory::in_memory(10);
    history.push("alpha".to_string());
    history.push("alpha".to_string());
    history.push("beta".to_string());
    history.push("alpha".to_string());

    let entries: Vec<_> = history.iter().collect();
    assert_eq!(entries, vec!["alpha", "beta", "alpha"]);
    assert_eq!(history.last(), Some("alpha"));
}

#[test]
fn in_memory_rejects_blank_and_whitespace_only_prompts() {
    let mut history = PromptHistory::in_memory(10);
    history.push(String::new());
    history.push("   ".to_string());
    history.push("\n\t".to_string());

    assert!(history.is_empty());
}

#[test]
fn in_memory_drops_oldest_when_capacity_reached() {
    let mut history = PromptHistory::in_memory(3);
    history.push("one".to_string());
    history.push("two".to_string());
    history.push("three".to_string());
    history.push("four".to_string());

    assert_eq!(history.len(), 3);
    let entries: Vec<_> = history.iter().collect();
    assert_eq!(entries, vec!["two", "three", "four"]);
}

#[test]
fn in_memory_get_returns_entries_by_index() {
    let mut history = PromptHistory::in_memory(10);
    history.push("alpha".to_string());
    history.push("beta".to_string());

    assert_eq!(history.get(0), Some("alpha"));
    assert_eq!(history.get(1), Some("beta"));
    assert_eq!(history.get(2), None);
}

#[test]
fn in_memory_never_touches_disk() {
    let path = temp_history_path("never_touches_disk");
    let mut history = PromptHistory::in_memory(10);
    history.push("alpha".to_string());

    assert!(history.persist_path().is_none());
    assert!(!path.exists(), "in-memory history should not create files");
}

#[test]
fn default_capacity_matches_constant() {
    let history = PromptHistory::in_memory(DEFAULT_PROMPT_HISTORY_CAPACITY);
    assert_eq!(history.capacity(), DEFAULT_PROMPT_HISTORY_CAPACITY);
}

#[test]
fn persistence_appends_each_push_to_disk() {
    let path = temp_history_path("appends_each_push");
    let mut history = PromptHistory::with_persistence(10, path.clone());
    history.push("first".to_string());
    history.push("second".to_string());

    let on_disk = fs::read_to_string(&path).expect("history file");
    assert_eq!(on_disk, "first\nsecond\n");
}

#[test]
fn persistence_round_trips_multiline_prompts() {
    let path = temp_history_path("multiline_round_trip");
    let original = "line one\nline two with \\ slash\rand carriage";
    {
        let mut history = PromptHistory::with_persistence(10, path.clone());
        history.push(original.to_string());
    }

    let reopened = PromptHistory::with_persistence(10, path.clone());
    let entries: Vec<_> = reopened.iter().collect();
    assert_eq!(entries, vec![original]);
}

#[test]
fn persistence_load_dedups_consecutive_duplicates_on_disk() {
    let path = temp_history_path("dedup_on_load");
    {
        let mut file = fs::File::create(&path).expect("create");
        writeln!(file, "alpha").unwrap();
        writeln!(file, "alpha").unwrap();
        writeln!(file, "beta").unwrap();
        writeln!(file, "beta").unwrap();
        writeln!(file, "alpha").unwrap();
    }

    let history = PromptHistory::with_persistence(10, path);
    let entries: Vec<_> = history.iter().collect();
    assert_eq!(entries, vec!["alpha", "beta", "alpha"]);
}

#[test]
fn persistence_rewrites_file_when_capacity_evicts_oldest() {
    let path = temp_history_path("rewrites_on_evict");
    let mut history = PromptHistory::with_persistence(3, path.clone());
    history.push("one".to_string());
    history.push("two".to_string());
    history.push("three".to_string());
    history.push("four".to_string());

    let on_disk = fs::read_to_string(&path).expect("history file");
    assert_eq!(on_disk, "two\nthree\nfour\n");
}

#[test]
fn persistence_load_truncates_to_capacity_keeping_most_recent() {
    let path = temp_history_path("truncate_to_capacity");
    {
        let mut file = fs::File::create(&path).expect("create");
        for entry in ["one", "two", "three", "four", "five"] {
            writeln!(file, "{entry}").unwrap();
        }
    }

    let history = PromptHistory::with_persistence(3, path);
    let entries: Vec<_> = history.iter().collect();
    assert_eq!(entries, vec!["three", "four", "five"]);
}

#[test]
fn persistence_missing_file_is_a_silent_empty_history() {
    let path = temp_history_path("missing_file");
    fs::remove_dir_all(path.parent().unwrap()).ok();
    let history = PromptHistory::with_persistence(10, path.clone());

    assert!(history.is_empty());
    assert!(!path.exists());
}

#[test]
fn persistence_creates_missing_parent_directory_on_first_push() {
    let path = temp_history_path("creates_parent_dir")
        .join("nested")
        .join("prompts");
    assert!(!path.exists());
    let mut history = PromptHistory::with_persistence(10, path.clone());
    history.push("alpha".to_string());

    let on_disk = fs::read_to_string(&path).expect("history file");
    assert_eq!(on_disk, "alpha\n");
}

#[cfg(unix)]
#[test]
fn persistence_file_is_user_only_readable() {
    use std::os::unix::fs::PermissionsExt;

    let assert_0o600 = |path: &PathBuf| {
        let mode = fs::metadata(path)
            .expect("history file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "history file must be user-only");
    };

    // Create-new path: first push appends to a fresh file.
    let path = temp_history_path("perms_create");
    {
        let mut history = PromptHistory::with_persistence(10, path.clone());
        history.push("first".to_string());
    }
    assert_0o600(&path);

    // Append-to-existing path: subsequent pushes append in place.
    {
        let mut history = PromptHistory::with_persistence(10, path.clone());
        history.push("second".to_string());
    }
    assert_0o600(&path);

    // Rewrite path: capacity eviction rewrites the whole file.
    let rewrite = temp_history_path("perms_rewrite");
    {
        let mut history = PromptHistory::with_persistence(2, rewrite.clone());
        history.push("one".to_string());
        history.push("two".to_string());
        history.push("three".to_string());
    }
    assert_0o600(&rewrite);

    // Pre-existing world-readable file is hardened on the next write.
    let preexisting = temp_history_path("perms_preexisting");
    {
        let mut file = fs::File::create(&preexisting).expect("create");
        writeln!(file, "old").unwrap();
    }
    fs::set_permissions(&preexisting, fs::Permissions::from_mode(0o644)).expect("chmod");
    {
        let mut history = PromptHistory::with_persistence(10, preexisting.clone());
        history.push("new".to_string());
    }
    assert_0o600(&preexisting);
}
