use std::path::PathBuf;

use squeezy_core::AppConfig;

use crate::{CompactionCheckpoint, SqueezyStore, sessions::ResumeItem};

fn temp_root(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "squeezy-store-tests-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).expect("create temp root");
    path
}

fn open_store(label: &str) -> (PathBuf, SqueezyStore) {
    let root = temp_root(label);
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SqueezyStore::open(&config.workspace_root, None).expect("open store");
    (root, store)
}

fn sample_checkpoint(replacement_id: &str, created: u128) -> CompactionCheckpoint {
    CompactionCheckpoint {
        replacement_id: replacement_id.to_string(),
        session_id: "sess-1".to_string(),
        generation: 4,
        items: vec![
            ResumeItem::UserText {
                text: "first user turn".to_string(),
            },
            ResumeItem::AssistantText {
                text: "first assistant reply".to_string(),
            },
        ],
        created_unix_millis: created,
    }
}

#[test]
fn compaction_checkpoint_round_trip() {
    let (_root, store) = open_store("ckpt-roundtrip");
    let checkpoint = sample_checkpoint("ckpt-1", 1_000);
    store
        .put_compaction_checkpoint(&checkpoint)
        .expect("put checkpoint");
    let loaded = store
        .get_compaction_checkpoint("ckpt-1")
        .expect("get checkpoint")
        .expect("checkpoint present");
    assert_eq!(loaded, checkpoint);
}

#[test]
fn compaction_checkpoint_missing_id_returns_none() {
    let (_root, store) = open_store("ckpt-missing");
    let loaded = store
        .get_compaction_checkpoint("does-not-exist")
        .expect("get checkpoint");
    assert!(loaded.is_none());
}

#[test]
fn compaction_checkpoint_prune_drops_old_only() {
    let (_root, store) = open_store("ckpt-prune");
    let old = sample_checkpoint("ckpt-old", 100);
    let fresh = sample_checkpoint("ckpt-fresh", 1_000);
    store.put_compaction_checkpoint(&old).expect("put old");
    store.put_compaction_checkpoint(&fresh).expect("put fresh");
    let removed = store
        .prune_compaction_checkpoints(500)
        .expect("prune older than 500");
    assert_eq!(removed, 1);
    assert!(
        store
            .get_compaction_checkpoint("ckpt-old")
            .expect("get old")
            .is_none(),
        "old checkpoint should be pruned",
    );
    assert!(
        store
            .get_compaction_checkpoint("ckpt-fresh")
            .expect("get fresh")
            .is_some(),
        "fresh checkpoint should remain",
    );
}
