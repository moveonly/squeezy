use std::{fs, path::PathBuf};

use redb::{Database, TableDefinition};
use serde_json::json;
use squeezy_core::{AppConfig, CostSnapshot, FileId, SessionLogConfig, SessionMetrics};

use crate::{GraphStoreMetadata, Observation, ObservationKind, SqueezyStore, StoredToolReceipt};

use super::*;

#[test]
fn session_store_lists_filters_and_exports_sessions() {
    let root = temp_root("list-filter-export");
    let config = AppConfig {
        workspace_root: root.clone(),
        session_logs: SessionLogConfig {
            log_dir: Some(PathBuf::from(".squeezy/sessions")),
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle
        .append_event(SessionEvent::new(
            "user_message",
            None,
            Some("find payment bug".to_string()),
            json!({"ok": true}),
        ))
        .expect("append event");

    let sessions = store
        .list(&SessionQuery {
            query: Some("payment".to_string()),
            provider: Some("test-provider".to_string()),
            ..SessionQuery::default()
        })
        .expect("list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, handle.session_id());

    let exported = store.export(handle.session_id()).expect("export");
    assert_eq!(
        exported["metadata"]["first_user_task"].as_str(),
        Some("find payment bug")
    );
    assert_eq!(exported["events"].as_array().map(Vec::len), Some(1));
}

#[test]
fn malformed_event_lines_are_counted_as_warnings() {
    let root = temp_root("malformed");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    fs::write(
        store.root().join(handle.session_id()).join("events.jsonl"),
        b"{not json}\n",
    )
    .expect("write malformed line");

    let record = store.show(handle.session_id()).expect("show");
    assert_eq!(record.event_warnings, 1);
    assert!(record.events.is_empty());
}

#[test]
fn finish_preserves_terminal_status_set_by_earlier_events() {
    let root = temp_root("preserve-terminal-status");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");

    handle
        .update_metadata(|metadata| {
            metadata.status = SessionStatus::Failed;
            metadata.latest_summary = Some("provider error".to_string());
        })
        .expect("mark failed");

    handle
        .finish(
            SessionStatus::Completed,
            CostSnapshot::default(),
            SessionMetrics::default(),
            0,
        )
        .expect("finish session");

    let metadata = handle.metadata().expect("read metadata");
    assert_eq!(metadata.status, SessionStatus::Failed);
    assert!(metadata.ended_at_ms.is_some());
}

#[test]
fn routine_events_skip_metadata_writes_but_event_count_stays_accurate() {
    let root = temp_root("metadata-write-amplification");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");

    let metadata_path = store.root().join(handle.session_id()).join("metadata.json");
    let mtime_after_create = fs::metadata(&metadata_path)
        .expect("stat fresh metadata")
        .modified()
        .expect("mtime");

    // A burst of routine events (the kinds the agent emits per tool call /
    // tool result / approval round trip) must not touch metadata.json. Many
    // filesystems, including APFS on macOS, have second-granularity mtimes,
    // so we rely on a content hash plus a file-size check instead of trying
    // to race the clock.
    let before_bytes = fs::read(&metadata_path).expect("read metadata before");
    for index in 0..20 {
        handle
            .append_event(SessionEvent::new(
                "tool_call",
                Some("1".to_string()),
                Some(format!("tool {index}")),
                json!({"index": index}),
            ))
            .expect("append routine event");
    }
    let after_bytes = fs::read(&metadata_path).expect("read metadata after");
    assert_eq!(
        before_bytes, after_bytes,
        "routine events must not rewrite metadata.json",
    );
    let _ = mtime_after_create;

    // event_count is still surfaced accurately via the in-memory counter so
    // `sessions show` and `sessions list` consumers see a current value even
    // before a metadata-touching event flushes it.
    let observed = handle.metadata().expect("read metadata").event_count;
    assert_eq!(observed, 20, "event_count must reflect routine events");

    // The first user_message and any assistant/failed/cancelled summary do
    // get persisted so discovery surfaces stay useful across processes.
    handle
        .append_event(SessionEvent::new(
            "user_message",
            None,
            Some("first task".to_string()),
            json!({}),
        ))
        .expect("append user_message");
    let on_disk = fs::read_to_string(&metadata_path).expect("read metadata after user_message");
    assert!(on_disk.contains("first task"), "metadata: {on_disk}");
    let metadata = handle.metadata().expect("read metadata");
    assert_eq!(metadata.event_count, 21);
    assert_eq!(metadata.first_user_task.as_deref(), Some("first task"));
}

#[test]
fn cleanup_does_not_sweep_running_sessions_via_retention() {
    let root = temp_root("retention-skip-running");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let running = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start running");
    // Forge an ancient start time. A retention sweep would normally pick
    // this up, but the session is still Running so cleanup must skip it.
    running
        .update_metadata(|metadata| {
            metadata.started_at_ms = 1;
        })
        .expect("backdate running session");
    let completed = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start completed");
    completed
        .finish(
            SessionStatus::Completed,
            CostSnapshot::default(),
            SessionMetrics::default(),
            0,
        )
        .expect("finish completed");
    completed
        .update_metadata(|metadata| {
            metadata.started_at_ms = 1;
            metadata.ended_at_ms = Some(1);
        })
        .expect("backdate completed session");

    let report = store.cleanup(&[]).expect("cleanup");
    assert_eq!(
        report.removed,
        vec![completed.session_id().to_string()],
        "only the completed-and-expired session should be swept",
    );
    assert!(
        store.root().join(running.session_id()).exists(),
        "running session must survive a retention sweep",
    );
}

#[test]
fn cleanup_excluding_skips_protected_session() {
    let root = temp_root("cleanup-protect");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let protected = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start protected");
    let collateral = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start collateral");

    let report = store
        .cleanup_excluding(
            &[
                protected.session_id().to_string(),
                collateral.session_id().to_string(),
            ],
            Some(protected.session_id()),
        )
        .expect("cleanup");

    assert_eq!(report.removed, vec![collateral.session_id().to_string()]);
    assert!(
        store.root().join(protected.session_id()).exists(),
        "protected session must remain on disk"
    );
}

#[test]
fn state_store_round_trips_graph_receipts_and_observations() {
    let root = temp_root("state-round-trip");
    let store = SqueezyStore::open(&root, None).expect("open store");

    let metadata = GraphStoreMetadata {
        workspace_root: root.display().to_string(),
        crawl_options_hash: "crawl".to_string(),
        language_registry_version: "langs".to_string(),
        graph_format_version: 1,
    };
    store.set_graph_metadata(&metadata).expect("set metadata");
    assert_eq!(store.graph_metadata().expect("metadata"), Some(metadata));

    let file_id = FileId::new("src/lib.rs");
    store
        .put_graph_partition(&file_id, &serde_json::json!({"hash": "abc"}))
        .expect("put partition");
    let partition: serde_json::Value = store
        .graph_partition(&file_id)
        .expect("partition")
        .expect("partition exists");
    assert_eq!(partition["hash"], "abc");

    store
        .put_tool_receipt(&StoredToolReceipt {
            tool_name: "read_file".to_string(),
            stable_output_sha256: "out".to_string(),
            call_id: "call_1".to_string(),
            content_sha256: Some("content".to_string()),
            model_output_bytes: 42,
            created_unix_millis: 1,
        })
        .expect("put receipt");
    assert_eq!(store.tool_receipts().expect("receipts").len(), 1);

    let mut observation = Observation::new(
        ObservationKind::Decision,
        "Use redb for graph persistence",
        "test",
    );
    observation.tags.push("graph".to_string());
    let observation = store.put_observation(observation).expect("put observation");
    assert_eq!(
        store
            .get_observation(&observation.id)
            .expect("get observation")
            .expect("observation")
            .text,
        "Use redb for graph persistence"
    );
    assert_eq!(
        store
            .search_observations("redb graph", 10)
            .expect("search observations")
            .len(),
        1
    );
    store
        .delete_observation(&observation.id)
        .expect("delete observation");
    assert!(
        store
            .search_observations("redb graph", 10)
            .expect("search observations")
            .is_empty()
    );
}

#[test]
fn state_store_schema_mismatch_backs_up_old_database_without_data_loss() {
    let root = temp_root("state-schema-mismatch");
    let state = root.join(".squeezy").join("cache").join("state.redb");
    fs::create_dir_all(state.parent().unwrap()).expect("create cache dir");
    write_schema_version(&state, 0);

    let store = SqueezyStore::open(&root, None).expect("open store");
    assert_eq!(
        store
            .graph_metadata()
            .expect("metadata")
            .map(|metadata| metadata.graph_format_version),
        None
    );
    assert!(
        fs::read_dir(state.parent().unwrap())
            .expect("read cache")
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().contains("schema-0")),
        "old schema database should be backed up"
    );
}

#[test]
fn state_store_open_rejects_a_second_handle_on_the_same_file() {
    // redb enforces single-process exclusivity by failing the second
    // `Database::create` with a lock error. Squeezy relies on this contract
    // to keep agent and tool-registry layers sharing a single
    // `Arc<SqueezyStore>` instead of accidentally racing two independent
    // handles against the same file: regressions there silently disable
    // graph persistence because the second open returns an error that
    // callers downgrade to `None`. This test pins the redb behavior so we
    // notice if it ever changes.
    let root = temp_root("state-dual-open");
    let first = SqueezyStore::open(&root, None).expect("first open");
    let second = SqueezyStore::open(&root, None);
    assert!(
        second.is_err(),
        "redb must reject a second open on the same database file"
    );
    let _ = first;
}

fn write_schema_version(path: &std::path::Path, version: u64) {
    const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
    let database = Database::create(path).expect("create old database");
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

fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("squeezy-store-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
}
