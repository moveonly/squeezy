use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc, time::Duration};

use redb::{Database, TableDefinition};
use serde_json::json;
use squeezy_core::{
    AppConfig, ContextAttachment, ContextAttachmentKind, ContextAttachmentSource,
    ContextAttachmentStatus, ContextCompactionRecord, ContextCompactionState,
    ContextCompactionTrigger, ContextEstimate, ContextPin, CostSnapshot, FileId, SessionLogConfig,
    SessionMetrics,
};

use crate::{
    BugReportOptions, GraphStoreMetadata, Observation, ObservationKind, SqueezyStore,
    StoredReadSnapshot, StoredToolReceipt,
};

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
    handle.flush_events().expect("flush events");

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
fn session_export_preserves_task_state_events() {
    let root = temp_root("task-state-export");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle
        .append_event(SessionEvent::new(
            "task_state",
            Some("1".to_string()),
            Some("Implement task UX | status=blocked".to_string()),
            json!({
                "snapshot": {
                    "task": "Implement task UX",
                    "status": "blocked",
                    "blocker": "waiting for approval",
                    "next_action": "cancel or approve",
                    "verification": "not_started",
                    "replan_reason": "new evidence changed the next step"
                }
            }),
        ))
        .expect("append task state");
    handle.flush_events().expect("flush events");

    let exported = store.export(handle.session_id()).expect("export");
    let events = exported["events"].as_array().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "task_state");
    assert_eq!(
        events[0]["payload"]["snapshot"]["replan_reason"],
        "new evidence changed the next step"
    );
}

