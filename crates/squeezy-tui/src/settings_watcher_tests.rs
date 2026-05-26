use super::*;
use std::fs;
use std::thread;
use std::time::Duration;

#[test]
fn poll_reports_no_change_when_paths_are_missing() {
    let watcher = SettingsWatcher {
        files: vec![WatchedFile {
            path: PathBuf::from("/nonexistent/squeezy-settings-watcher-test.toml"),
            mtime: None,
        }],
    };
    let mut watcher = watcher;
    assert!(!watcher.poll(), "missing file should not be flagged");
}

#[test]
fn poll_reports_change_when_file_appears_and_again_when_it_moves() {
    let tmp = std::env::temp_dir().join(format!(
        "squeezy-settings-watcher-{}.toml",
        std::process::id()
    ));
    let _ = fs::remove_file(&tmp);
    let mut watcher = SettingsWatcher {
        files: vec![WatchedFile {
            path: tmp.clone(),
            mtime: None,
        }],
    };
    assert!(!watcher.poll(), "no file → no change yet");

    fs::write(&tmp, "[model]\nprovider = \"openai\"\n").expect("write tmp settings");
    assert!(watcher.poll(), "file appearing must fire one change event");
    assert!(!watcher.poll(), "no further changes until mtime moves");

    // mtime resolution on some filesystems is 1s; sleep just enough.
    thread::sleep(Duration::from_millis(1100));
    fs::write(&tmp, "[model]\nprovider = \"anthropic\"\n").expect("rewrite tmp");
    assert!(watcher.poll(), "rewrite must fire another change event");

    let _ = fs::remove_file(&tmp);
    assert!(watcher.poll(), "deletion is also a change");
}
