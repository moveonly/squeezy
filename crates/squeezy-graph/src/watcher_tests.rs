use std::{
    fs,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use super::{ChangeBatch, FileWatcher, WatcherConfig};

fn temp_dir(label: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "squeezy-graph-watcher-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

#[test]
fn change_batch_is_empty_until_paths_are_recorded() {
    let batch = ChangeBatch::default();
    assert!(batch.is_empty());
    let batch = ChangeBatch {
        modified: vec!["a.rs".into()],
        removed: Vec::new(),
    };
    assert!(!batch.is_empty());
}

#[test]
fn watcher_emits_change_batch_after_debounce_window() {
    let root = temp_dir("emits-batch");
    let captured: Arc<Mutex<Vec<ChangeBatch>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    let config = WatcherConfig {
        src_dirs: vec![root.clone()],
        // Short debounce so the test finishes quickly. The production
        // default is 10s.
        debounce_ms: 250,
    };
    let watcher = FileWatcher::start_polling_for_tests(config, move |batch: ChangeBatch| {
        captured_clone.lock().unwrap().push(batch);
    })
    .expect("watcher start");

    let file = root.join("touched.txt");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut attempt = 0usize;
    while Instant::now() < deadline && captured.lock().unwrap().is_empty() {
        fs::write(&file, format!("hello {attempt}\n")).expect("write file");
        // Wait long enough for the debounce window to close plus a tick.
        thread::sleep(Duration::from_millis(500));
        attempt += 1;
    }

    let batches = captured.lock().unwrap().clone();
    drop(watcher);
    assert!(!batches.is_empty(), "expected at least one batch");
    let has_path = batches.iter().any(|batch| {
        batch
            .modified
            .iter()
            .any(|path| path.ends_with("touched.txt"))
    });
    assert!(
        has_path,
        "expected debounced batch to contain the touched file, got {batches:?}",
    );
}
