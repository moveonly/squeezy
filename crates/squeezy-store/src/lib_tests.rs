use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Serialises tests that mutate `XDG_CACHE_HOME` to avoid data races in
/// parallel test threads (Rust 2024: `env::set_var` is `unsafe`).
static XDG_LOCK: Mutex<()> = Mutex::new(());

fn with_xdg_cache_home<R>(xdg: &Path, body: impl FnOnce() -> R) -> R {
    let _guard = XDG_LOCK.lock().expect("XDG_CACHE_HOME lock");
    let previous = std::env::var_os("XDG_CACHE_HOME");
    // SAFETY: XDG_LOCK above serialises mutations of XDG_CACHE_HOME across
    // all tests in this module.
    unsafe { std::env::set_var("XDG_CACHE_HOME", xdg) };
    let result = body();
    unsafe {
        match previous {
            Some(value) => std::env::set_var("XDG_CACHE_HOME", value),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }
    result
}

use redb::{Database, TableDefinition};
use serde_json::json;
use squeezy_core::FileId;
use squeezy_core::{AppConfig, LanguageKind};

use crate::{
    CompactionCheckpoint, GRAPH_FILE_NAME, GRAPH_PARTITIONS, GRAPH_SCHEMA_VERSION, GraphStore,
    GraphStoreMetadata, GraphWriteBatch, META, STATE_FILE_NAME, SqueezyStore, WorkspaceStores,
    cache_dir_path, encode_graph, fs_util, graph_manifest_path, graph_path, graph_v3_dir_path,
    insert_json, sessions::ResumeItem, state_path,
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

#[test]
fn atomic_write_replaces_existing_file() {
    let root = temp_root("atomic-replace-existing");
    let path = root.join("metadata.json");
    fs_util::write_bytes_atomically(&path, br#"{"value":"first"}"#).expect("first write");
    fs_util::write_bytes_atomically(&path, br#"{"value":"second"}"#).expect("replace write");
    let body = std::fs::read_to_string(&path).expect("read replaced file");
    assert_eq!(body, r#"{"value":"second"}"#);
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .expect("read temp root")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".tmp"))
        })
        .collect();
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn squeezy_store_reuses_lazy_graph_store_handle() {
    let (root, store) = open_store("lazy-graph-handle");
    let file_id = FileId("src/lib.rs".to_string());
    store
        .put_graph_partition(&file_id, &json!({ "symbols": ["one"] }))
        .expect("first graph write");
    store
        .put_graph_partition(&file_id, &json!({ "symbols": ["two"] }))
        .expect("second graph write through cached handle");
    let stored: serde_json::Value = store
        .graph_partition(&file_id)
        .expect("read graph partition")
        .expect("partition exists");
    assert_eq!(stored["symbols"][0], "two");
    let _ = std::fs::remove_dir_all(&root);
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
fn xdg_cache_root_resolves_under_xdg_cache_home_with_repo_id() {
    let root = temp_root("xdg-cache-root");
    let xdg = temp_root("xdg-cache-home");

    let resolved = with_xdg_cache_home(&xdg, || cache_dir_path(&root, Some(Path::new("xdg"))));

    assert!(
        resolved.starts_with(&xdg),
        "resolved={}",
        resolved.display()
    );
    let expected_parent = xdg.join("squeezy");
    assert_eq!(resolved.parent(), Some(expected_parent.as_path()));
    assert!(
        resolved
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("xdg-cache-root")),
        "repo id should preserve a readable workspace prefix: {}",
        resolved.display()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_mountinfo_classifies_network_and_virtual_filesystems() {
    let entries = super::parse_linux_mountinfo(
        "31 23 0:27 / / rw,relatime - ext4 /dev/sda1 rw\n\
         32 23 0:28 / /mnt/share rw,relatime - nfs4 server:/share rw\n\
         33 23 0:29 / /work rw,relatime - overlay overlay rw\n\
         34 23 0:30 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n",
    );

    let nfs = entries
        .iter()
        .find(|entry| entry.mount_point == Path::new("/mnt/share"))
        .expect("nfs mount");
    assert_eq!(
        super::classify_filesystem(&nfs.fs_type),
        crate::StorageMountClassification::Network
    );
    let overlay = entries
        .iter()
        .find(|entry| entry.mount_point == Path::new("/work"))
        .expect("overlay mount");
    assert_eq!(
        super::classify_filesystem(&overlay.fs_type),
        crate::StorageMountClassification::Virtual
    );
    let procfs = entries
        .iter()
        .find(|entry| entry.mount_point == Path::new("/proc"))
        .expect("proc mount");
    assert_eq!(
        super::classify_filesystem(&procfs.fs_type),
        crate::StorageMountClassification::Virtual
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
    batch
        .set_import_graph(&json!({"imports_by_file": {"src/first.rs": ["src/second.rs"]}}))
        .expect("encode import graph");
    assert_eq!(batch.len(), 3);
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
    let import_graph: serde_json::Value = store
        .import_graph()
        .expect("load import graph")
        .expect("import graph present");
    assert_eq!(
        import_graph["imports_by_file"]["src/first.rs"][0],
        "src/second.rs"
    );

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
    let (root, store) = open_store("state-only");
    assert!(
        state_path(&root, None).exists(),
        "{STATE_FILE_NAME} should be created by SqueezyStore::open"
    );
    assert!(
        !graph_path(&root, None).exists(),
        "{GRAPH_FILE_NAME} should remain unopened until graph persistence is needed"
    );
    assert!(
        !graph_v3_dir_path(&root, None).exists(),
        "graph v3 cache should remain unopened until graph persistence is needed"
    );

    let metadata = GraphStoreMetadata {
        workspace_root: root.display().to_string(),
        crawl_options_hash: "crawl".to_string(),
        language_registry_version: "langs".to_string(),
        graph_format_version: 1,
    };
    store
        .set_graph_metadata(&metadata)
        .expect("lazy graph metadata write");
    assert_eq!(
        store.graph_metadata().expect("lazy graph metadata read"),
        Some(metadata)
    );
}

#[test]
fn graph_open_creates_split_graph_store() {
    let (root, _store) = open_graph_store("graph-only");
    assert!(
        graph_manifest_path(&root, None).exists(),
        "graph v3 manifest should be created by GraphStore::open"
    );
    assert!(
        !graph_path(&root, None).exists(),
        "{GRAPH_FILE_NAME} should remain a legacy fallback"
    );
}

#[test]
fn workspace_stores_reuses_state_and_graph_handles() {
    let root = temp_root("workspace-stores");
    let stores = WorkspaceStores::new(root.clone(), None);

    let first_state = stores.state().expect("first state open");
    let second_state = stores.state().expect("second state open");
    assert!(Arc::ptr_eq(&first_state, &second_state));

    let first_graph = stores.graph().expect("first graph open");
    let second_graph = stores.graph().expect("second graph open");
    assert!(Arc::ptr_eq(&first_graph, &second_graph));
    assert!(graph_manifest_path(&root, None).exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn graph_write_batch_uses_language_bucket_shards() {
    let (root, store) = open_graph_store("graph-shards");
    let file_id = FileId::new("src/lib.rs");
    let mut batch = GraphWriteBatch::new();
    batch
        .upsert_partition_for_language(
            &file_id,
            LanguageKind::Rust,
            &json!({ "symbols": ["rust"] }),
        )
        .expect("encode rust partition");
    store.apply_graph_batch(&batch).expect("apply shard batch");

    let shard_dir = graph_v3_dir_path(&root, None).join("shards").join("rust");
    let shard_files = std::fs::read_dir(&shard_dir)
        .expect("read rust shard dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "redb"))
        .count();
    assert_eq!(shard_files, 1, "expected one rust shard file");
    let stored: serde_json::Value = store
        .graph_partition(&file_id)
        .expect("read graph partition")
        .expect("partition exists");
    assert_eq!(stored["symbols"][0], "rust");

    let mut resolver_batch = GraphWriteBatch::new();
    resolver_batch
        .upsert_resolver_entry_for_language(
            &file_id,
            LanguageKind::Rust,
            &json!({"exports": ["Rust"]}),
        )
        .expect("encode resolver entry");
    store
        .apply_graph_batch(&resolver_batch)
        .expect("write resolver entry");
    let mut remove_resolver = GraphWriteBatch::new();
    remove_resolver.remove_resolver_entry(&file_id);
    store
        .apply_graph_batch(&remove_resolver)
        .expect("remove resolver entry");
    assert!(
        store
            .graph_partition::<serde_json::Value>(&file_id)
            .expect("read partition after resolver removal")
            .is_some(),
        "resolver-only removal must not orphan the graph partition"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn graph_store_reads_legacy_graph_redb_as_best_effort_fallback() {
    let root = temp_root("legacy-graph-fallback");
    let file_id = FileId::new("src/legacy.rs");
    let legacy_path = graph_path(&root, None);
    std::fs::create_dir_all(legacy_path.parent().unwrap()).expect("create cache dir");
    let database = Database::create(&legacy_path).expect("create legacy graph");
    let write = database.begin_write().expect("legacy write");
    {
        let mut meta = write.open_table(META).expect("meta");
        insert_json(&mut meta, "schema_version", &GRAPH_SCHEMA_VERSION).expect("schema");
        let mut partitions = write.open_table(GRAPH_PARTITIONS).expect("partitions");
        let encoded = encode_graph(&json!({ "symbols": ["legacy"] })).expect("encode");
        partitions
            .insert(file_id.0.as_str(), encoded.as_slice())
            .expect("insert legacy partition");
    }
    write.commit().expect("commit legacy");
    drop(database);

    let store = GraphStore::open(&root, None).expect("open v3 graph");
    let stored: serde_json::Value = store
        .graph_partition(&file_id)
        .expect("read legacy partition")
        .expect("legacy partition exists");
    assert_eq!(stored["symbols"][0], "legacy");
    assert!(graph_manifest_path(&root, None).exists());

    let _ = std::fs::remove_dir_all(&root);
}
