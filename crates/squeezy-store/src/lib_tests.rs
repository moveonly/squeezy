use std::path::{Path, PathBuf};

use redb::{Database, TableDefinition};
use serde_json::json;
use squeezy_core::AppConfig;
use squeezy_core::FileId;

use crate::{
    CompactionCheckpoint, GRAPH_FILE_NAME, GraphStore, GraphWriteBatch, STATE_FILE_NAME,
    SqueezyStore, graph_path, sessions::ResumeItem, state_path,
};

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

fn open_graph_store(label: &str) -> (PathBuf, GraphStore) {
    let root = temp_root(label);
    let store = GraphStore::open(&root, None).expect("open graph store");
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

/// Stamp `version` into the `state.redb` `meta` table so re-opening hits the
/// schema-mismatch reset path.
fn write_schema_version(path: &Path, version: u64) {
    const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
    let database = Database::create(path).expect("create database");
    let write = database.begin_write().expect("begin write");
    {
        let mut table = write.open_table(META).expect("open meta");
        let value = serde_json::to_vec(&version).expect("encode version");
        table
            .insert("schema_version", value.as_slice())
            .expect("insert version");
    }
    write.commit().expect("commit");
}

#[test]
fn schema_mismatch_resets_with_backup_path() {
    let root = temp_root("schema-mismatch-warns");
    let state = root.join(".squeezy").join("cache").join("state.redb");
    std::fs::create_dir_all(state.parent().unwrap()).expect("create cache dir");
    write_schema_version(&state, 4);

    let store = SqueezyStore::open(&root, None).expect("open store");

    let backup_name = std::fs::read_dir(state.parent().unwrap())
        .expect("read cache")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.contains("schema-4"))
        .expect("old schema database should be backed up");

    assert!(
        backup_name.ends_with(".redb.bak"),
        "backup path should keep a redb backup suffix: {backup_name}"
    );
    assert!(
        store.path().ends_with(STATE_FILE_NAME),
        "state store should reopen the active state file"
    );

    drop(store);
}

#[test]
fn oversized_state_file_rotates_without_redb_open() {
    let root = temp_root("oversized-state-rotates");
    let state = state_path(&root, None);
    std::fs::create_dir_all(state.parent().unwrap()).expect("create cache dir");
    let file = std::fs::File::create(&state).expect("create oversized placeholder");
    file.set_len(super::OVERSIZED_STATE_FAST_ROTATE_BYTES + 1)
        .expect("size sparse placeholder");
    drop(file);

    let store = SqueezyStore::open(&root, None).expect("open store");

    assert!(store.path().exists(), "active state.redb should be rebuilt");
    assert!(
        std::fs::read_dir(state.parent().unwrap())
            .expect("read cache")
            .filter_map(|entry| entry.ok())
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("oversized-state")),
        "oversized state file should be moved aside without redb open"
    );
}

#[test]
fn graph_write_batch_applies_resolver_cache_changes() {
    let (_root, store) = open_graph_store("resolver-batch");
    let first = FileId::new("src/first.rs");
    let second = FileId::new("src/second.rs");

    let mut batch = GraphWriteBatch::new();
    batch
        .upsert_resolver_entry(&first, &json!({"exports": ["First"]}))
        .expect("encode first resolver entry");
    batch
        .upsert_resolver_entry(&second, &json!({"exports": ["Second"]}))
        .expect("encode second resolver entry");
    assert_eq!(batch.len(), 2);
    store
        .apply_graph_batch(&batch)
        .expect("apply resolver batch");

    let first_entry: serde_json::Value = store
        .resolver_entry(&first)
        .expect("load first")
        .expect("first present");
    assert_eq!(first_entry["exports"][0], "First");
    let second_entry: serde_json::Value = store
        .resolver_entry(&second)
        .expect("load second")
        .expect("second present");
    assert_eq!(second_entry["exports"][0], "Second");

    let mut update = GraphWriteBatch::new();
    update.remove_resolver_entry(&first);
    update
        .upsert_resolver_entry(&second, &json!({"exports": ["SecondV2"]}))
        .expect("encode updated second resolver entry");
    store
        .apply_graph_batch(&update)
        .expect("apply resolver update");

    assert!(
        store
            .resolver_entry::<serde_json::Value>(&first)
            .expect("load removed first")
            .is_none(),
        "resolver removal should be applied in the batch"
    );
    let second_entry: serde_json::Value = store
        .resolver_entry(&second)
        .expect("load updated second")
        .expect("second remains");
    assert_eq!(second_entry["exports"][0], "SecondV2");
}

#[test]
fn state_open_creates_state_only_store() {
    let (root, _store) = open_store("state-only");
    assert!(
        state_path(&root, None).exists(),
        "{STATE_FILE_NAME} should be created by SqueezyStore::open"
    );
    assert!(
        !graph_path(&root, None).exists(),
        "{GRAPH_FILE_NAME} should remain unopened until graph persistence is needed"
    );
}

#[test]
fn graph_open_creates_split_graph_store() {
    let (root, _store) = open_graph_store("graph-only");
    assert!(
        graph_path(&root, None).exists(),
        "{GRAPH_FILE_NAME} should be created by GraphStore::open"
    );
}
