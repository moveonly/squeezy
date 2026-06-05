use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use squeezy_core::{
    AppConfig, CostSnapshot, DEFAULT_TELEMETRY_ENDPOINT, FeedbackConfig, TurnMetrics,
};

use super::*;

#[test]
fn disabled_client_does_not_send() {
    assert!(!TelemetryClient::disabled().enabled());
}

#[test]
fn draining_event_buffer_preserves_batch_capacity() {
    let config = AppConfig::default();
    let mut events = vec![TelemetryEvent::app_started(&config)];

    let drained = drain_event_buffer(&mut events);

    assert_eq!(drained.len(), 1);
    assert!(events.is_empty());
    assert!(events.capacity() >= MAX_BATCH_EVENTS);
}

#[test]
fn telemetry_disabled_when_install_id_cannot_be_persisted() {
    let root = telemetry_temp_root();
    fs::create_dir_all(&root).unwrap();
    // Use a path whose parent already exists as a file, so create_dir_all
    // and the subsequent write both fail deterministically.
    let blocker = root.join("blocker");
    fs::write(&blocker, b"").unwrap();
    let bad_path = blocker.join("install_id");

    let config = AppConfig {
        telemetry: telemetry_config(true, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };
    let client = TelemetryClient::from_config_with_install_path(&config, &bad_path);
    assert!(
        !client.enabled(),
        "telemetry must be disabled when install_id cannot be persisted"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn install_id_is_persisted() {
    let root = telemetry_temp_root();
    let path = root.join("install_id");
    let config = AppConfig {
        telemetry: telemetry_config(true, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };

    let first = TelemetryClient::from_config_with_install_path(&config, &path);
    let second = TelemetryClient::from_config_with_install_path(&config, &path);

    let first_id = first.state.as_ref().unwrap().install_id.clone();
    let second_id = second.state.as_ref().unwrap().install_id.clone();
    assert_eq!(first_id, second_id);
    assert!(is_uuid_like(&first_id));
    assert!(fs::read_to_string(path).unwrap().contains(&first_id));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn feedback_preview_is_redacted_and_size_checked() {
    let config = AppConfig {
        feedback: FeedbackConfig {
            max_feedback_bytes: 128,
            ..FeedbackConfig::default()
        },
        ..AppConfig::default()
    };
    let prepared = prepare_feedback(
        &config,
        "fails with OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz123456",
        "cli",
    )
    .expect("prepare feedback");

    assert_eq!(prepared.source, "cli");
    assert!(prepared.message.contains("<redacted:"));
    assert!(
        !prepared
            .message
            .contains("sk-abcdefghijklmnopqrstuvwxyz123456")
    );

    let error = prepare_feedback(&config, &"x".repeat(129), "cli")
        .expect_err("oversized feedback must fail");
    assert!(error.to_string().contains("max_feedback_bytes"));
}

#[tokio::test]
async fn record_buffers_events_for_periodic_batch_flush() {
    let root = telemetry_temp_root();
    let path = root.join("install_id");
    let config = AppConfig {
        telemetry: telemetry_config(true, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };
    let client = TelemetryClient::from_config_with_install_path(&config, &path);

    client.record(TelemetryEvent::app_started(&config)).await;

    let state = client.state.as_ref().unwrap();
    let queue = state.queue.lock().await;
    assert_eq!(queue.events.len(), 1);
    assert!(queue.flush_scheduled);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn durable_summary_is_pending_before_send_and_cleared_after_ack() {
    let root = telemetry_temp_root();
    let store = TelemetryStore::open(root.join("telemetry.redb")).expect("open telemetry store");
    let session_id = "22222222-2222-4222-8222-222222222222";
    let trace_id = "a".repeat(32);
    store
        .mark_session_started(session_id, &trace_id, 1_000)
        .expect("mark session start");
    let config = AppConfig::default();
    let mut started = TelemetryEvent::app_started(&config);
    started.timestamp_ms = 1_000;
    started.event_sequence = 1;
    store
        .append_event(session_id, &started)
        .expect("append start");
    let mut ended = TelemetryEvent::session_ended(
        &config,
        SessionTelemetryReport {
            duration_ms: 500,
            status: SessionStatusKind::Completed,
            store_session_id: None,
            turns: 1,
            tool_calls: 2,
            tool_successes: 2,
            tool_errors: 0,
            tool_denials: 0,
            tool_cancellations: 0,
            budget_denials: 0,
            subagent_calls: 0,
            subagent_failures: 0,
            subagent_kind_counts: std::collections::BTreeMap::new(),
            subagent_cap_rejections: 0,
        },
    );
    ended.timestamp_ms = 1_500;
    ended.event_sequence = 2;
    store.append_event(session_id, &ended).expect("append end");
    store
        .mark_session_ended(session_id, ended.timestamp_ms)
        .expect("mark session end");

    let summary_id = store
        .finalize_session_summary(session_id, false)
        .expect("finalize")
        .expect("summary id");
    let due = store
        .lease_due_summaries(2_000, 10, PENDING_LEASE_MS)
        .expect("lease due");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].summary_id, summary_id);
    assert_eq!(due[0].event.event, TelemetryEventName::SessionSummary);
    assert_eq!(
        due[0].event.properties.session_status,
        Some(SessionStatusKind::Completed)
    );

    store.mark_summary_sent(&due[0]).expect("mark sent");
    assert!(
        store
            .lease_due_summaries(3_000, 10, PENDING_LEASE_MS)
            .expect("lease after sent")
            .is_empty()
    );
    assert!(
        store
            .session_events(session_id)
            .expect("events after sent")
            .is_empty()
    );
    assert!(
        store
            .session(session_id)
            .expect("session after sent")
            .is_none()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn failed_summary_is_rescheduled_not_deleted() {
    let root = telemetry_temp_root();
    let store = TelemetryStore::open(root.join("telemetry.redb")).expect("open telemetry store");
    let session_id = "22222222-2222-4222-8222-222222222222";
    store
        .mark_session_started(session_id, &"b".repeat(32), 1_000)
        .expect("mark session start");
    let mut event = TelemetryEvent::app_started(&AppConfig::default());
    event.timestamp_ms = 1_000;
    event.event_sequence = 1;
    store
        .append_event(session_id, &event)
        .expect("append event");
    let summary_id = store
        .finalize_session_summary(session_id, false)
        .expect("finalize")
        .expect("summary id");
    let leased = store
        .lease_due_summaries(2_000, 10, PENDING_LEASE_MS)
        .expect("lease");
    assert_eq!(leased.len(), 1);

    store
        .mark_summary_failed(&summary_id, 2_100)
        .expect("mark failed");
    assert!(
        store
            .lease_due_summaries(2_200, 10, PENDING_LEASE_MS)
            .expect("early retry")
            .is_empty()
    );
    assert_eq!(
        store
            .lease_due_summaries(7_200, 10, PENDING_LEASE_MS)
            .expect("due retry")
            .len(),
        1
    );
    assert_eq!(
        store
            .session_events(session_id)
            .expect("events preserved")
            .len(),
        1
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn abnormal_session_is_synthesized_on_next_startup() {
    let root = telemetry_temp_root();
    let store = TelemetryStore::open(root.join("telemetry.redb")).expect("open telemetry store");
    let prior_session = "22222222-2222-4222-8222-222222222222";
    store
        .mark_session_started(prior_session, &"c".repeat(32), 1_000)
        .expect("mark session start");
    let mut event = TelemetryEvent::app_started(&AppConfig::default());
    event.timestamp_ms = 1_000;
    event.event_sequence = 1;
    store
        .append_event(prior_session, &event)
        .expect("append event");

    store
        .synthesize_abnormal_sessions("33333333-3333-4333-8333-333333333333")
        .expect("synthesize abnormal");
    let due = store
        .lease_due_summaries(2_000, 10, PENDING_LEASE_MS)
        .expect("lease due");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].event.properties.abnormal_exit, Some(true));
    assert_eq!(
        due[0].event.properties.session_status,
        Some(SessionStatusKind::Failed)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn telemetry_opt_out_purges_pending_product_ledger() {
    let root = telemetry_temp_root();
    let install_id_path = root.join("install_id");
    fs::create_dir_all(&root).unwrap();
    fs::write(&install_id_path, "11111111-1111-4111-8111-111111111111\n").unwrap();
    let store_path = root.join("telemetry.redb");
    {
        let store = TelemetryStore::open(store_path.clone()).expect("open telemetry store");
        let session_id = "22222222-2222-4222-8222-222222222222";
        store
            .mark_session_started(session_id, &"d".repeat(32), 1_000)
            .expect("mark session start");
        let mut event = TelemetryEvent::app_started(&AppConfig::default());
        event.timestamp_ms = 1_000;
        event.event_sequence = 1;
        store
            .append_event(session_id, &event)
            .expect("append event");
        store
            .finalize_session_summary(session_id, false)
            .expect("finalize");
    }
    assert!(store_path.exists(), "ledger should exist before opt-out");

    let config = AppConfig {
        telemetry: telemetry_config(false, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };
    let client = TelemetryClient::from_config_with_install_path(&config, &install_id_path);
    assert!(!client.enabled());
    assert!(
        !store_path.exists(),
        "opt-out must purge automatic telemetry"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn session_summary_does_not_double_count_turn_and_tool_totals() {
    let config = AppConfig::default();
    let session = StoredTelemetrySession {
        session_id: "22222222-2222-4222-8222-222222222222".to_string(),
        trace_id: "e".repeat(32),
        started_at_ms: 1_000,
        ended_at_ms: Some(2_000),
        clean_end: true,
        summary_id: None,
    };
    let mut turn = TelemetryEvent::turn_completed(
        &config,
        1,
        TurnMetrics {
            tool_calls: 2,
            bytes_read: 300,
            model_output_bytes: 50,
            matches_returned: 4,
            ..TurnMetrics::default()
        },
    );
    turn.timestamp_ms = 1_500;
    turn.event_sequence = 1;
    let mut shell = TelemetryEvent::tool_completed(ToolTelemetryReport {
        provider: &config.provider,
        model: &config.model,
        turn_index: 1,
        tool_sequence: 1,
        tool_name: "shell",
        status: ToolStatusKind::Success,
        duration: Duration::from_millis(10),
        cost: ToolCostProperties {
            bytes_read: 100,
            output_bytes: 25,
            matches_returned: 1,
            ..ToolCostProperties::default()
        },
    });
    shell.timestamp_ms = 1_600;
    shell.event_sequence = 2;
    let mut grep = TelemetryEvent::tool_completed(ToolTelemetryReport {
        provider: &config.provider,
        model: &config.model,
        turn_index: 1,
        tool_sequence: 2,
        tool_name: "grep",
        status: ToolStatusKind::Error,
        duration: Duration::from_millis(10),
        cost: ToolCostProperties {
            bytes_read: 200,
            output_bytes: 25,
            matches_returned: 3,
            ..ToolCostProperties::default()
        },
    });
    grep.timestamp_ms = 1_700;
    grep.event_sequence = 3;

    let summary = build_summary_from_events(&session, vec![turn, shell, grep], false, None);

    assert_eq!(summary.properties.tool_calls, Some(2));
    assert_eq!(summary.properties.tool_successes, Some(1));
    assert_eq!(summary.properties.tool_errors, Some(1));
    assert_eq!(summary.properties.bytes_read, Some(300));
    assert_eq!(summary.properties.matches_returned, Some(4));
}

fn telemetry_temp_root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "squeezy-telemetry-{}-{}-{}",
        now_ms(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn event_properties_are_sanitized_enums_and_numbers() {
    let config = AppConfig {
        model: "private-company-model".to_string(),
        ..AppConfig::default()
    };
    let event = TelemetryEvent::turn_completed(
        &config,
        7,
        TurnMetrics {
            tool_calls: 3,
            bytes_read: 123_456,
            provider: CostSnapshot {
                input_tokens: Some(10),
                output_tokens: Some(2),
                ..CostSnapshot::default()
            },
            ..TurnMetrics::default()
        },
    );
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("squeezy_turn_completed"));
    assert!(text.contains("\"turn_index\":7"));
    assert!(text.contains("\"tool_calls\":3"));
    assert!(text.contains("other"));
    assert!(!text.contains("private-company-model"));
}

#[test]
fn tool_event_does_not_include_arguments_or_paths() {
    let config = AppConfig::default();
    let event = TelemetryEvent::tool_completed(ToolTelemetryReport {
        provider: &config.provider,
        model: &config.model,
        turn_index: 2,
        tool_sequence: 5,
        tool_name: "grep",
        status: ToolStatusKind::Success,
        duration: Duration::from_millis(42),
        cost: ToolCostProperties {
            files_scanned: 2,
            bytes_read: 100,
            matches_returned: 1,
            output_bytes: 50,
        },
    });
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("grep"));
    assert!(text.contains("\"turn_index\":2"));
    assert!(text.contains("\"tool_sequence\":5"));
    assert!(text.contains("\"duration_ms\":42"));
    assert!(!text.contains("pattern"));
    assert!(!text.contains("/Users/"));
    assert!(!text.contains("OPENAI_API_KEY"));
    assert!(!text.contains(DEFAULT_TELEMETRY_ENDPOINT));
}

#[test]
fn tool_completed_event_never_carries_sha_fields() {
    // SHA fields were removed from telemetry (noise removal): tool_completed
    // must not include args_sha256, output_sha256, or content_sha256 even
    // when a call is made. Those hashes are kept locally for dedup but must
    // not be forwarded to the remote summary.
    let config = AppConfig::default();
    let event = TelemetryEvent::tool_completed(ToolTelemetryReport {
        provider: &config.provider,
        model: &config.model,
        turn_index: 1,
        tool_sequence: 1,
        tool_name: "read_file",
        status: ToolStatusKind::Success,
        duration: Duration::from_millis(7),
        cost: ToolCostProperties {
            files_scanned: 0,
            bytes_read: 16,
            matches_returned: 0,
            output_bytes: 32,
        },
    });
    let text = serde_json::to_string(&event).unwrap();
    assert!(
        !text.contains("args_sha256"),
        "args_sha256 must be absent: {text}"
    );
    assert!(
        !text.contains("output_sha256"),
        "output_sha256 must be absent: {text}"
    );
    assert!(
        !text.contains("content_sha256"),
        "content_sha256 must be absent: {text}"
    );
}

#[test]
fn graph_navigation_tool_events_are_classified_as_graph_family() {
    let config = AppConfig::default();
    let event = TelemetryEvent::tool_completed(ToolTelemetryReport {
        provider: &config.provider,
        model: &config.model,
        turn_index: 1,
        tool_sequence: 1,
        tool_name: "read_slice",
        status: ToolStatusKind::Success,
        duration: Duration::from_millis(5),
        cost: ToolCostProperties {
            files_scanned: 0,
            bytes_read: 32,
            matches_returned: 1,
            output_bytes: 64,
        },
    });
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("\"tool_name\":\"graph\""));
    assert!(text.contains("\"tool_family\":\"graph\""));
}

#[test]
fn ai_reviewer_allow_downgrade_event_tags_capability() {
    // Reviewer downgrade-audit: the reviewer silently downgrades model `allow` decisions
    // when the capability is not in the operator's allowlist. The counter
    // must carry the capability label so operators can pivot dashboards on
    // it and decide which capability deserves to be added to the allowlist.
    let event = TelemetryEvent::ai_reviewer_allow_downgrade("shell");
    let text = serde_json::to_string(&event).unwrap();

    assert!(
        text.contains("ai_reviewer_allow_downgrade"),
        "event name slug missing: {text}"
    );
    assert!(
        text.contains("\"permission_capability\":\"shell\""),
        "permission_capability missing: {text}"
    );
}

#[test]
fn shell_sandbox_best_effort_fallback_event_tags_backend_and_tool() {
    // F3-4: surface silent best_effort sandbox degradation as a counter.
    // The event name must be the documented `approval.best_effort.fallback`
    // slug so dashboards can pivot on it; the `sandbox_backend` property
    // carries which platform backend was attempted.
    let event = TelemetryEvent::shell_sandbox_best_effort_fallback("macos-sandbox-exec");
    let text = serde_json::to_string(&event).unwrap();

    assert!(
        text.contains("approval_best_effort_fallback"),
        "event name slug missing: {text}"
    );
    assert!(
        text.contains("\"sandbox_backend\":\"macos-sandbox-exec\""),
        "sandbox_backend missing: {text}"
    );
    assert!(
        text.contains("\"tool_name\":\"shell\""),
        "tool_name missing: {text}"
    );
    assert!(
        text.contains("\"tool_family\":\"shell\""),
        "tool_family missing: {text}"
    );
}

#[test]
fn trace_id_is_session_scoped_and_w3c_shaped() {
    // squeezy-dpi: every TelemetryClient holds one trace_id that is
    // stable for the life of the client and looks like a W3C trace_id
    // (32 lowercase hex chars). Disabled clients have no id.
    let root = telemetry_temp_root();
    let path = root.join("install_id");
    let config = AppConfig {
        telemetry: telemetry_config(true, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };
    let client = TelemetryClient::from_config_with_install_path(&config, &path);
    let trace_id = client.trace_id().expect("enabled client has trace id");
    assert_eq!(trace_id.len(), 32, "trace_id len: {trace_id}");
    assert!(
        trace_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "trace_id not hex: {trace_id}"
    );
    assert_eq!(
        trace_id,
        client.trace_id().unwrap(),
        "trace_id must be stable across calls on one client"
    );

    let disabled = TelemetryClient::disabled();
    assert!(disabled.trace_id().is_none());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn events_carry_consistent_trace_id_across_a_turn() {
    // squeezy-dpi: every event emitted during the same turn must carry
    // the same trace_id and the same span_id so a debugger pulling one
    // Worker-side event can pull the rest of the turn's events.
    let root = telemetry_temp_root();
    let path = root.join("install_id");
    let config = AppConfig {
        telemetry: telemetry_config(true, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };
    let client = TelemetryClient::from_config_with_install_path(&config, &path);
    let trace_id = client.trace_id().unwrap();
    let span_id = client.begin_turn().expect("begin_turn on enabled client");

    client.record(TelemetryEvent::app_started(&config)).await;
    client
        .record(TelemetryEvent::tool_completed(ToolTelemetryReport {
            provider: &config.provider,
            model: &config.model,
            turn_index: 1,
            tool_sequence: 1,
            tool_name: "grep",
            status: ToolStatusKind::Success,
            duration: Duration::from_millis(3),
            cost: ToolCostProperties::default(),
        }))
        .await;
    client
        .record(TelemetryEvent::turn_completed(
            &config,
            1,
            TurnMetrics::default(),
        ))
        .await;

    let pending = client.pending_events_snapshot().await;
    assert_eq!(pending.len(), 3, "all three events enqueued");
    for event in &pending {
        assert_eq!(
            event.properties.trace_id.as_deref(),
            Some(trace_id.as_str()),
            "trace_id mismatch on {:?}",
            event.event,
        );
        assert_eq!(
            event.properties.span_id.as_deref(),
            Some(span_id.as_str()),
            "span_id mismatch on {:?}",
            event.event,
        );
    }
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn span_id_rotates_per_turn_and_clears_after_end_turn() {
    // squeezy-dpi: a new begin_turn replaces the active span_id; events
    // recorded after end_turn carry no span_id (but still carry trace_id).
    let root = telemetry_temp_root();
    let path = root.join("install_id");
    let config = AppConfig {
        telemetry: telemetry_config(true, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };
    let client = TelemetryClient::from_config_with_install_path(&config, &path);
    let trace_id = client.trace_id().unwrap();

    let span_a = client.begin_turn().unwrap();
    client
        .record(TelemetryEvent::turn_completed(
            &config,
            1,
            TurnMetrics::default(),
        ))
        .await;
    client.end_turn();
    client
        .record(TelemetryEvent::failure_seen(ErrorKind::Provider))
        .await;
    let span_b = client.begin_turn().unwrap();
    client
        .record(TelemetryEvent::turn_completed(
            &config,
            2,
            TurnMetrics::default(),
        ))
        .await;

    assert_ne!(span_a, span_b, "begin_turn must rotate span_id");
    assert_eq!(span_a.len(), 16, "span_id is 16 hex chars");
    assert_eq!(span_b.len(), 16, "span_id is 16 hex chars");
    assert!(span_a.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(span_b.bytes().all(|b| b.is_ascii_hexdigit()));

    let pending = client.pending_events_snapshot().await;
    assert_eq!(pending.len(), 3);
    assert_eq!(
        pending[0].properties.span_id.as_deref(),
        Some(span_a.as_str())
    );
    assert!(
        pending[1].properties.span_id.is_none(),
        "event after end_turn carries no span_id: {:?}",
        pending[1].properties.span_id,
    );
    assert_eq!(
        pending[2].properties.span_id.as_deref(),
        Some(span_b.as_str())
    );
    for event in &pending {
        assert_eq!(
            event.properties.trace_id.as_deref(),
            Some(trace_id.as_str())
        );
    }
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn disabled_client_does_not_stamp_or_track_spans() {
    // begin_turn / end_turn / trace_id on a disabled client are no-ops
    // and must not panic.
    let client = TelemetryClient::disabled();
    assert!(client.begin_turn().is_none());
    client.end_turn();
    assert!(client.trace_id().is_none());
    client
        .record(TelemetryEvent::failure_seen(ErrorKind::Io))
        .await;
    assert!(client.pending_events_snapshot().await.is_empty());
}

#[test]
fn spawn_without_tokio_runtime_buffers_event() {
    let root = telemetry_temp_root();
    let config = AppConfig {
        telemetry: telemetry_config(true, DEFAULT_TELEMETRY_ENDPOINT),
        ..AppConfig::default()
    };
    let client = TelemetryClient::from_config_with_install_path(&config, root.join("install_id"));
    client.spawn(TelemetryEvent::failure_seen(ErrorKind::Config));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pending = runtime.block_on(client.pending_events_snapshot());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].properties.error_kind, Some(ErrorKind::Config));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn store_session_id_is_stamped_on_every_event_after_set() {
    let root = telemetry_temp_root();
    let path = root.join("install_id");
    let config = AppConfig {
        telemetry: telemetry_config(true, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };
    let client = TelemetryClient::from_config_with_install_path(&config, &path);

    client.record(TelemetryEvent::app_started(&config)).await;
    let before = client.pending_events_snapshot().await;
    assert!(
        before[0].properties.store_session_id.is_none(),
        "store_session_id must be absent before set"
    );

    client.set_store_session_id("22222222-2222-4222-8222-222222222222");
    client
        .record(TelemetryEvent::failure_seen(ErrorKind::Io))
        .await;

    let after = client.pending_events_snapshot().await;
    assert_eq!(after.len(), 2);
    assert!(
        after[0].properties.store_session_id.is_none(),
        "already-enqueued event must not be retroactively stamped"
    );
    assert_eq!(
        after[1].properties.store_session_id.as_deref(),
        Some("22222222-2222-4222-8222-222222222222"),
        "event after set must carry store_session_id"
    );

    let disabled = TelemetryClient::disabled();
    disabled.set_store_session_id("should-not-panic");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn graph_event_carries_timing_counts_and_language_distribution() {
    let event = TelemetryEvent::graph_build_completed(GraphPerfReport {
        refresh_kind: RefreshKind::Cold,
        status: OutcomeStatus::Success,
        sequence_scope: GraphSequenceScope::OneShot,
        duration_ms: 125,
        files_seen: 12,
        files_changed: 12,
        files_parsed: 8,
        bytes_parsed: 2048,
        excluded_files: 5,
        excluded_dirs: 1,
        excluded_bytes: 512,
        persisted_files_loaded: 6,
        persisted_files_missed: 2,
        persistence_rebuilt: true,
        symbols: 77,
        edges: 42,
        language_distribution: LanguageDistribution {
            c_files: 2,
            csharp_files: 2,
            cpp_files: 3,
            dart_files: 1,
            go_files: 1,
            java_files: 1,
            javascript_files: 2,
            jsx_files: 1,
            kotlin_files: 1,
            php_files: 4,
            python_files: 4,
            ruby_files: 1,
            rust_files: 8,
            scala_files: 1,
            swift_files: 1,
            typescript_files: 3,
            tsx_files: 1,
            supported_files: 20,
            unsupported_files: 3,
            unknown_files: 1,
        },
        error_kind: None,
    });
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("squeezy_graph_build_completed"));
    assert!(text.contains("\"duration_ms\":125"));
    assert!(text.contains("\"c_files\":2"));
    assert!(text.contains("\"csharp_files\":2"));
    assert!(text.contains("\"cpp_files\":3"));
    assert!(text.contains("\"go_files\":1"));
    assert!(text.contains("\"php_files\":4"));
    assert!(text.contains("\"typescript_files\":3"));
    assert!(text.contains("\"excluded_files\":5"));
    assert!(text.contains("\"persisted_files_loaded\":6"));
    assert!(text.contains("\"persistence_rebuilt\":1"));
    assert!(text.contains("\"python_files\":4"));
    assert!(text.contains("\"rust_files\":8"));
    assert!(text.contains("\"unsupported_files\":3"));
    assert!(!text.contains("/Users/"));
}

#[test]
fn startup_ready_event_carries_route_and_duration() {
    let config = AppConfig::default();
    let event = TelemetryEvent::startup_ready(
        &config,
        StartupRoute::ResumePickerResume,
        Duration::from_millis(321),
    );
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("squeezy_startup_ready"));
    assert!(text.contains("\"startup_route\":\"resume_picker_resume\""));
    assert!(text.contains("\"duration_ms\":321"));
    assert!(!text.contains(&config.model));
}

#[test]
fn session_ended_event_carries_aggregate_perf_and_failure_counts() {
    let config = AppConfig::default();
    let event = TelemetryEvent::session_ended(
        &config,
        SessionTelemetryReport {
            duration_ms: 12_345,
            status: SessionStatusKind::Completed,
            store_session_id: Some("22222222-2222-4222-8222-222222222222".to_string()),
            turns: 4,
            tool_calls: 9,
            tool_successes: 7,
            tool_errors: 1,
            tool_denials: 1,
            tool_cancellations: 0,
            budget_denials: 2,
            subagent_calls: 3,
            subagent_failures: 1,
            subagent_kind_counts: std::collections::BTreeMap::new(),
            subagent_cap_rejections: 0,
        },
    );
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("squeezy_session_ended"));
    assert!(text.contains("\"session_status\":\"completed\""));
    assert!(text.contains("\"turn_count\":4"));
    assert!(text.contains("\"tool_errors\":1"));
    assert!(text.contains("\"subagent_failures\":1"));
    assert!(text.contains("\"store_session_id\":\"22222222-2222-4222-8222-222222222222\""));
    assert!(!text.contains(&config.model));
}

#[test]
fn slash_command_event_uses_command_dimensions_only() {
    let event = TelemetryEvent::slash_command_used(SlashTelemetryReport::new(
        "/plan",
        SlashSurface::TuiComposer,
        SlashOutcome::Accepted,
        SlashAliasKind::Canonical,
        SlashArgShape::FreeText,
    ));
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("squeezy_slash_command_used"));
    assert!(text.contains("\"slash_command\":\"plan\""));
    assert!(text.contains("\"slash_surface\":\"tui_composer\""));
    assert!(text.contains("\"slash_arg_shape\":\"free_text\""));
    assert!(!text.contains("analyze this repo"));
}

#[test]
fn slash_command_event_sanitizes_unknown_command_heads() {
    let event = TelemetryEvent::slash_command_used(SlashTelemetryReport::new(
        "/Custom/Thing?secret=abc",
        SlashSurface::TuiComposer,
        SlashOutcome::Unknown,
        SlashAliasKind::Unknown,
        SlashArgShape::Present,
    ));
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("\"slash_command\":\"unknown\""));
    assert!(!text.contains("/Custom/Thing"));
    assert!(!text.contains("secret"));
}

#[test]
fn config_change_event_uses_bucketed_values() {
    let event = TelemetryEvent::config_change_committed(ConfigChangeReport {
        scope: ConfigScopeKind::Project,
        section: "models",
        field: "model.model",
        apply_tier: ConfigApplyTier::NextPrompt,
        change_kind: ConfigChangeKind::Set,
        prev_bucket: "model_custom",
        new_bucket: "model_custom",
    });
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("squeezy_config_change_committed"));
    assert!(text.contains("\"config_scope\":\"project\""));
    assert!(text.contains("\"config_prev_bucket\":\"model_custom\""));
    assert!(text.contains("\"config_new_bucket\":\"model_custom\""));
    assert!(!text.contains("gpt-5-codex"));
}

#[test]
fn mcp_tool_name_classified_as_mcp_family() {
    use crate::{FirstPartyToolName, ToolFamily};
    assert_eq!(
        FirstPartyToolName::from_tool_name("mcp__my_server__do_thing"),
        FirstPartyToolName::Mcp
    );
    assert_eq!(
        ToolFamily::from_tool_name("mcp__another__tool"),
        ToolFamily::Mcp
    );
}

#[test]
fn mcp_discovery_event_folds_into_summary_counts() {
    use crate::McpDiscoveryReport;
    let report = McpDiscoveryReport {
        servers_stdio: 2,
        servers_http: 1,
        servers_sse: 0,
        servers_enabled: 3,
        servers_disabled: 0,
        tools_discovered: 5,
        tools_cached: 0,
        tools_stale_retained: 0,
        tools_dropped_disabled: 0,
        discovery_errors: 0,
        error_kind_counts: std::collections::BTreeMap::new(),
        has_resources: true,
        has_elicitation: false,
        has_experimental: false,
        duration_ms: 120,
    };
    let event = TelemetryEvent::mcp_discovery(report);
    let text = serde_json::to_string(&event).unwrap();
    assert!(text.contains("squeezy_mcp_discovery"));
    assert!(text.contains("transport_stdio"));
    assert!(text.contains("cap_resources"));
    // elicitation not present since has_elicitation = false
    assert!(!text.contains("cap_elicitation"));
}

#[test]
fn provider_error_event_carries_kind_token() {
    use crate::ProviderErrorKind;
    let event = TelemetryEvent::provider_error(ProviderErrorKind::RateLimit);
    let text = serde_json::to_string(&event).unwrap();
    assert!(text.contains("squeezy_provider_error"));
    assert!(text.contains("rate_limit"));
}

#[test]
fn skill_activation_event_folds_into_summary() {
    use crate::SkillActivationReport;
    let mut source_counts = std::collections::BTreeMap::new();
    source_counts.insert("user".to_string(), 1u64);
    let event = TelemetryEvent::skill_activated(SkillActivationReport {
        total: 1,
        included: 1,
        dropped: 0,
        body_truncated: 0,
        preamble_emitted: true,
        preamble_omitted_count: 0,
        explicit_count: 1,
        trigger_count: 0,
        implicit_shell_count: 0,
        source_counts,
    });
    let text = serde_json::to_string(&event).unwrap();
    assert!(text.contains("squeezy_skill_activated"));
    assert!(text.contains("source_user"));
    assert!(text.contains("activation_explicit"));
}

#[test]
fn session_summary_includes_new_domain_counts_when_events_present() {
    use crate::{McpDiscoveryReport, ProviderErrorKind, SkillActivationReport};
    let session = StoredTelemetrySession {
        session_id: "test".to_string(),
        trace_id: "00000000000000000000000000000001".to_string(),
        started_at_ms: 0,
        ended_at_ms: Some(1000),
        clean_end: true,
        summary_id: None,
    };
    let mut source_counts = std::collections::BTreeMap::new();
    source_counts.insert("project".to_string(), 2u64);
    let events = vec![
        TelemetryEvent::mcp_discovery(McpDiscoveryReport {
            servers_stdio: 1,
            ..McpDiscoveryReport::default()
        }),
        TelemetryEvent::provider_error(ProviderErrorKind::Transport),
        TelemetryEvent::skill_activated(SkillActivationReport {
            total: 2,
            included: 2,
            source_counts,
            ..SkillActivationReport::default()
        }),
    ];
    let summary =
        build_summary_from_events(&session, events, false, Some(SessionStatusKind::Completed));
    // MCP counts populated.
    let mcp = summary.properties.mcp_counts.as_ref().expect("mcp_counts");
    assert!(mcp.contains_key("transport_stdio"), "mcp_counts: {mcp:?}");
    // Provider error counts populated.
    let pe = summary
        .properties
        .provider_error_counts
        .as_ref()
        .expect("provider_error_counts");
    assert!(
        pe.contains_key("transport"),
        "provider_error_counts: {pe:?}"
    );
    // Skill counts populated.
    let sc = summary
        .properties
        .skill_counts
        .as_ref()
        .expect("skill_counts");
    assert!(sc.contains_key("source_project"), "skill_counts: {sc:?}");
}
