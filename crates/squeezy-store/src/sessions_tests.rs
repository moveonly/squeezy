use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    time::Duration,
};

use redb::{Database, TableDefinition};
use serde_json::json;
use squeezy_core::{
    AnthropicThinkingBlock, AnthropicThinkingKind, AppConfig, ContextAttachment,
    ContextAttachmentKind, ContextAttachmentSource, ContextAttachmentStatus,
    ContextCompactionRecord, ContextCompactionState, ContextCompactionTrigger, ContextEstimate,
    ContextPin, CostSnapshot, FileId, ReasoningPayload, SessionLogConfig, SessionMetrics,
};

use crate::{
    BugReportOptions, GraphStore, GraphStoreMetadata, Observation, ObservationKind, SqueezyStore,
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    // Lazy materialisation defers the on-disk session dir until a
    // substantive event arrives. Seed one so the test can overwrite
    // events.jsonl with a malformed line afterwards.
    materialise_session(&handle);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
fn open_session_seeds_replay_sequence_from_existing_jsonl() {
    let root = temp_root("replay-open-count");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
            SessionReplayEventKind::ModelStarted,
            Some("1".to_string()),
            json!({"model": "test-model"}),
        ))
        .expect("append start");

    let session_id = handle.session_id().to_string();
    let reopened = store.open_session(session_id.clone());
    reopened
        .append_replay_event(SessionReplayEvent::new(
            SessionReplayEventKind::ModelCompleted,
            Some("1".to_string()),
            json!({"response_id": "resp_1", "cost": CostSnapshot::default()}),
        ))
        .expect("append completion after open");

    let tape = store.replay_tape(&session_id).expect("read replay");
    let sequences: Vec<u64> = tape.events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, vec![1, 2, 3]);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    materialise_session(&handle);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        image_media_type: None,
        image_data_base64: None,
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
    // Lazy materialisation: a freshly started session is in-memory only,
    // so metadata.json must not exist until the first substantive event
    // arrives.
    assert!(
        !metadata_path.exists(),
        "metadata.json must stay absent until a substantive event arrives",
    );

    // The first substantive event materialises the on-disk session
    // artefacts (creates the dir, writes metadata.json + resume_state.json,
    // spawns the events.jsonl writer thread).
    handle
        .append_event(SessionEvent::new(
            "tool_call",
            Some("1".to_string()),
            Some("tool 0".to_string()),
            json!({"index": 0}),
        ))
        .expect("first substantive event must materialise");
    assert!(
        metadata_path.exists(),
        "metadata.json must be written when the first substantive event arrives",
    );

    // A burst of further routine events (the kinds the agent emits per
    // tool call / tool result / approval round trip) must not touch
    // metadata.json. Many filesystems, including APFS on macOS, have
    // second-granularity mtimes, so we rely on a content hash plus a
    // file-size check instead of trying to race the clock.
    let before_bytes = fs::read(&metadata_path).expect("read metadata before");
    for index in 1..20 {
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
fn over_cap_appends_rewrite_metadata_only_once() {
    let root = temp_root("metadata-truncate-write-amplification");
    let config = AppConfig {
        workspace_root: root.clone(),
        session_logs: SessionLogConfig {
            // A few bytes: the first substantive event already crosses the
            // cap, so every later append takes the over-cap branch.
            max_session_bytes: 4,
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    let metadata_path = store.root().join(handle.session_id()).join("metadata.json");

    // Cross the cap once, then flush so the writer thread has flipped
    // metadata.json to the `Truncated` state on disk.
    handle
        .append_event(SessionEvent::new(
            "tool_call",
            Some("1".to_string()),
            Some("tool 0".to_string()),
            json!({"index": 0}),
        ))
        .expect("append over-cap event");
    handle.flush_events().expect("flush events");
    let truncated = handle.metadata().expect("read metadata");
    assert_eq!(truncated.status, SessionStatus::Truncated);

    // Plant a sentinel directly on disk: a `Running` status that the writer
    // would never produce. The truncation closure only ever writes
    // `Truncated`, so if a later over-cap append re-runs it, this sentinel
    // gets clobbered. If the writer short-circuits after recording the
    // transition once, the sentinel survives. This is a content check rather
    // than an mtime check because many filesystems have second-granularity
    // mtimes that a fast test cannot race.
    let mut sentinel = read_session_metadata(&metadata_path).expect("read metadata");
    sentinel.status = SessionStatus::Running;
    write_json(&metadata_path, &sentinel).expect("write sentinel metadata");

    for index in 1..20 {
        handle
            .append_event(SessionEvent::new(
                "tool_call",
                Some("1".to_string()),
                Some(format!("tool {index}")),
                json!({"index": index}),
            ))
            .expect("append over-cap event");
    }
    handle.flush_events().expect("flush events");

    let after = read_session_metadata(&metadata_path).expect("read metadata after further appends");
    assert_eq!(
        after.status,
        SessionStatus::Running,
        "over-cap appends must not rewrite metadata.json after truncation is recorded",
    );
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
            .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
            .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start running");
    // Both sessions need to be materialised on disk so the retention
    // sweep can find them by reading `metadata.json`. Lazy materialisation
    // means we have to drive at least one substantive event through each
    // handle before backdating the on-disk timestamp.
    materialise_session(&running);
    // Forge an ancient start time. A retention sweep would normally pick
    // this up, but the session is still Running so cleanup must skip it.
    running
        .update_metadata(|metadata| {
            metadata.started_at_ms = 1;
        })
        .expect("backdate running session");
    let completed = store
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start completed");
    materialise_session(&completed);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start protected");
    let collateral = store
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start collateral");
    // Lazy materialisation means cleanup_excluding cannot find a session
    // until it has materialised to disk. Seed both with a substantive
    // event so the explicit-ids sweep can target them.
    materialise_session(&protected);
    materialise_session(&collateral);

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
fn state_and_graph_stores_round_trip_split_cache_data() {
    let root = temp_root("state-round-trip");
    let store = SqueezyStore::open(&root, None).expect("open store");
    let graph_store = GraphStore::open(&root, None).expect("open graph store");

    let metadata = GraphStoreMetadata {
        workspace_root: root.display().to_string(),
        crawl_options_hash: "crawl".to_string(),
        language_registry_version: "langs".to_string(),
        graph_format_version: 1,
    };
    graph_store
        .set_graph_metadata(&metadata)
        .expect("set metadata");
    assert_eq!(
        graph_store.graph_metadata().expect("metadata"),
        Some(metadata)
    );

    let file_id = FileId::new("src/lib.rs");
    graph_store
        .put_graph_partition(&file_id, &serde_json::json!({"hash": "abc"}))
        .expect("put partition");
    let partition: serde_json::Value = graph_store
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
    let nul_suffixed_path = StoredReadSnapshot {
        path: "src/lib.rs\0generated".to_string(),
        tool_name: "read_slice".to_string(),
        call_id: "read_3".to_string(),
        stable_output_sha256: "read-out-3".to_string(),
        content_sha256: Some("generated-content".to_string()),
        start_byte: 0,
        end_byte: 4,
        content: "fake".to_string(),
        model_output_bytes: 64,
        created_unix_millis: 4,
    };
    store
        .put_read_snapshot(&nul_suffixed_path)
        .expect("put similar path snapshot");
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
        store.tool_receipts().expect("receipts"),
        Vec::<StoredToolReceipt>::new(),
        "new state store should open after the old schema is backed up"
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
    // metadata.json and read back via `show`. Materialise the session
    // before dropping the handle so the on-disk metadata snapshot is
    // visible to `SessionStore::show`.
    let mut metadata = SessionMetadata::new(&config, "openai");
    metadata.token_calibration = calibration.clone();
    let handle = store.start_session(metadata).expect("start session");
    materialise_session(&handle);
    let session_id = handle.session_id().to_string();
    drop(handle);
    let record = store.show(&session_id).expect("show session");
    assert_eq!(record.metadata.token_calibration, calibration);
}

#[test]
fn session_metadata_v0_file_reads_as_v1() {
    // Pre-versioning `metadata.json` files are missing the
    // `schema_version` field. The reader migration framework must treat
    // them as v0, run any registered v0 -> v1 migrations, and stamp
    // SESSION_METADATA_SCHEMA_VERSION onto the deserialized struct so
    // the rest of the binary sees a fully migrated value. Sibling
    // fields must survive untouched.
    let (_root, store, _) = open_test_store("metadata-v0-reads-as-v1");
    let session_dir = store.root().join("v0-session");
    fs::create_dir_all(&session_dir).expect("create session dir");
    let v0_json = json!({
        "session_id": "v0-session",
        "started_at_ms": 1_700_000_000_000_u64,
        "ended_at_ms": null,
        "cwd": "/tmp/work",
        "workspace_root": "/tmp/work",
        "repo_root": null,
        "branch": null,
        "provider": "openai",
        "model": "test-model",
        "mode": "build",
        "status": "completed",
        "first_user_task": "hello",
        "latest_summary": null,
        "cost": CostSnapshot::default(),
        "metrics": SessionMetrics::default(),
        "redactions": 0,
        "resume_available": false,
        "resume_unavailable_reason": null,
        "event_count": 7,
        // intentionally missing: schema_version
    });
    assert!(
        v0_json.get("schema_version").is_none(),
        "v0 fixture must omit schema_version",
    );
    fs::write(
        session_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&v0_json).expect("encode v0 json"),
    )
    .expect("write v0 metadata.json");

    let record = store.show("v0-session").expect("show v0 session");
    assert_eq!(
        record.metadata.schema_version, SESSION_METADATA_SCHEMA_VERSION,
        "migration must stamp the current version onto v0 payloads"
    );
    assert_eq!(record.metadata.session_id, "v0-session");
    assert_eq!(record.metadata.first_user_task.as_deref(), Some("hello"));
    assert_eq!(record.metadata.event_count, 7);
    assert_eq!(record.metadata.provider, "openai");
    assert_eq!(record.metadata.model, "test-model");
}

#[test]
fn session_metadata_v1_file_reads_unchanged() {
    // A `metadata.json` already written at the current schema version
    // must round-trip without the migration chain mutating its payload:
    // `apply_session_metadata_migrations` short-circuits when the
    // incoming version already matches SESSION_METADATA_SCHEMA_VERSION,
    // so deserialization sees the on-disk bytes as-is.
    let (_root, store, config) = open_test_store("metadata-v1-reads-unchanged");
    let session_dir = store.root().join("v1-session");
    fs::create_dir_all(&session_dir).expect("create session dir");
    let mut metadata = SessionMetadata::new(&config, "openai");
    metadata.session_id = "v1-session".to_string();
    metadata.started_at_ms = 1_700_000_100_000;
    metadata.first_user_task = Some("plan the rollout".to_string());
    metadata.event_count = 3;
    assert_eq!(metadata.schema_version, SESSION_METADATA_SCHEMA_VERSION);
    fs::write(
        session_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata).expect("encode v1"),
    )
    .expect("write v1 metadata.json");

    let record = store.show("v1-session").expect("show v1 session");
    assert_eq!(
        record.metadata.schema_version,
        SESSION_METADATA_SCHEMA_VERSION
    );
    assert_eq!(record.metadata.session_id, metadata.session_id);
    assert_eq!(record.metadata.first_user_task, metadata.first_user_task);
    assert_eq!(record.metadata.event_count, metadata.event_count);
    assert_eq!(record.metadata.started_at_ms, metadata.started_at_ms);
}

fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("squeezy-store-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
}

/// Force a pending session to materialise its on-disk artefacts so a
/// test that exercises archive/cleanup/show semantics can rely on a
/// real `metadata.json` + `events.jsonl` pair existing.
///
/// Production callers materialise implicitly the first time a
/// substantive event lands on the handle — this helper mirrors that by
/// queueing a single `tool_call` and waiting for the writer to drain.
fn materialise_session(handle: &SessionHandle) {
    handle
        .append_event(SessionEvent::new(
            "tool_call",
            None,
            Some("test materialise".to_string()),
            json!({}),
        ))
        .expect("seed materialising event");
    handle.flush_events().expect("flush seed event");
}

/// Acceptance for `F12-pi-lazy-session-file-creation`: a freshly
/// started session must not leave any on-disk artefact behind until a
/// substantive event arrives. Quick-exit code paths such as `squeezy
/// --prompt --help` build an `Agent` and tear it down before any user
/// turn runs, so the only events that ever reach the handle are
/// lifecycle markers like `session_started`. Those must stay
/// in-memory.
#[test]
fn start_session_does_not_create_disk_artefacts_until_first_substantive_event() {
    let root = temp_root("lazy-no-disk-until-substantive");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    let session_dir = store.root().join(handle.session_id());

    assert!(
        !session_dir.exists(),
        "start_session must not create the on-disk session directory",
    );

    // The in-memory metadata view still works while the session stays
    // pending — `metadata()` falls back to the cached snapshot rather
    // than reading a non-existent file.
    let metadata = handle.metadata().expect("metadata while pending");
    assert_eq!(metadata.session_id, handle.session_id());
    assert_eq!(metadata.status, SessionStatus::Running);
    assert!(metadata.resume_available);

    // A lifecycle-only event (the agent emits one of these right after
    // `start_session_log`) is buffered in memory and must not trigger
    // materialisation: a `squeezy --prompt --help` that exits before
    // any real turn would otherwise leave a stub session dir behind.
    handle
        .append_event(SessionEvent::new(
            "session_started",
            None,
            Some("session started".to_string()),
            json!({}),
        ))
        .expect("buffered lifecycle event");
    assert!(
        !session_dir.exists(),
        "lifecycle-only events must not materialise the session directory",
    );

    // flush_events on a pending session is a no-op; it must not race a
    // lazy materialisation by sneaking the dir into existence.
    handle.flush_events().expect("flush while pending");
    assert!(
        !session_dir.exists(),
        "flush_events on a pending session must remain a no-op",
    );

    // The first substantive event promotes the session: dir,
    // metadata.json, and resume_state.json all appear.
    handle
        .append_event(SessionEvent::new(
            "user_message",
            None,
            Some("first task".to_string()),
            json!({}),
        ))
        .expect("first substantive event must materialise");
    handle.flush_events().expect("flush after promotion");

    assert!(
        session_dir.exists(),
        "session dir must exist after promotion"
    );
    assert!(
        session_dir.join("metadata.json").exists(),
        "metadata.json must exist after promotion",
    );
    assert!(
        session_dir.join("resume_state.json").exists(),
        "resume_state.json must exist after promotion",
    );

    // The buffered lifecycle event lands in events.jsonl ahead of the
    // substantive event that triggered promotion, preserving the
    // arrival order the caller asked for.
    let record = store.show(handle.session_id()).expect("show");
    assert_eq!(
        record
            .events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["session_started", "user_message"],
        "buffered lifecycle event must be replayed before the promoting event",
    );
    assert_eq!(
        record.metadata.first_user_task.as_deref(),
        Some("first task")
    );
}

/// A session that never sees a substantive event must leave no on-disk
/// trace when its handle drops — even if the agent appended lifecycle
/// events and called `finish` on it (mirroring the `--prompt --help`
/// shutdown path).
#[test]
fn pending_session_with_only_lifecycle_events_leaves_no_disk_dir_on_drop() {
    let root = temp_root("lazy-drop-no-disk");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let session_id = {
        let handle = store
            .start_session(SessionMetadata::new(&config, "test-provider"))
            .expect("start session");
        handle
            .append_event(SessionEvent::new(
                "session_started",
                None,
                Some("session started".to_string()),
                json!({}),
            ))
            .expect("buffered session_started");
        handle
            .finish(
                SessionStatus::Completed,
                CostSnapshot::default(),
                SessionMetrics::default(),
                0,
            )
            .expect("finish while still pending");
        handle.session_id().to_string()
    };
    let session_dir = store.root().join(&session_id);
    assert!(
        !session_dir.exists(),
        "a session that never saw a substantive event must leave no on-disk dir",
    );
    let listed = store
        .list(&SessionQuery::default())
        .expect("list after pending finish");
    assert!(
        listed
            .iter()
            .all(|metadata| metadata.session_id != session_id),
        "pending-only sessions must not appear in the on-disk listing",
    );
}

/// `SessionStore::show` is the read surface the CLI calls to inspect a
/// session by id. When asked for a session that was started but never
/// materialised (no `metadata.json` on disk), it must surface a clean
/// "not found" error rather than a raw IO failure.
#[test]
fn show_returns_clean_not_found_for_unmaterialised_session_id() {
    let root = temp_root("lazy-show-not-found");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    let session_id = handle.session_id().to_string();
    let error = store
        .show(&session_id)
        .expect_err("show must error on a pending-only session");
    let message = error.to_string();
    assert!(
        message.contains("not found"),
        "show should mention not-found semantics, got: {message}",
    );
    assert!(
        message.contains(&session_id),
        "show should mention the session id, got: {message}",
    );
}

/// `write_resume_state` is an explicit "make my checkpoint durable"
/// call. It must promote a pending session to its on-disk form so the
/// fork path (and any other caller that explicitly persists resume
/// state) sees a real session directory afterwards.
#[test]
fn write_resume_state_materialises_pending_session() {
    let root = temp_root("lazy-write-resume-materialises");
    let config = AppConfig {
        workspace_root: root.clone(),
        ..AppConfig::default()
    };
    let store = SessionStore::open(&config);
    let handle = store
        .start_session(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    let session_dir = store.root().join(handle.session_id());
    assert!(!session_dir.exists());

    handle
        .write_resume_state(&SessionResumeState {
            resume_available: true,
            conversation: vec![ResumeItem::UserText {
                text: "carry me forward".to_string(),
            }],
            ..SessionResumeState::default()
        })
        .expect("write resume promotes");

    assert!(
        session_dir.join("metadata.json").exists(),
        "metadata.json must exist after write_resume_state",
    );
    let resume: SessionResumeState =
        serde_json::from_str(&fs::read_to_string(session_dir.join("resume_state.json")).unwrap())
            .expect("parse resume");
    assert!(resume.resume_available);
    assert_eq!(resume.conversation.len(), 1);
}

#[test]
fn routing_session_disabled_defaults_false_for_old_resume_state() {
    let resume: SessionResumeState = serde_json::from_str(
        r#"{
          "resume_available": true,
          "previous_response_id": null,
          "conversation": [],
          "transcript": []
        }"#,
    )
    .expect("old resume json");

    assert!(!resume.routing_session_disabled);
    assert!(!resume.routing_prior_turn_was_hard);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
fn replay_keeps_post_compact_base_and_skips_dropped_turns() {
    // Producer/consumer contract: the `context_compacted` event's
    // `conversation` field carries the *post-compact* base (summary head
    // + kept recent items), not the dropped slice. `replay_resume_state`
    // must snap to that base, dropping older pre-compact turns and
    // forward-replaying only strictly-newer events. This pins the fix
    // for `squeezy-bgc` (wave-1 critical): a buggy producer that wrote
    // the dropped slice into `conversation` resurrected older turns and
    // silently lost the kept ones on resume.
    let root = temp_root("replay-post-compact-base");
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");

    // Pre-compact turns 1+2 (these are the items the compaction will
    // drop). They live in `events.jsonl` before the compaction event;
    // the snap-to-checkpoint replay must skip them entirely.
    for (user, assistant) in [
        ("turn1 user prompt", "turn1 assistant reply"),
        ("turn2 user prompt", "turn2 assistant reply"),
    ] {
        handle
            .append_event(SessionEvent::new(
                "user_message",
                None,
                Some(user.to_string()),
                json!({}),
            ))
            .expect("append pre-compact user");
        handle
            .append_event(SessionEvent::new(
                "assistant_completed",
                None,
                Some(assistant.to_string()),
                json!({}),
            ))
            .expect("append pre-compact assistant");
    }
    // Pre-compact turns 3+4 (these are the items the compaction keeps).
    // They live in `events.jsonl` before the compaction event too, but
    // are subsumed by the checkpoint snapshot — the checkpoint already
    // carries them, so the snap-to-checkpoint replay must not double-
    // insert them via linear forward replay.
    for (user, assistant) in [
        ("turn3 user prompt", "turn3 assistant reply"),
        ("turn4 user prompt", "turn4 assistant reply"),
    ] {
        handle
            .append_event(SessionEvent::new(
                "user_message",
                None,
                Some(user.to_string()),
                json!({}),
            ))
            .expect("append kept user");
        handle
            .append_event(SessionEvent::new(
                "assistant_completed",
                None,
                Some(assistant.to_string()),
                json!({}),
            ))
            .expect("append kept assistant");
    }

    // Post-compact base = summary head + kept turns 3+4. This is what
    // the producer now stamps into the `conversation` field after the
    // fix; pre-fix code wrote the dropped (turn1+2) slice here.
    let post_compact_base: Vec<ResumeItem> = vec![
        ResumeItem::UserText {
            text: "<compaction summary head>".to_string(),
        },
        ResumeItem::UserText {
            text: "turn3 user prompt".to_string(),
        },
        ResumeItem::AssistantText {
            text: "turn3 assistant reply".to_string(),
        },
        ResumeItem::UserText {
            text: "turn4 user prompt".to_string(),
        },
        ResumeItem::AssistantText {
            text: "turn4 assistant reply".to_string(),
        },
    ];
    handle
        .append_event(SessionEvent::new(
            "context_compacted",
            None,
            Some("compacted".to_string()),
            json!({
                "record": null,
                "summary": "<compaction summary head>",
                "replacement_id": "ckpt-1",
                "conversation": post_compact_base.clone(),
            }),
        ))
        .expect("append context_compacted");
    // Turn 5: a post-compact user prompt + assistant reply that must be
    // forward-replayed on top of the checkpoint.
    handle
        .append_event(SessionEvent::new(
            "user_message",
            None,
            Some("turn5 user prompt".to_string()),
            json!({}),
        ))
        .expect("append post-compact user");
    handle
        .append_event(SessionEvent::new(
            "assistant_completed",
            None,
            Some("turn5 assistant reply".to_string()),
            json!({}),
        ))
        .expect("append post-compact assistant");
    handle.flush_events().expect("flush events");

    let replayed = handle.replay_resume_state().expect("replay");
    assert!(replayed.resume_available);

    // The dropped slice (turn1+2) must not resurface.
    for ghost in [
        "turn1 user prompt",
        "turn1 assistant reply",
        "turn2 user prompt",
        "turn2 assistant reply",
    ] {
        assert!(
            !replayed.conversation.iter().any(|item| match item {
                ResumeItem::UserText { text } | ResumeItem::AssistantText { text } => text == ghost,
                _ => false,
            }),
            "dropped turn {ghost:?} must not appear in replay; got {:?}",
            replayed.conversation,
        );
    }

    // Summary head + kept turns 3+4 must be present, in order, with no
    // duplicates (the linear-replay pass must skip events at or before
    // the checkpoint index).
    let expected_prefix: Vec<&str> = vec![
        "<compaction summary head>",
        "turn3 user prompt",
        "turn3 assistant reply",
        "turn4 user prompt",
        "turn4 assistant reply",
        "turn5 user prompt",
        "turn5 assistant reply",
    ];
    let actual_texts: Vec<String> = replayed
        .conversation
        .iter()
        .map(|item| match item {
            ResumeItem::UserText { text } | ResumeItem::AssistantText { text } => text.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        actual_texts,
        expected_prefix
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        "replay must equal post-compact base + strictly-newer events",
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
    // The replayed transcript (the UI hydration path) must surface the
    // reasoning attached to the assistant message that immediately
    // follows it via `TranscriptItem.reasoning`. Without this the TUI
    // resume flow rebuilds the screen as user → assistant with no
    // reasoning chip, which is the user-visible "resumed session lost
    // my reasoning" regression.
    assert_eq!(replayed.transcript.len(), 2, "user + assistant");
    let assistant = &replayed.transcript[1];
    assert_eq!(assistant.role, squeezy_core::Role::Assistant);
    let attached = assistant
        .reasoning
        .as_deref()
        .expect("assistant message must carry buffered reasoning after resume hydration");
    assert!(
        attached
            .display_text
            .contains("Read the file before patching it."),
        "first reasoning segment must be preserved in the chip body, got {:?}",
        attached.display_text,
    );
    assert!(
        attached
            .display_text
            .contains("First check the failing test."),
        "second reasoning segment must also be preserved, got {:?}",
        attached.display_text,
    );
    assert!(
        matches!(attached.payload, ReasoningPayload::Anthropic { .. }),
        "the last segment's payload metadata wins so per-provider fields stay consistent",
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    materialise_session(&handle);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    materialise_session(&handle);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    materialise_session(&handle);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    materialise_session(&handle);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    materialise_session(&handle);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    materialise_session(&handle);
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

/// Acceptance test for the session-archived lifecycle field: once a
/// session is archived, it must disappear from the default list surface
/// so the next list query does not pull stale history into the user's
/// working view.
#[test]
fn archived_session_excluded_from_list_default() {
    let (_root, store, config) = open_test_store("archived-excluded-default");
    let handle = store
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    materialise_session(&handle);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start live session");
    materialise_session(&live);
    let live_id = live.session_id().to_string();
    drop(live);

    let archived = store
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start archived session");
    materialise_session(&archived);
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    // The test fabricates events.jsonl + replay.jsonl with fixed
    // timestamps by writing the files directly. Lazy materialisation
    // would otherwise leave the session dir absent, so seed a single
    // event to force the directory + writer into existence before we
    // overwrite both logs.
    materialise_session(&handle);
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
        parent_event_sequence: None,
    }));
    events_jsonl.extend(event_line(&SessionEvent {
        ts_unix_ms: 300,
        kind: "assistant_completed".to_string(),
        turn_id: Some("turn-1".to_string()),
        summary: Some("done".to_string()),
        payload: json!({"text": "done", "response_id": "resp_1"}),
        parent_event_sequence: None,
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

fn raw_event(
    ts_unix_ms: u64,
    kind: &str,
    summary: Option<&str>,
    parent_event_sequence: Option<u64>,
) -> SessionEvent {
    SessionEvent {
        ts_unix_ms,
        kind: kind.to_string(),
        turn_id: None,
        summary: summary.map(str::to_string),
        payload: json!({}),
        parent_event_sequence,
    }
}

#[test]
fn session_event_omits_parent_when_none_on_serialise() {
    let event = SessionEvent::new("user_message", None, Some("hi".to_string()), json!({}));
    let body = serde_json::to_string(&event).expect("serialise");
    assert!(
        !body.contains("parent_event_sequence"),
        "linear events stay byte-identical: {body}",
    );
}

#[test]
fn session_event_round_trips_parent_event_sequence() {
    let event = SessionEvent::new("user_message", None, Some("retry".to_string()), json!({}))
        .with_parent_event_sequence(2);
    let body = serde_json::to_string(&event).expect("serialise");
    assert!(body.contains("parent_event_sequence"));
    let parsed: SessionEvent = serde_json::from_str(&body).expect("deserialise");
    assert_eq!(parsed.parent_event_sequence, Some(2));
}

#[test]
fn session_event_deserialises_legacy_payload_without_parent_field() {
    let legacy = json!({
        "ts_unix_ms": 1_700_000_000_000_u64,
        "kind": "user_message",
        "turn_id": null,
        "summary": "legacy",
        "payload": {"text": "legacy"},
    });
    let event: SessionEvent = serde_json::from_value(legacy).expect("legacy deserialise");
    assert_eq!(event.parent_event_sequence, None);
}

#[test]
fn detect_branches_returns_empty_for_linear_log() {
    let events = vec![
        raw_event(10, "user_message", Some("q1"), None),
        raw_event(20, "assistant_completed", Some("a1"), None),
        raw_event(30, "user_message", Some("q2"), None),
        raw_event(40, "assistant_completed", Some("a2"), None),
    ];
    assert!(detect_branches(&events).is_empty());
}

#[test]
fn detect_branches_returns_empty_for_single_or_zero_events() {
    assert!(detect_branches(&[]).is_empty());
    let one = vec![raw_event(10, "user_message", Some("q1"), None)];
    assert!(detect_branches(&one).is_empty());
}

#[test]
fn detect_branches_finds_both_paths_when_user_reprompts() {
    // Tree (linear unless noted):
    //   0 user "q1"
    //   1 assistant "a1"
    //   2 user "q2 (path A)"      -> linear child of 1
    //   3 assistant "a2-A"
    //   4 user "q2 (path B)"      -> branched off 1 (re-prompt)
    //   5 assistant "a2-B"
    let events = vec![
        raw_event(10, "user_message", Some("q1"), None),
        raw_event(20, "assistant_completed", Some("a1"), None),
        raw_event(30, "user_message", Some("q2 path A"), None),
        raw_event(40, "assistant_completed", Some("a2 A"), None),
        raw_event(50, "user_message", Some("q2 path B"), Some(1)),
        raw_event(60, "assistant_completed", Some("a2 B"), None),
    ];

    let tips = detect_branches(&events);
    assert_eq!(tips.len(), 2, "two leaves expected: {tips:?}");

    // Newest tip first.
    let tip_b = &tips[0];
    let tip_a = &tips[1];
    assert_eq!(tip_b.tip_sequence, 5);
    assert_eq!(tip_b.branched_from_sequence, 1);
    assert_eq!(
        tip_b.first_message_after_branch.as_deref(),
        Some("q2 path B"),
    );
    assert_eq!(tip_a.tip_sequence, 3);
    assert_eq!(tip_a.branched_from_sequence, 1);
    assert_eq!(
        tip_a.first_message_after_branch.as_deref(),
        Some("q2 path A"),
    );
}

#[test]
fn detect_branches_handles_three_way_fork() {
    // 0 -> 1 -> { 2 (linear), 3 (branch), 4 (branch) }
    let events = vec![
        raw_event(10, "user_message", Some("root"), None),
        raw_event(20, "assistant_completed", Some("a1"), None),
        raw_event(30, "user_message", Some("path-1"), None),
        raw_event(31, "user_message", Some("path-2"), Some(1)),
        raw_event(32, "user_message", Some("path-3"), Some(1)),
    ];
    let tips = detect_branches(&events);
    assert_eq!(tips.len(), 3);
    let sequences: Vec<u64> = tips.iter().map(|t| t.tip_sequence).collect();
    assert!(sequences.contains(&2));
    assert!(sequences.contains(&3));
    assert!(sequences.contains(&4));
    for tip in &tips {
        assert_eq!(tip.branched_from_sequence, 1);
    }
}

#[test]
fn detect_branches_ignores_self_or_out_of_range_parent() {
    // Self-parent and a forward-pointing parent are both treated as the
    // implicit linear parent so a malformed log still produces a sane
    // tree (and no spurious "branch").
    let events = vec![
        raw_event(10, "user_message", Some("q1"), None),
        raw_event(20, "assistant_completed", Some("a1"), Some(1)),
        raw_event(30, "user_message", Some("q2"), Some(99)),
    ];
    assert!(detect_branches(&events).is_empty());
}

/// Seed a session directory under the store root without going through
/// `start_session`. The resolver only enumerates directory names, so we
/// don't need a full event log — this keeps the prefix tests independent
/// of `next_session_id`'s timestamp/pid format, which would otherwise
/// make ambiguity hard to construct in a single millisecond.
fn seed_session_dir(store: &SessionStore, id: &str) {
    fs::create_dir_all(store.root().join(id)).expect("seed live session dir");
}

fn seed_archived_session_dir(store: &SessionStore, id: &str) {
    fs::create_dir_all(store.root().join(super::ARCHIVED_SUBDIR).join(id))
        .expect("seed archived session dir");
}

#[test]
fn resolve_session_id_prefix_exact_match_wins_over_longer_ids() {
    // An exact match must resolve to itself even when a longer id would
    // otherwise share the same prefix. Without this guard, typing the
    // full id of a short session would surface as ambiguous as soon as a
    // longer id began with the same characters.
    let (_root, store, _) = open_test_store("resolve-exact-match");
    seed_session_dir(&store, "abc12345");
    seed_session_dir(&store, "abc12345-extra-suffix");

    let resolved = store
        .resolve_session_id_prefix("abc12345")
        .expect("exact match must resolve");
    assert_eq!(resolved, "abc12345");
}

#[test]
fn resolve_session_id_prefix_unique_prefix_resolves() {
    // The headline ergonomics target: typing a short prefix of a unique
    // session id resolves to the full id. Archived sessions count just
    // like live ones so `squeezy sessions resume abc12` keeps working
    // after the session has aged into `archived/`.
    let (_root, store, _) = open_test_store("resolve-unique-prefix");
    seed_session_dir(&store, "alpha-001-live");
    seed_archived_session_dir(&store, "beta-002-archived");

    assert_eq!(
        store
            .resolve_session_id_prefix("alph")
            .expect("unique live prefix"),
        "alpha-001-live",
    );
    assert_eq!(
        store
            .resolve_session_id_prefix("beta")
            .expect("unique archived prefix"),
        "beta-002-archived",
    );
}

#[test]
fn resolve_session_id_prefix_ambiguous_lists_all_candidates() {
    // When the prefix is ambiguous the error must carry every matching
    // candidate, sorted ascending so a downstream CLI can render a
    // stable hint. Mixing live and archived ids in the candidate list
    // exercises the cross-tree enumeration too.
    let (_root, store, _) = open_test_store("resolve-ambiguous");
    seed_session_dir(&store, "shared-001-live");
    seed_archived_session_dir(&store, "shared-002-archived");
    seed_session_dir(&store, "other-id");

    match store.resolve_session_id_prefix("shared") {
        Err(ResolveError::AmbiguousPrefix { prefix, matches }) => {
            assert_eq!(prefix, "shared");
            assert_eq!(
                matches,
                vec![
                    "shared-001-live".to_string(),
                    "shared-002-archived".to_string(),
                ],
            );
        }
        other => panic!("expected AmbiguousPrefix, got {other:?}"),
    }
}

#[test]
fn resolve_session_id_prefix_not_found_when_no_candidate_matches() {
    // A prefix with no live or archived match must produce NotFound,
    // and an empty prefix must do the same so accidental `--session ""`
    // invocations don't silently grab the first directory the OS
    // enumerates.
    let (_root, store, _) = open_test_store("resolve-not-found");
    seed_session_dir(&store, "live-001");
    seed_archived_session_dir(&store, "archived-001");

    match store.resolve_session_id_prefix("zzz") {
        Err(ResolveError::NotFound { prefix }) => assert_eq!(prefix, "zzz"),
        other => panic!("expected NotFound for unmatched prefix, got {other:?}"),
    }
    match store.resolve_session_id_prefix("") {
        Err(ResolveError::NotFound { prefix }) => assert!(prefix.is_empty()),
        other => panic!("expected NotFound for empty prefix, got {other:?}"),
    }
}

#[test]
fn session_metadata_deserialises_legacy_payload_without_display_name_or_labels() {
    // Older `metadata.json` files predate `display_name` / `labels`.
    // `serde(default)` keeps them loadable so a rollout can ship the
    // new fields without rewriting on-disk session histories.
    let legacy = json!({
        "session_id": "sess-legacy",
        "started_at_ms": 1_700_000_000_000_u64,
        "ended_at_ms": null,
        "cwd": "/work/repo",
        "workspace_root": "/work/repo",
        "repo_root": null,
        "branch": null,
        "provider": "test-provider",
        "model": "test-model",
        "mode": "build",
        "status": "running",
        "first_user_task": "carry me forward",
        "latest_summary": null,
        "cost": CostSnapshot::default(),
        "metrics": SessionMetrics::default(),
        "redactions": 0,
        "resume_available": true,
        "resume_unavailable_reason": null,
        "event_count": 0,
    });
    let metadata: SessionMetadata =
        serde_json::from_value(legacy).expect("legacy metadata deserialise");
    assert!(
        metadata.display_name.is_none(),
        "legacy metadata must default to no display name"
    );
    assert!(
        metadata.labels.is_empty(),
        "legacy metadata must default to no labels"
    );
}

#[test]
fn session_metadata_round_trips_display_name_and_labels() {
    let metadata = SessionMetadata {
        session_id: "sess-roundtrip".to_string(),
        cwd: "/work/repo".to_string(),
        workspace_root: "/work/repo".to_string(),
        display_name: Some("payments refactor".to_string()),
        labels: vec!["bugfix".to_string(), "payments".to_string()],
        ..SessionMetadata::default()
    };
    let text = serde_json::to_string(&metadata).expect("serialise");
    let parsed: SessionMetadata = serde_json::from_str(&text).expect("deserialise");
    assert_eq!(parsed.display_name.as_deref(), Some("payments refactor"));
    assert_eq!(parsed.labels, vec!["bugfix", "payments"]);
}

#[test]
fn update_metadata_and_index_persists_display_name_to_metadata_json() {
    // End-to-end: rename a session, then re-open it and confirm the new
    // display_name reaches the on-disk metadata.json. The global index
    // refresh path is exercised in production but skipped here because
    // `temp_root` lives under the OS temp dir and the index write
    // guards against polluting `~/.squeezy/sessions/index.jsonl`.
    let root = temp_root("update-metadata-display-name");
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");
    handle
        .append_event(SessionEvent::new(
            "user_message",
            None,
            Some("seed".to_string()),
            json!({"ok": true}),
        ))
        .expect("append event");
    handle.flush_events().expect("flush");

    let snapshot = handle
        .update_metadata_and_index(|metadata| {
            metadata.display_name = Some("payments refactor".to_string());
            metadata.labels.push("bugfix".to_string());
        })
        .expect("update metadata");
    assert_eq!(snapshot.display_name.as_deref(), Some("payments refactor"));
    assert_eq!(snapshot.labels, vec!["bugfix".to_string()]);

    let reread = store.show(handle.session_id()).expect("re-open session");
    assert_eq!(
        reread.metadata.display_name.as_deref(),
        Some("payments refactor"),
        "rename must survive a fresh metadata read"
    );
    assert_eq!(reread.metadata.labels, vec!["bugfix".to_string()]);
}

#[test]
fn global_session_index_entry_propagates_display_name() {
    let metadata = SessionMetadata {
        session_id: "sess-index".to_string(),
        cwd: "/work/repo".to_string(),
        workspace_root: "/work/repo".to_string(),
        first_user_task: Some("inferred task".to_string()),
        display_name: Some("nice name".to_string()),
        ..SessionMetadata::default()
    };
    let entry = GlobalSessionIndexEntry::from_metadata(&metadata, 42);
    assert_eq!(entry.display_name.as_deref(), Some("nice name"));
    assert_eq!(entry.title.as_deref(), Some("inferred task"));
}

#[test]
fn custom_session_event_round_trips_through_event_log() {
    // Acceptance for the extension-author surface: a `Custom` event
    // appended via the typed API must survive the full
    // append -> events.jsonl -> show() -> try_from_event loop with its
    // extension-supplied `kind` discriminator and `payload` intact.
    // Without this, any sidecar telemetry / audit data an extension
    // attaches to a session would be silently corrupted on every reload.
    let root = temp_root("custom-event-roundtrip");
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");

    let custom = SessionEventKind::Custom {
        kind: "my_org.audit_log".to_string(),
        payload: json!({
            "actor": "alice",
            "tokens": 42,
            "tags": ["billing", "qa"],
            "nested": {"flag": true}
        }),
    };
    handle
        .append_typed_event(custom.clone(), Some("turn-1".to_string()), None)
        .expect("append custom event");
    handle.flush_events().expect("flush events");

    let record = store.show(handle.session_id()).expect("show");
    let event = record
        .events
        .iter()
        .find(|event| event.kind == "custom")
        .expect("custom event present in events.jsonl");
    assert_eq!(event.kind, custom.discriminator());
    let typed = SessionEventKind::try_from_event(event).expect("typed view");
    assert_eq!(typed, custom);
}

#[test]
fn custom_session_events_are_ignored_by_core_readers() {
    // Acceptance: extension-authored Custom events must not influence
    // the conversation replay reducer or the session enumeration
    // surface. A Custom event sitting between a user message and an
    // assistant reply must be enumerated by `show()` (no data loss)
    // and listed by `list()` (no broken discovery), but the replay
    // fallback must reconstruct the conversation as if the Custom
    // event were not there — otherwise an extension could poison the
    // resume payload by appending arbitrary JSON.
    let root = temp_root("custom-event-ignored");
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");

    handle
        .append_typed_event(
            SessionEventKind::UserMessage {
                text: "kick it off".to_string(),
            },
            None,
            None,
        )
        .expect("user message");
    handle
        .append_typed_event(
            SessionEventKind::Custom {
                kind: "telemetry".to_string(),
                payload: json!({"latency_ms": 7, "model": "test"}),
            },
            Some("1".to_string()),
            None,
        )
        .expect("custom event");
    handle
        .append_typed_event(
            SessionEventKind::AssistantCompleted {
                text: "done".to_string(),
                response_id: Some("resp-1".to_string()),
            },
            Some("1".to_string()),
            None,
        )
        .expect("assistant completion");
    handle.flush_events().expect("flush events");

    let listed = store.list(&SessionQuery::default()).expect("list");
    assert!(
        listed
            .iter()
            .any(|metadata| metadata.session_id == handle.session_id()),
        "list() must still surface the session even when Custom events are interleaved",
    );

    let record = store.show(handle.session_id()).expect("show");
    assert_eq!(
        record.events.len(),
        3,
        "show() must return every appended event, including the Custom one",
    );
    let custom_event = record
        .events
        .iter()
        .find(|event| event.kind == "custom")
        .expect("custom event must round-trip through show()");
    assert!(matches!(
        SessionEventKind::try_from_event(custom_event),
        Some(SessionEventKind::Custom { .. })
    ));

    let session_dir = store.root().join(handle.session_id());
    fs::remove_file(session_dir.join("resume_state.json"))
        .expect("delete resume_state.json to force the events.jsonl fallback");
    let replayed = handle
        .replay_resume_state()
        .expect("replay reconstructs from events.jsonl");
    assert_eq!(
        replayed.conversation,
        vec![
            ResumeItem::UserText {
                text: "kick it off".to_string(),
            },
            ResumeItem::AssistantText {
                text: "done".to_string(),
            },
        ],
        "Custom events must be ignored by the replay reducer",
    );
}

#[test]
fn replay_resume_state_hydrates_tool_result_cards() {
    // A resumed session must surface tool-result cards (shell
    // commands, file edits, grep results) the same way a fresh
    // turn does. Before `HydratedTranscriptItem` landed,
    // `apply_event_to_replay` pushed `SessionEventKind::ToolResult`
    // only into `conversation` (the LLM context) and silently
    // dropped it from the UI hydration path — so a resumed
    // session went from `user → tool_call → tool_result → assistant`
    // to `user → assistant` with the tool work invisible.
    let root = temp_root("replay-tool-result-hydration");
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
        .start_session_eager(SessionMetadata::new(&config, "test-provider"))
        .expect("start session");

    let tool_call_args = json!({"command": "ls", "workdir": "."});
    // The shape production actually persists: the model-facing
    // `FunctionCallOutput` resume item, whose `output` is the
    // `ToolResult::model_output()` string `{"status":…,"content":…}`. This is
    // NOT a serialized `ToolResult` — `tool_name`/`status`/`content` are not
    // top-level fields. The TUI rebuilds the full card from this on hydration
    // (see `reconstruct_resumed_tool_result`); the store forwards it verbatim.
    let model_output = json!({
        "status": "Success",
        "content": {
            "command": "ls",
            "stdout": "Cargo.toml\nsrc",
            "stderr": "",
            "exit_code": 0,
        },
    })
    .to_string();
    let tool_result_payload = json!({
        "type": "function_call_output",
        "call_id": "call-1",
        "output": model_output,
    });

    handle
        .append_typed_event(
            SessionEventKind::UserMessage {
                text: "list the files".to_string(),
            },
            None,
            None,
        )
        .expect("user message");
    handle
        .append_typed_event(
            SessionEventKind::ToolCall {
                call_id: "call-1".to_string(),
                tool: "shell".to_string(),
                arguments: tool_call_args.clone(),
            },
            Some("1".to_string()),
            None,
        )
        .expect("tool call");
    handle
        .append_typed_event(
            SessionEventKind::ToolResult {
                output: tool_result_payload.clone(),
            },
            Some("1".to_string()),
            None,
        )
        .expect("tool result");
    handle
        .append_typed_event(
            SessionEventKind::AssistantCompleted {
                text: "found Cargo.toml and src".to_string(),
                response_id: Some("resp-1".to_string()),
            },
            Some("1".to_string()),
            None,
        )
        .expect("assistant completion");
    handle.flush_events().expect("flush events");

    let session_dir = store.root().join(handle.session_id());
    fs::remove_file(session_dir.join("resume_state.json"))
        .expect("force the events.jsonl replay path");

    let replayed = handle
        .replay_resume_state()
        .expect("replay reconstructs from events.jsonl");
    assert!(replayed.resume_available);

    // `hydrated_transcript` must carry: the user message, the
    // tool-result card (with the matching tool_call paired in),
    // and the assistant message.
    assert_eq!(
        replayed.hydrated_transcript.len(),
        3,
        "expected user + tool-result + assistant entries, got {:#?}",
        replayed.hydrated_transcript
    );

    match &replayed.hydrated_transcript[0] {
        HydratedTranscriptItem::Message { item } => {
            assert_eq!(item.role, squeezy_core::Role::User);
            assert_eq!(item.content, "list the files");
        }
        other => panic!("entry 0 should be the user message, got {other:?}"),
    }

    match &replayed.hydrated_transcript[1] {
        HydratedTranscriptItem::ToolResult { call, result } => {
            let call = call.as_ref().expect("matching ToolCall must be paired");
            assert_eq!(call.call_id, "call-1");
            assert_eq!(call.tool, "shell");
            assert_eq!(call.arguments, tool_call_args);
            // The store forwards the persisted FunctionCallOutput resume item
            // verbatim — not a serialized ToolResult. `tool_name` comes from
            // the paired call above; `status`/`content` live inside the
            // `output` model_output string, which the TUI reconstructs from.
            assert_eq!(
                result.get("call_id").and_then(|v| v.as_str()),
                Some("call-1")
            );
            assert_eq!(
                result.get("type").and_then(|v| v.as_str()),
                Some("function_call_output")
            );
            let model_output: serde_json::Value = serde_json::from_str(
                result
                    .get("output")
                    .and_then(|v| v.as_str())
                    .expect("output carries the model_output string"),
            )
            .expect("output is model_output JSON");
            assert_eq!(
                model_output.get("status").and_then(|v| v.as_str()),
                Some("Success")
            );
            assert_eq!(
                model_output
                    .pointer("/content/stdout")
                    .and_then(|v| v.as_str()),
                Some("Cargo.toml\nsrc")
            );
        }
        other => panic!("entry 1 should be a ToolResult card, got {other:?}"),
    }

    match &replayed.hydrated_transcript[2] {
        HydratedTranscriptItem::Message { item } => {
            assert_eq!(item.role, squeezy_core::Role::Assistant);
            assert_eq!(item.content, "found Cargo.toml and src");
        }
        other => panic!("entry 2 should be the assistant message, got {other:?}"),
    }

    // Legacy `transcript` field is also still populated — older
    // binaries that haven't learned about `hydrated_transcript`
    // continue to read user / assistant messages out of it without
    // crashing on the missing tool-result card.
    assert_eq!(replayed.transcript.len(), 2);
    assert_eq!(replayed.transcript[0].role, squeezy_core::Role::User);
    assert_eq!(replayed.transcript[1].role, squeezy_core::Role::Assistant);
}

#[test]
fn write_json_is_atomic_no_stale_tmp_and_preserves_original() {
    let root = temp_root("write-json-atomic");
    let path = root.join("metadata.json");

    // Seed a valid file, then a regular rewrite.
    write_json(&path, &json!({ "k": "first" })).expect("seed write");
    write_json(&path, &json!({ "k": "second" })).expect("rewrite");

    // The final file deserializes to the latest value...
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read target")).expect("valid json");
    assert_eq!(value.get("k").and_then(|v| v.as_str()), Some("second"));

    // ...and no temp sibling is left behind on the happy path. The write
    // routes through a sibling temp + rename rather than truncating the
    // target in place, so a reader only ever sees a complete file.
    let leftover: Vec<PathBuf> = fs::read_dir(&root)
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".tmp"))
        })
        .collect();
    assert!(leftover.is_empty(), "stale temp file(s) left: {leftover:?}");

    // A pre-existing stale temp sibling (e.g. from a crashed prior write)
    // does not corrupt or block the next write: the good target survives
    // and is replaced atomically. Use a high seq value to avoid colliding
    // with the live WRITE_UNIQUE_COUNTER in this process.
    let stale_tmp = root.join(format!(".metadata.json.{}.999999.tmp", std::process::id()));
    fs::write(&stale_tmp, b"{ this is not valid json").expect("seed stale tmp");
    write_json(&path, &json!({ "k": "third" })).expect("rewrite over stale tmp");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read target")).expect("valid json");
    assert_eq!(value.get("k").and_then(|v| v.as_str()), Some("third"));

    let _ = fs::remove_dir_all(&root);
}

fn leftover_tmp_files(root: &std::path::Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".tmp"))
        })
        .collect()
}