#[test]
fn session_resume_state_preserves_context_compaction() {
    let root = temp_root("context-compaction");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    let compaction = ContextCompactionState {
        generation: 2,
        summary: Some("compacted facts".to_string()),
        pinned: vec![ContextPin {
            id: "pin-0001".to_string(),
            label: "decision".to_string(),
            summary: "keep this decision".to_string(),
            source: "transcript:1".to_string(),
            created_unix_ms: 42,
        }],
        last: Some(ContextCompactionRecord {
            generation: 2,
            trigger: ContextCompactionTrigger::Manual,
            compacted_at_ms: 43,
            before: ContextEstimate {
                bytes: 4000,
                estimated_tokens: 1000,
                items: 20,
            },
            after: ContextEstimate {
                bytes: 800,
                estimated_tokens: 200,
                items: 5,
            },
            dropped_items: 15,
            summary_bytes: 700,
            replacement_id: None,
        }),
        history: Vec::new(),
    };
    handle
        .write_resume_state(&SessionResumeState {
            resume_available: true,
            context_compaction: compaction.clone(),
            ..SessionResumeState::default()
        })
        .expect("write resume");
    handle
        .append_event(SessionEvent::new(
            "context_compacted",
            Some("turn-1".to_string()),
            Some("compacted context".to_string()),
            json!({ "record": compaction.last.clone() }),
        ))
        .expect("append compaction event");
    handle.flush_events().expect("flush events");

    let record = store.show(handle.session_id()).expect("show");
    assert_eq!(
        record.resume_state.expect("resume").context_compaction,
        compaction
    );
    let exported = store.export(handle.session_id()).expect("export");
    assert_eq!(exported["events"][0]["kind"], "context_compacted");
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
fn replay_tape_round_trips_and_exports() {
    let root = temp_root("replay-round-trip");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");

    handle
        .append_replay_event(SessionReplayEvent::new(
            SessionReplayEventKind::UserMessage,
            Some("1".to_string()),
            json!({"input": "find a bug"}),
        ))
        .expect("append user");
    handle
        .append_replay_event(SessionReplayEvent::new(
            SessionReplayEventKind::ModelCompleted,
            Some("1".to_string()),
            json!({"response_id": "resp_1", "cost": CostSnapshot::default()}),
        ))
        .expect("append completion");

    let tape = store.replay_tape(handle.session_id()).expect("read replay");
    assert_eq!(tape.schema_version, SESSION_REPLAY_SCHEMA_VERSION);
    assert_eq!(tape.session_id, handle.session_id());
    assert_eq!(tape.events.len(), 2);
    assert_eq!(tape.events[0].sequence, 1);
    assert_eq!(tape.events[1].sequence, 2);
    assert_eq!(tape.warnings, 0);

    let exported = store.export(handle.session_id()).expect("export");
    assert_eq!(
        exported["replay"]["events"].as_array().map(Vec::len),
        Some(2)
    );
}

#[test]
fn replay_tape_counts_tampered_lines_as_warnings() {
    let root = temp_root("replay-tamper");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    fs::write(
        store.root().join(handle.session_id()).join("replay.jsonl"),
        br#"{"schema_version":1,"ts_unix_ms":1,"sequence":1,"kind":"user_message","turn_id":"1","payload_sha256":"bad","payload":{"input":"changed"}}"#,
    )
    .expect("write tampered replay");

    let tape = store.replay_tape(handle.session_id()).expect("read replay");
    assert!(tape.events.is_empty());
    assert_eq!(tape.warnings, 1);
}

#[test]
fn bug_report_redacts_replay_tape() {
    let root = temp_root("bug-report-replay-redaction");
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
        .append_replay_event(SessionReplayEvent::new(
            SessionReplayEventKind::ModelTextDelta,
            Some("1".to_string()),
            json!({"text": "Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456"}),
        ))
        .expect("append replay");

    let bundle = store
        .build_bug_report(
            &config,
            handle.session_id(),
            BugReportOptions {
                excluded_sections: BTreeSet::new(),
                max_section_bytes: 4096,
                max_archive_bytes: 2 * 1024 * 1024,
            },
        )
        .expect("build report");
    let archive = String::from_utf8_lossy(&bundle.archive_bytes);
    assert!(archive.contains("replay.json"));
    assert!(archive.contains("<redacted:"));
    assert!(!archive.contains("abcdefghijklmnopqrstuvwxyz123456"));
}

#[test]
fn bug_report_archive_redacts_events_and_records_exclusions() {
    let root = temp_root("bug-report-redaction");
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
            Some("OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz123456".to_string()),
            json!({
                "OPENAI_API_KEY=sk-keyonlyabcdefghijklmnopqrstuvwxyz123456": "secret-shaped key",
                "stderr": "Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456",
                "path": "src/lib.rs",
            }),
        ))
        .expect("append event");
    handle
        .append_event(SessionEvent::new(
            "approval_requested",
            Some("1".to_string()),
            Some("shell".to_string()),
            json!({"permission": {"metadata": {"env": "TOKEN=secret-token-value"}}}),
        ))
        .expect("append approval");
    handle.flush_events().expect("flush events");

    let bundle = store
        .build_bug_report(
            &config,
            handle.session_id(),
            BugReportOptions {
                excluded_sections: BTreeSet::from(["replay".to_string()]),
                max_section_bytes: 4096,
                max_archive_bytes: 2 * 1024 * 1024,
            },
        )
        .expect("build report");
    let archive = String::from_utf8_lossy(&bundle.archive_bytes);

    assert_eq!(
        bundle.manifest["archive_bytes"].as_u64(),
        Some(bundle.archive_bytes.len() as u64)
    );
    assert!(archive.contains("manifest.json"));
    assert!(archive.contains("session/events.jsonl"));
    assert!(archive.contains("\"excluded_sections\""));
    assert!(archive.contains("replay"));
    assert!(archive.contains("<redacted:"), "{archive}");
    assert!(!archive.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
    assert!(!archive.contains("sk-keyonlyabcdefghijklmnopqrstuvwxyz123456"));
    assert!(!archive.contains("abcdefghijklmnopqrstuvwxyz123456"));
    assert!(!archive.contains("secret-token-value"));
    assert!(
        bundle
            .sections
            .iter()
            .any(|section| section.name == "events" && section.redactions > 0)
    );
    assert!(bundle.preview_text().contains("archive_bytes="));
}

