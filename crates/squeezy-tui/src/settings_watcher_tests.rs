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
            digest: None,
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
            digest: None,
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

#[test]
fn identical_resave_moves_mtime_but_stays_quiet() {
    let tmp = std::env::temp_dir().join(format!(
        "squeezy-settings-watcher-noop-{}.toml",
        std::process::id()
    ));
    let body = "[model]\nprovider = \"openai\"\n";
    fs::write(&tmp, body).expect("seed settings");
    let mut watcher = SettingsWatcher {
        files: vec![WatchedFile {
            path: tmp.clone(),
            mtime: current_mtime(&tmp),
            digest: file_digest(&tmp),
        }],
    };

    // A sync agent / idempotent re-save rewrites the same bytes: the mtime moves
    // but the content is unchanged, so the watcher must NOT report a change (no
    // reload, no "settings reloaded from disk" status to stack up while idle).
    thread::sleep(Duration::from_millis(1100));
    fs::write(&tmp, body).expect("identical re-save");
    assert!(
        !watcher.poll(),
        "an identical re-save (mtime moved, bytes unchanged) must stay quiet"
    );

    // A genuine content edit still fires exactly once.
    thread::sleep(Duration::from_millis(1100));
    fs::write(&tmp, "[model]\nprovider = \"anthropic\"\n").expect("real edit");
    assert!(watcher.poll(), "a real content change must fire");
    assert!(!watcher.poll(), "and not re-fire on the next poll");

    let _ = fs::remove_file(&tmp);
}
