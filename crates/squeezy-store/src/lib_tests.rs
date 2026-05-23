use std::{fs, path::PathBuf};

use serde_json::json;
use squeezy_core::{AppConfig, CostSnapshot, SessionLogConfig, SessionMetrics};

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

fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("squeezy-store-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
}