#[test]
fn context_attachments_store_redacted_text_and_export_metadata() {
    let root = temp_root("context-attachments");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    let attachment = ContextAttachment {
        id: "att-0001".to_string(),
        source: ContextAttachmentSource::Paste,
        kind: ContextAttachmentKind::Log,
        status: ContextAttachmentStatus::Attached,
        label: "pasted context".to_string(),
        path: None,
        original_sha256: "original".to_string(),
        redacted_sha256: Some("redacted".to_string()),
        original_bytes: 40,
        stored_bytes: 30,
        preview_bytes: 20,
        redactions: 1,
        preview: "OPENAI_API_KEY=<redacted:openai_key#1 bytes=29>".to_string(),
        truncated: false,
    };

    handle
        .write_context_attachment(
            &attachment,
            Some("OPENAI_API_KEY=<redacted:openai_key#1 bytes=29>"),
        )
        .expect("write attachment");

    let record = store.show(handle.session_id()).expect("show");
    assert_eq!(record.attachments, vec![attachment.clone()]);
    let exported = store.export(handle.session_id()).expect("export");
    assert_eq!(exported["attachments"][0]["id"].as_str(), Some("att-0001"));
    let text_path = store
        .root()
        .join(handle.session_id())
        .join("attachments")
        .join("att-0001.txt");
    let on_disk = fs::read_to_string(text_path).expect("read attachment text");
    assert!(on_disk.contains("<redacted:openai_key"));
    assert!(!on_disk.contains("sk-abcdefghijklmnopqrstuvwxyz"));
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
fn session_log_writer_flushes_concurrent_events() {
    let root = temp_root("async-session-log-concurrent");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = Arc::new(
        store
            .start_session(SessionMetadata::new(&config, "test-provider"))
            .expect("start session"),
    );

    let threads = (0..50)
        .map(|index| {
            let handle = handle.clone();
            std::thread::spawn(move || {
                handle
                    .append_event(SessionEvent::new(
                        "tool_call",
                        Some("1".to_string()),
                        Some(format!("tool {index}")),
                        json!({"index": index}),
                    ))
                    .expect("append event");
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().expect("append thread");
    }
    handle.flush_events().expect("flush events");

    let record = store.show(handle.session_id()).expect("show session");
    assert_eq!(record.events.len(), 50);
    assert_eq!(handle.metadata().expect("metadata").event_count, 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_event_does_not_block_tokio_reactor() {
    let root = temp_root("async-session-log-reactor");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = Arc::new(
        store
            .start_session(SessionMetadata::new(&config, "test-provider"))
            .expect("start session"),
    );
    let canary = tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(10)).await;
    });

    let mut tasks = Vec::new();
    for index in 0..100 {
        let handle = handle.clone();
        tasks.push(tokio::spawn(async move {
            handle
                .append_event(SessionEvent::new(
                    "tool_call",
                    Some("1".to_string()),
                    Some(format!("tool {index}")),
                    json!({"index": index}),
                ))
                .expect("append event");
        }));
    }

    tokio::time::timeout(Duration::from_millis(50), canary)
        .await
        .expect("reactor canary should not be starved")
        .expect("canary task");
    for task in tasks {
        task.await.expect("append task");
    }
    handle.flush_events().expect("flush events");

    let record = store.show(handle.session_id()).expect("show session");
    assert_eq!(record.events.len(), 100);
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
            summary: Some("read_file sample".to_string()),
        })
        .expect("put receipt");
    assert_eq!(store.tool_receipts().expect("receipts").len(), 1);

    let read_snapshot = StoredReadSnapshot {
        path: "src/lib.rs".to_string(),
        tool_name: "read_slice".to_string(),
        call_id: "read_1".to_string(),
        stable_output_sha256: "read-out".to_string(),
        content_sha256: Some("read-content".to_string()),
        start_byte: 0,
        end_byte: 12,
        content: "fn main() {}".to_string(),
        model_output_bytes: 128,
        created_unix_millis: 2,
    };
    store
        .put_read_snapshot(&read_snapshot)
        .expect("put read snapshot");
    assert_eq!(
        store
            .read_snapshot("src/lib.rs")
            .expect("read snapshot")
            .expect("snapshot exists"),
        read_snapshot
    );

    // A second snapshot for a non-overlapping window must coexist with the
    // first rather than overwriting it: read_snapshots are keyed by
    // `(path, start_byte, end_byte)` so distinct windows of the same file
    // remain independently retrievable.
    let second_window = StoredReadSnapshot {
        path: "src/lib.rs".to_string(),
        tool_name: "read_slice".to_string(),
        call_id: "read_2".to_string(),
        stable_output_sha256: "read-out-2".to_string(),
        content_sha256: Some("read-content".to_string()),
        start_byte: 64,
        end_byte: 96,
        content: "    println!(\"hello\");          ".to_string(),
        model_output_bytes: 256,
        created_unix_millis: 3,
    };
    store
        .put_read_snapshot(&second_window)
        .expect("put second window snapshot");
    let mut snapshots = store
        .read_snapshots_for_path("src/lib.rs")
        .expect("snapshots for path");
    snapshots.sort_by_key(|snapshot| snapshot.start_byte);
    assert_eq!(
        snapshots,
        vec![read_snapshot.clone(), second_window.clone()]
    );
    assert_eq!(
        store
            .read_snapshot_for_window("src/lib.rs", 0, 12)
            .expect("first window")
            .expect("first window stored"),
        read_snapshot
    );
    assert_eq!(
        store
            .read_snapshot_for_window("src/lib.rs", 64, 96)
            .expect("second window")
            .expect("second window stored"),
        second_window
    );
    assert!(
        store
            .read_snapshot_for_window("src/lib.rs", 0, 64)
            .expect("missing window")
            .is_none(),
        "windows that were never stored must not match a stale snapshot"
    );

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

#[test]
fn replay_resume_state_without_resume_json() {
    let root = temp_root("replay-without-resume-json");
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
            json!({}),
        ))
        .expect("append user event");
    handle
        .append_event(SessionEvent::new(
            "assistant_completed",
            None,
            Some("checking the payments module".to_string()),
            json!({ "response_id": "resp_1" }),
        ))
        .expect("append assistant event");
    handle.flush_events().expect("flush events");
    let session_dir = store.root().join(handle.session_id());
    std::fs::remove_file(session_dir.join("resume_state.json")).expect("delete resume_state.json");

    let replayed = handle.replay_resume_state().expect("replay");
    assert!(replayed.resume_available);
    assert_eq!(replayed.conversation.len(), 2);
    match &replayed.conversation[0] {
        ResumeItem::UserText { text } => assert!(text.contains("find payment bug")),
        other => panic!("expected UserText, got {other:?}"),
    }
    match &replayed.conversation[1] {
        ResumeItem::AssistantText { text } => assert!(text.contains("checking the payments")),
        other => panic!("expected AssistantText, got {other:?}"),
    }
}

