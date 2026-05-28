use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc, time::Duration};

use redb::{Database, TableDefinition};
use serde_json::json;
use squeezy_core::{
    AnthropicThinkingBlock, AnthropicThinkingKind, AppConfig, ContextAttachment,
    ContextAttachmentKind, ContextAttachmentSource, ContextAttachmentStatus,
    ContextCompactionRecord, ContextCompactionState, ContextCompactionTrigger, ContextEstimate,
    ContextPin, CostSnapshot, FileId, ReasoningPayload, SessionLogConfig, SessionMetrics,
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
fn fork_creates_child_with_parent_id() {
    let root = temp_root("fork-creates-child");
    let config = AppConfig {
        workspace_root: root.clone(),
        session_logs: SessionLogConfig {
            log_dir: Some(PathBuf::from(".squeezy/sessions")),
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let parent = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start parent");
    parent
        .write_resume_state(&SessionResumeState {
            resume_available: true,
            conversation: vec![ResumeItem::UserText {
                text: "carry me forward".to_string(),
            }],
            ..SessionResumeState::default()
        })
        .expect("write parent resume");
    let parent_id = parent.session_id().to_string();

    let child = store
        .fork_session(&parent_id, SessionMetadata::new(&config, "test-provider"))
        .expect("fork session");
    let child_metadata = child.metadata().expect("child metadata");
    assert_eq!(
        child_metadata.parent_id.as_deref(),
        Some(parent_id.as_str())
    );
    assert_ne!(child.session_id(), parent_id);

    let child_resume = child.read_resume_state().expect("child resume");
    assert!(matches!(
        child_resume.conversation.first(),
        Some(ResumeItem::UserText { text }) if text == "carry me forward"
    ));

    // Parent session must still exist intact on disk so a later
    // `squeezy sessions resume <parent_id>` works.
    let parent_record = store.show(&parent_id).expect("parent show");
    assert_eq!(parent_record.metadata.session_id, parent_id);
    assert!(parent_record.metadata.parent_id.is_none());
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
    // Live retention is now a soft delete: the completed-and-expired
    // session moves to `archived/<id>/` instead of being removed
    // outright. `archived` carries the soft-deleted ids; `removed`
    // is reserved for the archive retention sweep that runs in the
    // same pass once `retention_archive_days` is exceeded.
    assert_eq!(
        report.archived,
        vec![completed.session_id().to_string()],
        "the completed-and-expired session should be archived, not deleted",
    );
    assert!(
        report.removed.is_empty(),
        "live retention must not permanently delete; got {:?}",
        report.removed,
    );
    assert!(
        store.root().join(running.session_id()).exists(),
        "running session must survive a retention sweep",
    );
    assert!(
        store
            .root()
            .join(super::ARCHIVED_SUBDIR)
            .join(completed.session_id())
            .exists(),
        "completed session must land in the archive subtree",
    );
    assert!(
        !store.root().join(completed.session_id()).exists(),
        "completed session must leave the live root after archival",
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

    // Explicit `ids` archive the live session rather than deleting it,
    // matching the retention sweep so neither path destroys history.
    assert_eq!(report.archived, vec![collateral.session_id().to_string()]);
    assert!(report.removed.is_empty());
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

#[test]
fn token_calibration_round_trips_through_metadata_and_global_file() {
    let root = temp_root("calibration-roundtrip");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);

    // Initial global file is missing -> defaults are returned, the call must
    // not panic or return an error.
    let initial = store.load_global_calibration();
    assert!(
        initial.providers.is_empty(),
        "fresh stores must yield an empty calibration"
    );

    // Persist a non-trivial calibration into the global file and confirm a
    // subsequent load sees the same ratios. The EMA blending is exercised by
    // the squeezy-llm tests; here we only verify the round trip.
    let mut calibration = squeezy_llm::TokenCalibration::default();
    calibration.record_sample("openai", 4500, 1000);
    calibration.record_sample("anthropic", 3800, 1000);
    store
        .save_global_calibration(&calibration)
        .expect("save global calibration");
    let reloaded = store.load_global_calibration();
    assert_eq!(reloaded, calibration);

    // The same calibration must also survive being written into a session's
    // metadata.json and read back via `show`.
    let mut metadata = SessionMetadata::new(&config, "openai");
    metadata.token_calibration = calibration.clone();
    let handle = store.start_session(metadata).expect("start session");
    let session_id = handle.session_id().to_string();
    drop(handle);
    let record = store.show(&session_id).expect("show session");
    assert_eq!(record.metadata.token_calibration, calibration);
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

#[test]
fn typed_session_events_round_trip_through_event_log() {
    let root = temp_root("typed-events-roundtrip");
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

    let expected = vec![
        SessionEventKind::SessionStarted,
        SessionEventKind::UserMessage {
            text: "ship it".to_string(),
        },
        SessionEventKind::ToolCall {
            call_id: "call-1".to_string(),
            tool: "read_file".to_string(),
            arguments: json!({"path": "src/lib.rs"}),
        },
        SessionEventKind::ToolResult {
            output: json!({"call_id": "call-1", "output": "fn main() {}"}),
        },
        SessionEventKind::ApprovalRequested {
            tool: "shell".to_string(),
            payload: json!({"command": "ls"}),
        },
        SessionEventKind::ApprovalDecided {
            tool: "shell".to_string(),
            decision: "allowed".to_string(),
            payload: json!({"command": "ls"}),
        },
        SessionEventKind::AssistantCompleted {
            text: "all set".to_string(),
            response_id: Some("resp-1".to_string()),
        },
        SessionEventKind::ContextCompacted {
            record: json!(null),
            summary: Some("compacted".to_string()),
            replacement_id: Some("ckpt-1".to_string()),
            conversation: Vec::new(),
        },
        SessionEventKind::Reasoning {
            payload: ReasoningPayload::OpenAi {
                item_id: "rs_typed_log".to_string(),
                summary: vec!["typed log thinking".to_string()],
                encrypted_content: Some("ENCRYPTED-TYPED".to_string()),
            },
        },
        SessionEventKind::SessionEnded {
            status: "completed".to_string(),
        },
    ];
    for kind in &expected {
        handle
            .append_typed_event(kind.clone(), Some("1".to_string()), None)
            .expect("append typed event");
    }
    handle.flush_events().expect("flush");

    let record = store.show(handle.session_id()).expect("show");
    assert_eq!(record.events.len(), expected.len());
    for (event, expected_kind) in record.events.iter().zip(expected.iter()) {
        assert_eq!(event.kind, expected_kind.discriminator());
        let typed = SessionEventKind::try_from_event(event).expect("typed view");
        assert_eq!(&typed, expected_kind);
    }
}

#[test]
fn replay_resume_state_falls_back_when_resume_json_deleted() {
    // Acceptance: deleting `resume_state.json` and triggering the replay
    // fallback must reconstruct a functionally equivalent conversation
    // (same shape, same order) from the durable `events.jsonl` stream.
    let root = temp_root("replay-fallback-equivalent");
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

    // A full turn: user prompt -> tool call -> tool result -> assistant
    // reply. Each step uses the typed append API so the test also pins
    // the typed-producer surface alongside the replay round-trip.
    handle
        .append_typed_event(
            SessionEventKind::UserMessage {
                text: "investigate failure".to_string(),
            },
            None,
            None,
        )
        .expect("user message");
    handle
        .append_typed_event(
            SessionEventKind::ToolCall {
                call_id: "call-7".to_string(),
                tool: "read_file".to_string(),
                arguments: json!({"path": "src/lib.rs"}),
            },
            Some("1".to_string()),
            None,
        )
        .expect("tool call");
    handle
        .append_typed_event(
            SessionEventKind::ToolResult {
                output: json!({"call_id": "call-7", "output": "fn main() {}"}),
            },
            Some("1".to_string()),
            None,
        )
        .expect("tool result");
    handle
        .append_typed_event(
            SessionEventKind::AssistantCompleted {
                text: "see line 1".to_string(),
                response_id: Some("resp-7".to_string()),
            },
            Some("1".to_string()),
            None,
        )
        .expect("assistant completion");
    handle.flush_events().expect("flush events");

    let session_dir = store.root().join(handle.session_id());
    fs::remove_file(session_dir.join("resume_state.json")).expect("delete resume_state.json");
    assert!(handle.read_resume_state().is_err(), "sidecar must be gone");

    // Replay must rebuild the exact conversation shape from the durable
    // event log — items in the same order, with `FunctionCallOutput`
    // unwrapped from the `{call_id, output}` envelope the agent emits.
    let replayed = handle.replay_resume_state().expect("replay");
    assert!(replayed.resume_available);
    assert_eq!(
        replayed.conversation,
        vec![
            ResumeItem::UserText {
                text: "investigate failure".to_string()
            },
            ResumeItem::FunctionCall {
                call_id: "call-7".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "src/lib.rs"}),
            },
            ResumeItem::FunctionCallOutput {
                call_id: "call-7".to_string(),
                output: "fn main() {}".to_string(),
            },
            ResumeItem::AssistantText {
                text: "see line 1".to_string()
            },
        ],
    );
}

#[test]
fn replay_resume_state_round_trips_reasoning_items() {
    // Acceptance for squeezy-fp0: a session that records reasoning blobs
    // alongside user / assistant text must, after `resume_state.json` is
    // deleted, replay an identical conversation off the durable
    // `events.jsonl` stream — including the reasoning items, with their
    // provider-tagged ids and opaque content preserved bit-for-bit. Without
    // this, resuming a gpt-5.x or Claude thinking session loses the model's
    // prior chain-of-thought and the next turn has to re-derive (and re-bill
    // for) the same reasoning.
    let root = temp_root("replay-reasoning-round-trip");
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

    let openai_reasoning = ReasoningPayload::OpenAi {
        item_id: "rs_openai_42".to_string(),
        summary: vec!["Read the file before patching it.".to_string()],
        encrypted_content: Some("ENCRYPTED-OPENAI-BLOB".to_string()),
    };
    let anthropic_reasoning = ReasoningPayload::Anthropic {
        blocks: vec![AnthropicThinkingBlock {
            kind: AnthropicThinkingKind::Thinking,
            text: "First check the failing test.".to_string(),
            signature: Some("sig-anthropic".to_string()),
            data: None,
        }],
    };

    handle
        .append_typed_event(
            SessionEventKind::UserMessage {
                text: "why does the test fail?".to_string(),
            },
            None,
            None,
        )
        .expect("user message");
    handle
        .append_typed_event(
            SessionEventKind::Reasoning {
                payload: openai_reasoning.clone(),
            },
            Some("1".to_string()),
            None,
        )
        .expect("openai reasoning");
    handle
        .append_typed_event(
            SessionEventKind::Reasoning {
                payload: anthropic_reasoning.clone(),
            },
            Some("1".to_string()),
            None,
        )
        .expect("anthropic reasoning");
    handle
        .append_typed_event(
            SessionEventKind::AssistantCompleted {
                text: "the test asserts the wrong column".to_string(),
                response_id: Some("resp-reasoning-1".to_string()),
            },
            Some("1".to_string()),
            None,
        )
        .expect("assistant completion");
    handle.flush_events().expect("flush events");

    let session_dir = store.root().join(handle.session_id());
    fs::remove_file(session_dir.join("resume_state.json"))
        .expect("delete resume_state.json to force the events.jsonl fallback");

    let replayed = handle
        .replay_resume_state()
        .expect("replay reconstructs from events.jsonl");
    assert!(replayed.resume_available);
    assert_eq!(
        replayed.conversation,
        vec![
            ResumeItem::UserText {
                text: "why does the test fail?".to_string(),
            },
            ResumeItem::Reasoning {
                payload: openai_reasoning,
            },
            ResumeItem::Reasoning {
                payload: anthropic_reasoning,
            },
            ResumeItem::AssistantText {
                text: "the test asserts the wrong column".to_string(),
            },
        ],
        "reasoning items must round-trip with id and content preserved",
    );
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
    // Explicit `ids` for an already-archived session are a no-op: the
    // archive retention sweep is the only path that hard-deletes, and
    // the just-archived session has not yet aged past
    // `retention_archive_days` (default 30 days).
    assert!(
        !report.removed.contains(&session_id),
        "archived session must not be hard-deleted by cleanup",
    );
    assert!(
        !report.archived.contains(&session_id),
        "already-archived session must not be re-archived",
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
fn cleanup_deletes_archived_sessions_past_archive_retention() {
    let root = temp_root("archive-retention-deletes");
    let config = AppConfig {
        workspace_root: root.clone(),
        session_logs: SessionLogConfig {
            log_dir: Some(PathBuf::from(".squeezy/sessions")),
            // Live retention stays high so we exercise the archive
            // retention path in isolation: nothing is archived by the
            // sweep itself; we age the existing archive entry by hand.
            log_retention_days: 3650,
            log_retention_archive_days: 7,
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle.flush_events().expect("flush events");
    let session_id = handle.session_id().to_string();
    store.archive_session(&session_id).expect("archive");

    // Backdate the archived metadata so the archival timestamp sits well
    // outside the 7-day archive retention. The cleanup sweep prefers
    // `archived_at_ms`, with `ended_at_ms`/`started_at_ms` as fallbacks
    // for legacy metadata files that predate `archived_at_ms`.
    let archived_dir = store.root().join(super::ARCHIVED_SUBDIR).join(&session_id);
    let metadata_path = archived_dir.join("metadata.json");
    let text = fs::read_to_string(&metadata_path).expect("read archived metadata");
    let mut metadata: super::SessionMetadata =
        serde_json::from_str(&text).expect("parse archived metadata");
    metadata.started_at_ms = 1;
    metadata.ended_at_ms = Some(1);
    metadata.archived_at_ms = Some(1);
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).expect("serialize"),
    )
    .expect("write metadata");

    let report = store.cleanup(&[]).expect("cleanup archive-retention");
    assert_eq!(
        report.removed,
        vec![session_id.clone()],
        "expired archived session should be hard-deleted",
    );
    assert!(
        report.archived.is_empty(),
        "no live session should have been archived in this sweep",
    );
    assert!(
        !archived_dir.exists(),
        "expired archived session must be removed from disk",
    );
}

#[test]
fn cleanup_with_archive_retention_disabled_keeps_archived_sessions() {
    let root = temp_root("archive-retention-disabled");
    let config = AppConfig {
        workspace_root: root.clone(),
        session_logs: SessionLogConfig {
            log_dir: Some(PathBuf::from(".squeezy/sessions")),
            log_retention_days: 3650,
            // `0` disables the archive sweep entirely: archived
            // sessions linger until the user removes them by hand.
            log_retention_archive_days: 0,
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle.flush_events().expect("flush events");
    let session_id = handle.session_id().to_string();
    store.archive_session(&session_id).expect("archive");

    let archived_dir = store.root().join(super::ARCHIVED_SUBDIR).join(&session_id);
    let metadata_path = archived_dir.join("metadata.json");
    let text = fs::read_to_string(&metadata_path).expect("read archived metadata");
    let mut metadata: super::SessionMetadata =
        serde_json::from_str(&text).expect("parse archived metadata");
    metadata.started_at_ms = 1;
    metadata.ended_at_ms = Some(1);
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).expect("serialize"),
    )
    .expect("write metadata");

    let report = store.cleanup(&[]).expect("cleanup");
    assert!(
        report.removed.is_empty(),
        "archive sweep must be a no-op when retention_archive_days = 0",
    );
    assert!(
        archived_dir.exists(),
        "archived session must survive when the archive sweep is disabled",
    );
}

#[test]
fn remove_session_archives_live_session() {
    let (_root, store, config) = open_test_store("remove-session-archives");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle.flush_events().expect("flush events");
    let session_id = handle.session_id().to_string();
    // Drop the handle so the session log writer thread shuts down
    // before we move the directory out from under it.
    drop(handle);

    store
        .remove_session(&session_id)
        .expect("remove_session should soft-archive");

    assert!(
        !store.root().join(&session_id).exists(),
        "live directory must be empty after remove_session",
    );
    assert!(
        store
            .root()
            .join(super::ARCHIVED_SUBDIR)
            .join(&session_id)
            .exists(),
        "remove_session must move the directory under the archived/ subtree",
    );
}

#[test]
fn archived_session_remains_readable_via_show() {
    let (_root, store, config) = open_test_store("archived-readable");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle
        .append_event(SessionEvent::new(
            "user_message",
            None,
            Some("read me back after archive".to_string()),
            json!({"ok": true}),
        ))
        .expect("append event");
    handle.flush_events().expect("flush events");
    let session_id = handle.session_id().to_string();
    drop(handle);
    store.archive_session(&session_id).expect("archive");

    let record = store
        .show(&session_id)
        .expect("show should resolve an archived session");
    assert_eq!(record.metadata.session_id, session_id);
    assert_eq!(record.metadata.status, SessionStatus::Archived);
    assert!(
        record
            .events
            .iter()
            .any(|event| event.summary.as_deref() == Some("read me back after archive")),
        "events.jsonl content must round-trip through archive",
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
    assert!(
        found.archived_at_ms.is_none(),
        "unarchive must clear the lifecycle timestamp",
    );
}

/// Acceptance test for the lifecycle field extension from
/// `audits/opencode-comparison-2026-05-25/12-sessions-state-and-compaction.md#f12-session-archived-state`:
/// once a session is archived, it must disappear from the default list
/// surface so `bd ready`-style discovery does not pull stale history into
/// the user's working view.
#[test]
fn archived_session_excluded_from_list_default() {
    let (_root, store, config) = open_test_store("archived-excluded-default");
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
    assert!(
        found.archived_at_ms.is_some(),
        "archive_session must stamp the lifecycle timestamp",
    );
}

/// `cleanup_with(CleanupMode::Purge, …)` is the explicit "I want this
/// gone now" escape hatch from archive-by-default. Live sessions named
/// in `ids` skip the archive tree; archived sessions named in `ids` are
/// removed immediately rather than waiting for archive retention.
#[test]
fn cleanup_with_purge_hard_deletes_live_and_archived_sessions() {
    let (_root, store, config) = open_test_store("cleanup-purge");
    let live = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start live session");
    live.flush_events().expect("flush live events");
    let live_id = live.session_id().to_string();
    drop(live);

    let archived = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start archived session");
    archived.flush_events().expect("flush archived events");
    let archived_id = archived.session_id().to_string();
    drop(archived);
    store
        .archive_session(&archived_id)
        .expect("archive session");

    let report = store
        .cleanup_with(
            &[live_id.clone(), archived_id.clone()],
            None,
            super::CleanupMode::Purge,
        )
        .expect("cleanup purge");

    assert!(
        report.archived.is_empty(),
        "purge must not soft-archive: archived={:?}",
        report.archived,
    );
    assert!(
        report.removed.contains(&live_id),
        "purge must hard-delete the live session: removed={:?}",
        report.removed,
    );
    assert!(
        report.removed.contains(&archived_id),
        "purge must hard-delete the already-archived session: removed={:?}",
        report.removed,
    );
    assert!(
        !store.root().join(&live_id).exists(),
        "live directory must be gone after purge",
    );
    assert!(
        !store
            .root()
            .join(super::ARCHIVED_SUBDIR)
            .join(&archived_id)
            .exists(),
        "archived directory must be gone after purge",
    );
}

#[test]
fn bundle_rollout_trace_is_empty_when_no_logs_exist() {
    let (_root, store, config) = open_test_store("rollout-trace-empty");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    let bundle = store
        .bundle_rollout_trace(handle.session_id())
        .expect("bundle empty trace");
    assert!(bundle.is_empty());
}

#[test]
fn bundle_rollout_trace_preserves_event_order_when_no_replay() {
    let (_root, store, config) = open_test_store("rollout-trace-events-only");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle
        .append_event(SessionEvent::new(
            "user_message",
            Some("turn-1".to_string()),
            Some("find payment bug".to_string()),
            json!({}),
        ))
        .expect("append user");
    handle
        .append_event(SessionEvent::new(
            "assistant_completed",
            Some("turn-1".to_string()),
            Some("looking at payments".to_string()),
            json!({"response_id": "resp_1"}),
        ))
        .expect("append assistant");
    handle.flush_events().expect("flush events");

    let bundle = store
        .bundle_rollout_trace(handle.session_id())
        .expect("bundle");
    assert_eq!(bundle.len(), 2);
    assert!(
        bundle
            .iter()
            .all(|entry| entry.source == RolloutEventSource::Event)
    );
    assert!(
        bundle
            .iter()
            .all(|entry| entry.schema_version == ROLLOUT_TRACE_SCHEMA_VERSION)
    );
    assert_eq!(bundle[0].kind, "user_message");
    assert_eq!(bundle[1].kind, "assistant_completed");
    // Replay-side fields stay absent for event-sourced rows so consumers can
    // pattern-match on `source` without re-checking discriminators.
    assert!(bundle.iter().all(|entry| entry.replay_kind.is_none()));
    assert!(bundle.iter().all(|entry| entry.payload_sha256.is_none()));
    // Typed event_kind is populated when the discriminator matches.
    assert!(matches!(
        bundle[0].event_kind,
        Some(SessionEventKind::UserMessage { .. })
    ));
    assert!(matches!(
        bundle[1].event_kind,
        Some(SessionEventKind::AssistantCompleted { .. })
    ));
}

#[test]
fn bundle_rollout_trace_emits_replay_when_no_events() {
    let (_root, store, config) = open_test_store("rollout-trace-replay-only");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle
        .append_replay_event(SessionReplayEvent::new(
            SessionReplayEventKind::UserMessage,
            Some("turn-1".to_string()),
            json!({"input": "find a bug"}),
        ))
        .expect("append user replay");
    handle
        .append_replay_event(SessionReplayEvent::new(
            SessionReplayEventKind::ModelStarted,
            Some("turn-1".to_string()),
            json!({"model": "gpt-test"}),
        ))
        .expect("append model started");
    handle
        .append_replay_event(SessionReplayEvent::new(
            SessionReplayEventKind::ModelCompleted,
            Some("turn-1".to_string()),
            json!({"response_id": "resp_1"}),
        ))
        .expect("append model completed");

    let bundle = store
        .bundle_rollout_trace(handle.session_id())
        .expect("bundle");
    assert_eq!(bundle.len(), 3);
    assert!(
        bundle
            .iter()
            .all(|entry| entry.source == RolloutEventSource::Replay)
    );
    // Sequence is preserved end-to-end so a downstream reducer can pair
    // model_started/model_completed without re-deriving the order.
    let sequences: Vec<u64> = bundle.iter().map(|entry| entry.sequence).collect();
    assert_eq!(sequences, vec![1, 2, 3]);
    assert!(bundle.iter().all(|entry| {
        entry
            .payload_sha256
            .as_deref()
            .is_some_and(|d| !d.is_empty())
    }));
    assert!(matches!(
        bundle[0].replay_kind,
        Some(SessionReplayEventKind::UserMessage)
    ));
    assert!(matches!(
        bundle[2].replay_kind,
        Some(SessionReplayEventKind::ModelCompleted)
    ));
    // event_kind is left empty for replay-sourced rows.
    assert!(bundle.iter().all(|entry| entry.event_kind.is_none()));
}

#[test]
fn bundle_rollout_trace_merges_events_and_replay_by_timestamp() {
    let (_root, store, config) = open_test_store("rollout-trace-merge");
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    let session_dir = store.root().join(handle.session_id());

    // Fabricate both logs with controlled timestamps so the merge order is
    // deterministic — `now_ms()` resolution is one millisecond and a tight
    // sequence of appends would otherwise alias into the same bucket.
    let event_line = |event: &SessionEvent| {
        let mut bytes = serde_json::to_vec(event).expect("serialise event");
        bytes.push(b'\n');
        bytes
    };
    let events_path = session_dir.join("events.jsonl");
    let replay_path = session_dir.join("replay.jsonl");

    let mut events_jsonl = Vec::new();
    events_jsonl.extend(event_line(&SessionEvent {
        ts_unix_ms: 100,
        kind: "user_message".to_string(),
        turn_id: Some("turn-1".to_string()),
        summary: Some("find a bug".to_string()),
        payload: json!({"text": "find a bug"}),
    }));
    events_jsonl.extend(event_line(&SessionEvent {
        ts_unix_ms: 300,
        kind: "assistant_completed".to_string(),
        turn_id: Some("turn-1".to_string()),
        summary: Some("done".to_string()),
        payload: json!({"text": "done", "response_id": "resp_1"}),
    }));
    fs::write(&events_path, events_jsonl).expect("write events.jsonl");

    let replay_line = |event: &SessionReplayEvent| {
        let mut bytes = serde_json::to_vec(event).expect("serialise replay");
        bytes.push(b'\n');
        bytes
    };
    // Manually-stamped replay rows with controlled sequences. The constructor
    // would call `now_ms()` and overwrite sequence at append time, so we go
    // around it for deterministic ordering.
    let mut replay_jsonl = Vec::new();
    let replay_event_start = SessionReplayEvent {
        schema_version: SESSION_REPLAY_SCHEMA_VERSION,
        ts_unix_ms: 200,
        sequence: 1,
        kind: SessionReplayEventKind::ModelStarted,
        turn_id: Some("turn-1".to_string()),
        payload_sha256: String::new(),
        payload: json!({"model": "gpt-test"}),
    };
    let replay_event_start = SessionReplayEvent {
        payload_sha256: hash_payload(&replay_event_start.payload),
        ..replay_event_start
    };
    let replay_event_completed = SessionReplayEvent {
        schema_version: SESSION_REPLAY_SCHEMA_VERSION,
        ts_unix_ms: 300, // same ms as the assistant_completed event
        sequence: 2,
        kind: SessionReplayEventKind::ModelCompleted,
        turn_id: Some("turn-1".to_string()),
        payload_sha256: String::new(),
        payload: json!({"response_id": "resp_1"}),
    };
    let replay_event_completed = SessionReplayEvent {
        payload_sha256: hash_payload(&replay_event_completed.payload),
        ..replay_event_completed
    };
    replay_jsonl.extend(replay_line(&replay_event_start));
    replay_jsonl.extend(replay_line(&replay_event_completed));
    fs::write(&replay_path, replay_jsonl).expect("write replay.jsonl");

    let bundle = store
        .bundle_rollout_trace(handle.session_id())
        .expect("bundle merged trace");
    // Expected order:
    //   ts=100 Event user_message
    //   ts=200 Replay ModelStarted
    //   ts=300 Replay ModelCompleted (replay sorts before event on ties)
    //   ts=300 Event assistant_completed
    assert_eq!(bundle.len(), 4);
    assert_eq!(
        (bundle[0].source, bundle[0].kind.as_str()),
        (RolloutEventSource::Event, "user_message"),
    );
    assert_eq!(
        (bundle[1].source, bundle[1].kind.as_str()),
        (RolloutEventSource::Replay, "model_started"),
    );
    assert_eq!(
        (bundle[2].source, bundle[2].kind.as_str()),
        (RolloutEventSource::Replay, "model_completed"),
        "replay row wins ties within the same millisecond",
    );
    assert_eq!(
        (bundle[3].source, bundle[3].kind.as_str()),
        (RolloutEventSource::Event, "assistant_completed"),
    );
    // Timestamps are non-decreasing across the whole bundle.
    let timestamps: Vec<u64> = bundle.iter().map(|entry| entry.ts_unix_ms).collect();
    assert!(timestamps.windows(2).all(|pair| pair[0] <= pair[1]));
}

fn hash_payload(payload: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(payload).unwrap_or_default();
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// `HOME` is process-global; the memory tests below mutate it to point at a
// temp dir so the user's real `~/.squeezy/memory.md` is never touched. The
// lock keeps parallel test runners from racing on the env mutation.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_home<R>(home: &Path, body: impl FnOnce() -> R) -> R {
    let _guard = HOME_LOCK.lock().expect("HOME lock");
    let previous = std::env::var_os("HOME");
    // SAFETY: the lock above serialises HOME mutations across the suite.
    unsafe {
        std::env::set_var("HOME", home);
    }
    let result = body();
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    result
}

#[test]
fn remember_and_recall_round_trip_through_user_memory_file() {
    let home = temp_root("remember-recall-round-trip");
    with_home(&home, || {
        let initial = SessionStore::recall(8_192);
        assert!(initial.is_none(), "fresh HOME must surface no memory body");

        let written = SessionStore::remember("prefers cargo nextest over cargo test")
            .expect("remember preference");
        assert_eq!(
            written,
            "prefers cargo nextest over cargo test".len() + 1,
            "first remember writes body + trailing newline only"
        );

        let recalled = SessionStore::recall(8_192).expect("recall after remember");
        assert!(recalled.contains("prefers cargo nextest over cargo test"));
        assert!(
            recalled.ends_with('\n'),
            "memory body keeps trailing newline"
        );

        let written_again =
            SessionStore::remember("repo uses redb for the graph store").expect("second remember");
        assert_eq!(
            written_again,
            "repo uses redb for the graph store".len() + 1,
            "follow-up remember skips the leading newline when file already ends with one"
        );

        let body = SessionStore::recall(8_192).expect("recall after second remember");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines,
            vec![
                "prefers cargo nextest over cargo test",
                "repo uses redb for the graph store",
            ],
            "each remembered line lands on its own row"
        );
    });
}

#[test]
fn remember_inserts_leading_newline_when_file_lacks_one() {
    let home = temp_root("remember-fixup-trailing-newline");
    with_home(&home, || {
        let memory = SessionStore::memory_path().expect("HOME set");
        fs::create_dir_all(memory.parent().expect("memory dir")).expect("mkdir");
        fs::write(&memory, b"manual entry without newline").expect("seed memory.md");

        let written = SessionStore::remember("agent-added entry").expect("remember");
        // 1 leading newline + body + trailing newline.
        assert_eq!(written, 1 + "agent-added entry".len() + 1);

        let body = fs::read_to_string(&memory).expect("read");
        assert_eq!(
            body, "manual entry without newline\nagent-added entry\n",
            "leading newline appended when prior file lacked one",
        );
    });
}

#[test]
fn remember_trims_whitespace_and_drops_empty_input() {
    let home = temp_root("remember-trim-empty");
    with_home(&home, || {
        let zero = SessionStore::remember("   \n\t  ").expect("empty input is ok");
        assert_eq!(zero, 0, "blank input must not touch the file");
        let memory = SessionStore::memory_path().expect("HOME set");
        let initial_body = fs::read_to_string(&memory).unwrap_or_default();
        assert!(
            initial_body.is_empty(),
            "blank remember leaves memory.md absent or empty (got {initial_body:?})",
        );

        let written =
            SessionStore::remember("   keep the trimmed body  \n").expect("trim and append");
        assert_eq!(written, "keep the trimmed body".len() + 1);

        let body = fs::read_to_string(&memory).expect("read");
        assert_eq!(body, "keep the trimmed body\n");
    });
}

#[test]
fn recall_truncates_at_char_boundary_with_marker() {
    let home = temp_root("recall-truncates-at-boundary");
    with_home(&home, || {
        SessionStore::remember("héllo world — long enough to be truncated")
            .expect("remember unicode body");

        // Cap=2 lands inside 'é' (a 2-byte char that occupies bytes 1..3 in
        // the persisted body). The recall must back off to byte 1, which is
        // the boundary after 'h'.
        let recalled = SessionStore::recall(2).expect("recall capped");
        assert_eq!(
            recalled, "h\n[truncated]",
            "char-boundary backoff yields a valid prefix plus marker",
        );

        // Cap >= body length returns the full body untouched.
        let full = SessionStore::recall(8_192).expect("recall uncapped");
        assert!(!full.contains("[truncated]"));
        assert!(full.starts_with("héllo world"));
    });
}

#[test]
fn recall_returns_none_when_max_bytes_zero_or_missing() {
    let home = temp_root("recall-disabled-or-missing");
    with_home(&home, || {
        assert!(
            SessionStore::recall(0).is_none(),
            "max_bytes=0 disables recall",
        );
        assert!(
            SessionStore::recall(8_192).is_none(),
            "missing memory.md surfaces as None",
        );
    });
}

#[test]
fn memory_path_is_none_when_home_unset() {
    let _guard = HOME_LOCK.lock().expect("HOME lock");
    let previous = std::env::var_os("HOME");
    unsafe {
        std::env::remove_var("HOME");
    }
    let path = SessionStore::memory_path();
    let recall = SessionStore::recall(8_192);
    let remember = SessionStore::remember("unused");
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    assert!(path.is_none(), "HOME unset means no memory path");
    assert!(recall.is_none(), "HOME unset means no recall body");
    assert!(
        remember.is_err(),
        "remember fails loudly when HOME is unset"
    );
}

#[test]
fn global_index_aggregates_sessions_across_projects() {
    let home = temp_root("global-index-cross-project");
    let project_a = temp_root("global-index-project-a");
    let project_b = temp_root("global-index-project-b");
    with_home(&home, || {
        let config_a = AppConfig {
            workspace_root: project_a.clone(),
            session_logs: SessionLogConfig {
                log_dir: Some(PathBuf::from(".squeezy/sessions")),
                ..SessionLogConfig::default()
            },
            ..AppConfig::default()
        };
        let config_b = AppConfig {
            workspace_root: project_b.clone(),
            session_logs: SessionLogConfig {
                log_dir: Some(PathBuf::from(".squeezy/sessions")),
                ..SessionLogConfig::default()
            },
            ..AppConfig::default()
        };
        let store_a = SessionStore::open(&config_a);
        let store_b = SessionStore::open(&config_b);

        let mut meta_a = SessionMetadata::new(&config_a, "test-provider");
        meta_a.cwd = project_a.display().to_string();
        let handle_a = store_a.start_session(meta_a).expect("start project A");
        handle_a
            .append_event(SessionEvent::new(
                "user_message",
                None,
                Some("fix payment bug".to_string()),
                json!({}),
            ))
            .expect("append A");
        handle_a.flush_events().expect("flush A");

        let mut meta_b = SessionMetadata::new(&config_b, "test-provider");
        meta_b.cwd = project_b.display().to_string();
        let handle_b = store_b.start_session(meta_b).expect("start project B");
        handle_b
            .append_event(SessionEvent::new(
                "user_message",
                None,
                Some("rewrite cache layer".to_string()),
                json!({}),
            ))
            .expect("append B");
        handle_b.flush_events().expect("flush B");

        // Per-project listings stay scoped — A sees only A, B only B.
        let listed_a = store_a.list(&SessionQuery::default()).expect("list A");
        let listed_b = store_b.list(&SessionQuery::default()).expect("list B");
        assert_eq!(listed_a.len(), 1, "project A list is project-local");
        assert_eq!(listed_b.len(), 1, "project B list is project-local");

        // The global index lifts both sessions out of their per-project
        // session roots so a resume picker run from either cwd can see them.
        let global = SessionStore::list_global_index();
        let ids: BTreeSet<String> = global.iter().map(|e| e.session_id.clone()).collect();
        assert!(
            ids.contains(handle_a.session_id()),
            "project A session missing from global index: {ids:?}",
        );
        assert!(
            ids.contains(handle_b.session_id()),
            "project B session missing from global index: {ids:?}",
        );

        // Each entry retains its source cwd + title so the picker can
        // render the "(repo) prompt" hint without re-reading metadata.
        let entry_a = global
            .iter()
            .find(|e| e.session_id == handle_a.session_id())
            .expect("entry A");
        let entry_b = global
            .iter()
            .find(|e| e.session_id == handle_b.session_id())
            .expect("entry B");
        assert_eq!(entry_a.cwd, project_a.display().to_string());
        assert_eq!(entry_b.cwd, project_b.display().to_string());
        assert_eq!(entry_a.title.as_deref(), Some("fix payment bug"));
        assert_eq!(entry_b.title.as_deref(), Some("rewrite cache layer"));
        assert!(entry_a.resume_available);
        assert!(entry_b.resume_available);
    });
}

#[test]
fn global_index_dedupes_by_session_id_keeping_latest() {
    let home = temp_root("global-index-dedup");
    with_home(&home, || {
        let entry_v1 = GlobalSessionIndexEntry {
            session_id: "sess-1".to_string(),
            cwd: "/work/repo".to_string(),
            workspace_root: "/work/repo".to_string(),
            repo_root: None,
            title: Some("initial".to_string()),
            started_at_ms: 1_000,
            last_event_at_ms: 1_000,
            turn_count: 0,
            resume_available: true,
        };
        let entry_v2 = GlobalSessionIndexEntry {
            title: Some("after first prompt".to_string()),
            last_event_at_ms: 2_000,
            turn_count: 1,
            ..entry_v1.clone()
        };
        let entry_v3 = GlobalSessionIndexEntry {
            title: Some("session finished".to_string()),
            last_event_at_ms: 3_000,
            turn_count: 4,
            resume_available: false,
            ..entry_v1.clone()
        };

        SessionStore::append_global_index_entry(&entry_v1);
        SessionStore::append_global_index_entry(&entry_v2);
        SessionStore::append_global_index_entry(&entry_v3);

        let listed = SessionStore::list_global_index();
        assert_eq!(
            listed.len(),
            1,
            "duplicate session_id rows must collapse to one entry",
        );
        let entry = &listed[0];
        assert_eq!(entry.session_id, "sess-1");
        assert_eq!(
            entry.title.as_deref(),
            Some("session finished"),
            "latest last_event_at_ms wins",
        );
        assert_eq!(entry.turn_count, 4);
        assert!(
            !entry.resume_available,
            "terminal snapshot overrides earlier resume_available",
        );
    });
}

#[test]
fn global_index_compacts_when_file_grows_past_threshold() {
    let home = temp_root("global-index-compaction");
    with_home(&home, || {
        // Pad the title so every append crosses ~1.5KB on disk; with the
        // 256KiB threshold this is far less than 500 rewrites but still
        // exercises the rewrite branch deterministically.
        let payload = "x".repeat(2_000);
        for i in 0..200u32 {
            SessionStore::append_global_index_entry(&GlobalSessionIndexEntry {
                session_id: format!("sess-{i}"),
                cwd: format!("/work/project-{i}"),
                workspace_root: format!("/work/project-{i}"),
                repo_root: None,
                title: Some(payload.clone()),
                started_at_ms: i as u64,
                last_event_at_ms: i as u64,
                turn_count: 0,
                resume_available: true,
            });
        }
        // Duplicate a subset so compaction has something to coalesce.
        for i in 0..50u32 {
            SessionStore::append_global_index_entry(&GlobalSessionIndexEntry {
                session_id: format!("sess-{i}"),
                cwd: format!("/work/project-{i}"),
                workspace_root: format!("/work/project-{i}"),
                repo_root: None,
                title: Some(payload.clone()),
                started_at_ms: i as u64,
                last_event_at_ms: (i as u64) + 1_000,
                turn_count: 7,
                resume_available: true,
            });
        }

        let path = SessionStore::global_index_path().expect("HOME set");
        let size_before = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(
            size_before > GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES,
            "test setup must exceed compaction threshold (got {size_before} bytes)",
        );

        let listed = SessionStore::list_global_index();
        assert_eq!(listed.len(), 200, "dedupe keeps one entry per session_id");

        let size_after = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(
            size_after < size_before,
            "list_global_index must rewrite the file when it exceeds the threshold (before={size_before}, after={size_after})",
        );

        // The rewritten file is still a valid newline-delimited JSONL
        // stream that a second read sees identically — important because
        // the next session start will append straight to its tail.
        let again = SessionStore::list_global_index();
        let ids_again: BTreeSet<String> = again.iter().map(|e| e.session_id.clone()).collect();
        let ids_first: BTreeSet<String> = listed.iter().map(|e| e.session_id.clone()).collect();
        assert_eq!(ids_again, ids_first);
    });
}

#[test]
fn global_index_path_is_none_when_home_unset() {
    let _guard = HOME_LOCK.lock().expect("HOME lock");
    let previous = std::env::var_os("HOME");
    unsafe {
        std::env::remove_var("HOME");
    }
    let path = SessionStore::global_index_path();
    let listed = SessionStore::list_global_index();
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    assert!(path.is_none(), "HOME unset means no global index path");
    assert!(
        listed.is_empty(),
        "HOME unset surfaces no entries — the index is best-effort enrichment",
    );
}

#[test]
fn archive_session_marks_global_index_entry_unresumable() {
    let home = temp_root("global-index-archive");
    let project = temp_root("global-index-archive-project");
    with_home(&home, || {
        let config = AppConfig {
            workspace_root: project.clone(),
            session_logs: SessionLogConfig {
                log_dir: Some(PathBuf::from(".squeezy/sessions")),
                ..SessionLogConfig::default()
            },
            ..AppConfig::default()
        };
        let store = SessionStore::open(&config);
        let mut meta = SessionMetadata::new(&config, "test-provider");
        meta.cwd = project.display().to_string();
        let handle = store.start_session(meta).expect("start");
        let id = handle.session_id().to_string();

        let live = SessionStore::list_global_index();
        let live_entry = live
            .iter()
            .find(|e| e.session_id == id)
            .expect("entry recorded on start");
        assert!(
            live_entry.resume_available,
            "fresh session must be resumable",
        );

        // Drop the handle first so the async writer doesn't fight us
        // for the session dir during the rename.
        drop(handle);
        store.archive_session(&id).expect("archive");

        let archived = SessionStore::list_global_index();
        let entry = archived
            .iter()
            .find(|e| e.session_id == id)
            .expect("archived entry still visible");
        assert!(
            !entry.resume_available,
            "archive must flip resume_available off so the picker hides the entry",
        );
    });
}