#[test]
fn write_json_bytes_uses_unique_temps_for_same_process_concurrent_writes() {
    let root = temp_root("write-json-concurrent");
    let path = root.join("metadata.json");
    let writers = 12usize;
    let barrier = Arc::new(Barrier::new(writers));
    let handles = (0..writers)
        .map(|writer| {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let payload = format!(r#"{{"writer":{writer}}}"#);
                write_json_bytes(&path, payload.as_bytes()).expect("concurrent write succeeds");
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().expect("writer thread should not panic");
    }

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read target")).expect("valid json");
    let writer = value
        .get("writer")
        .and_then(|value| value.as_u64())
        .expect("writer field");
    assert!(writer < writers as u64, "unexpected writer id {writer}");

    let leftover = leftover_tmp_files(&root);
    assert!(leftover.is_empty(), "stale temp file(s) left: {leftover:?}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rename_retry_retries_windows_lock_errors_and_stops_on_permission_denied() {
    let root = temp_root("rename-retry-classification");
    let from = root.join("from");
    let to = root.join("to");

    assert!(is_windows_sharing_violation(&std::io::Error::from_raw_os_error(32)));
    assert!(is_windows_sharing_violation(&std::io::Error::from_raw_os_error(33)));
    assert!(!is_windows_sharing_violation(&std::io::Error::from_raw_os_error(5)));

    let mut attempts = 0usize;
    retry_windows_rename_with(
        &from,
        &to,
        |_, _| {
            attempts += 1;
            if attempts < 3 {
                Err(std::io::Error::from_raw_os_error(32))
            } else {
                Ok(())
            }
        },
        |_| {},
    )
    .expect("eventual success after transient sharing violations");
    assert_eq!(attempts, 3);

    let mut denied_attempts = 0usize;
    let denied = retry_windows_rename_with(
        &from,
        &to,
        |_, _| {
            denied_attempts += 1;
            Err(std::io::Error::from_raw_os_error(5))
        },
        |_| {},
    );
    assert!(denied.is_err());
    assert_eq!(denied_attempts, 1, "permanent permission errors must not retry");

    let mut locked_attempts = 0usize;
    let locked = retry_windows_rename_with(
        &from,
        &to,
        |_, _| {
            locked_attempts += 1;
            Err(std::io::Error::from_raw_os_error(33))
        },
        |_| {},
    );
    assert!(locked.is_err());
    assert_eq!(
        locked_attempts,
        WINDOWS_RENAME_RETRY_ATTEMPTS as usize + 1,
        "retry attempts plus final rename"
    );

    let _ = fs::remove_dir_all(&root);
}

fn global_index_entry(session_id: String, started_at_ms: u64) -> GlobalSessionIndexEntry {
    GlobalSessionIndexEntry {
        session_id,
        cwd: "/Users/dev/projects/example-workspace".to_string(),
        workspace_root: "/Users/dev/projects/example-workspace".to_string(),
        repo_root: None,
        title: Some("concurrent rewrite".to_string()),
        display_name: None,
        started_at_ms,
        last_event_at_ms: started_at_ms,
        turn_count: 1,
        resume_available: true,
    }
}

#[test]
fn rewrite_global_index_uses_unique_temps_for_same_process_concurrent_rewrites() {
    let root = temp_root("global-index-concurrent-rewrite");
    let path = root.join("index.jsonl");
    let writers = 8usize;
    let barrier = Arc::new(Barrier::new(writers));
    let handles = (0..writers)
        .map(|writer| {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            std::thread::spawn(move || {
                let entries = vec![
                    global_index_entry(format!("session-{writer}-a"), writer as u64 * 10),
                    global_index_entry(format!("session-{writer}-b"), writer as u64 * 10 + 1),
                ];
                let refs = entries.iter().collect::<Vec<_>>();
                barrier.wait();
                rewrite_global_index(&path, &refs).expect("concurrent index rewrite succeeds");
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().expect("writer thread should not panic");
    }

    let lines = fs::read_to_string(&path).expect("read rewritten index");
    let parsed = lines
        .lines()
        .map(|line| serde_json::from_str::<GlobalSessionIndexEntry>(line).expect("valid entry"))
        .collect::<Vec<_>>();
    assert!(!parsed.is_empty(), "rewritten index should contain entries");
    assert!(
        parsed
            .iter()
            .all(|entry| entry.session_id.starts_with("session-")),
        "rewritten index contains unexpected entries: {parsed:?}"
    );

    let leftover = leftover_tmp_files(&root);
    assert!(leftover.is_empty(), "stale temp file(s) left: {leftover:?}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn global_index_caps_to_most_recent_and_compacts_oversized_file() {
    let home = temp_root("global-index-cap");
    with_home(&home, || {
        // More distinct sessions than the cap, with enough bytes to clear the
        // compaction threshold. Dedup alone can never shrink this (every id is
        // unique), so without the count cap the file would be rewritten whole
        // on every read — the exact pathology this guards.
        let total = GLOBAL_INDEX_MAX_ENTRIES + 1_500;
        for i in 0..total {
            let entry = GlobalSessionIndexEntry {
                session_id: format!("session-{i:06}"),
                cwd: "/Users/dev/projects/example-workspace".to_string(),
                workspace_root: "/Users/dev/projects/example-workspace".to_string(),
                repo_root: None,
                title: Some("recent task summary placeholder".to_string()),
                display_name: None,
                started_at_ms: i as u64,
                last_event_at_ms: i as u64,
                turn_count: 1,
                resume_available: true,
            };
            SessionStore::append_global_index_entry(&entry);
        }
        let path = SessionStore::global_index_path().expect("HOME set");
        assert!(
            fs::metadata(&path).expect("index exists").len() > GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES,
            "test fixture must exceed the compaction threshold"
        );

        // Invariants asserted rather than exact ids: other suite tests create
        // sessions without taking `HOME_LOCK`, so a concurrent real append can
        // land in this temp index. The cap, compaction, and ordering hold
        // regardless of such interlopers.
        let listed = SessionStore::list_global_index();
        assert_eq!(
            listed.len(),
            GLOBAL_INDEX_MAX_ENTRIES,
            "read must cap to the most-recent N"
        );
        assert!(
            listed
                .windows(2)
                .all(|pair| pair[0].started_at_ms >= pair[1].started_at_ms),
            "entries are returned newest-first"
        );
        // The read compacted the oversized file down to ~cap (far below the
        // `total` we wrote). A small margin tolerates a concurrent real append
        // landing after our compaction; the point is that it is bounded near
        // the cap, not unbounded with history.
        let lines = fs::read_to_string(&path)
            .expect("read index")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        assert!(
            lines <= GLOBAL_INDEX_MAX_ENTRIES + 64 && lines < total,
            "oversized index compacted near the cap on read (got {lines})"
        );
        assert_eq!(
            SessionStore::list_global_index().len(),
            GLOBAL_INDEX_MAX_ENTRIES
        );
    });
}

#[test]
fn global_index_cache_invalidates_when_append_changes_file() {
    let home = temp_root("global-index-cache");
    with_home(&home, || {
        let first = GlobalSessionIndexEntry {
            session_id: "session-one".to_string(),
            cwd: "/Users/dev/projects/one".to_string(),
            workspace_root: "/Users/dev/projects/one".to_string(),
            repo_root: None,
            title: Some("first".to_string()),
            display_name: None,
            started_at_ms: 1,
            last_event_at_ms: 1,
            turn_count: 1,
            resume_available: true,
        };
        SessionStore::append_global_index_entry(&first);
        let listed = SessionStore::list_global_index();
        assert!(
            listed
                .iter()
                .any(|entry| entry.session_id == first.session_id),
            "first append should be visible in the listed index: {listed:?}"
        );

        let second = GlobalSessionIndexEntry {
            session_id: "session-two".to_string(),
            cwd: "/Users/dev/projects/two".to_string(),
            workspace_root: "/Users/dev/projects/two".to_string(),
            repo_root: None,
            title: Some("second".to_string()),
            display_name: None,
            started_at_ms: 2,
            last_event_at_ms: 2,
            turn_count: 1,
            resume_available: true,
        };
        SessionStore::append_global_index_entry(&second);
        let listed = SessionStore::list_global_index();
        let first_pos = listed
            .iter()
            .position(|entry| entry.session_id == first.session_id)
            .expect("first session should still be visible");
        let second_pos = listed
            .iter()
            .position(|entry| entry.session_id == second.session_id)
            .expect("second append should invalidate the unchanged-file cache");
        assert!(
            second_pos < first_pos,
            "local entries should preserve newest-first order after cache invalidation: {listed:?}"
        );
    });
}