#[test]
fn replay_snaps_to_compaction_checkpoint() {
    let root = temp_root("replay-snap-checkpoint");
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
            Some("stale-prelude".to_string()),
            json!({}),
        ))
        .expect("stale event");
    let checkpoint_conversation = vec![
        ResumeItem::UserText {
            text: "compacted user line".to_string(),
        },
        ResumeItem::AssistantText {
            text: "compacted assistant line".to_string(),
        },
    ];
    handle
        .append_event(SessionEvent::new(
            "context_compacted",
            None,
            Some("compacted".to_string()),
            json!({
                "record": null,
                "summary": "compacted summary",
                "replacement_id": "ckpt-1",
                "conversation": checkpoint_conversation.clone(),
            }),
        ))
        .expect("compaction event");
    handle
        .append_event(SessionEvent::new(
            "user_message",
            None,
            Some("post-compaction question".to_string()),
            json!({}),
        ))
        .expect("post event");
    handle.flush_events().expect("flush events");

    let replayed = handle.replay_resume_state().expect("replay");
    assert!(
        replayed
            .conversation
            .iter()
            .all(|item| !matches!(item, ResumeItem::UserText { text } if text == "stale-prelude")),
        "stale events before the checkpoint must be skipped: {:?}",
        replayed.conversation,
    );
    assert!(
        replayed.conversation.iter().any(
            |item| matches!(item, ResumeItem::UserText { text } if text == "compacted user line")
        ),
        "snapshot user line should be present",
    );
    assert!(
        replayed.conversation.iter().any(|item| matches!(item, ResumeItem::UserText { text } if text.contains("post-compaction question"))),
        "post-checkpoint event should be replayed forward",
    );
}

#[test]
fn session_event_kind_parses_unknown_as_unknown() {
    let event = SessionEvent::new("bogus_kind", None, None, json!({"foo": "bar"}));
    let typed = SessionEventKind::try_from_event(&event).expect("typed");
    assert_eq!(typed, SessionEventKind::Unknown);
}

fn open_test_store(label: &str) -> (PathBuf, SessionStore, AppConfig) {
    let root = temp_root(label);
    let config = AppConfig {
        workspace_root: root.clone(),
        session_logs: SessionLogConfig {
            log_dir: Some(PathBuf::from(".squeezy/sessions")),
            log_retention_days: 0,
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    (root, store, config)
}

#[test]
fn archive_excludes_session_from_default_list() {
    let (_root, store, config) = open_test_store("archive-excludes");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle.flush_events().expect("flush events");
    let session_id = handle.session_id().to_string();
    store.archive_session(&session_id).expect("archive session");

    let listed_default = store.list(&SessionQuery::default()).expect("list default");
    assert!(
        listed_default
            .iter()
            .all(|metadata| metadata.session_id != session_id),
        "archived session must be hidden from default list",
    );

    let listed_include = store
        .list(&SessionQuery {
            include_archived: true,
            ..SessionQuery::default()
        })
        .expect("list include archived");
    let found = listed_include
        .iter()
        .find(|metadata| metadata.session_id == session_id)
        .expect("archived session visible with include_archived=true");
    assert_eq!(found.status, SessionStatus::Archived);
}

#[test]
fn cleanup_skips_archived_sessions() {
    let (_root, store, config) = open_test_store("cleanup-skips-archived");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle.flush_events().expect("flush events");
    let session_id = handle.session_id().to_string();
    store.archive_session(&session_id).expect("archive session");

    let report = store
        .cleanup(std::slice::from_ref(&session_id))
        .expect("cleanup with archived id");
    assert!(
        !report.removed.contains(&session_id),
        "archived session must not be removed by cleanup",
    );

    let still_there = store
        .list(&SessionQuery {
            include_archived: true,
            ..SessionQuery::default()
        })
        .expect("post-cleanup list");
    assert!(
        still_there
            .iter()
            .any(|metadata| metadata.session_id == session_id),
        "archived session survives cleanup",
    );
}

#[test]
fn unarchive_round_trip_restores_session() {
    let (_root, store, config) = open_test_store("unarchive-round-trip");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle.flush_events().expect("flush events");
    let session_id = handle.session_id().to_string();
    store.archive_session(&session_id).expect("archive");
    store.unarchive_session(&session_id).expect("unarchive");

    let listed = store
        .list(&SessionQuery::default())
        .expect("list after unarchive");
    let found = listed
        .iter()
        .find(|metadata| metadata.session_id == session_id)
        .expect("unarchived session in default list");
    assert_eq!(found.status, SessionStatus::Completed);
}
