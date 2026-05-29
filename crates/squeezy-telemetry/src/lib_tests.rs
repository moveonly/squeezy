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
        args_sha256: None,
        output_sha256: None,
        content_sha256: None,
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
fn tool_completed_event_pairs_args_output_content_sha256() {
    // F06: paired-SHA dispatch trace. Each tool_completed event must carry
    // (args_sha256, output_sha256, content_sha256) so offline replay can
    // answer "did we already pay for this exact call?" without re-running.
    let config = AppConfig::default();
    let args = "a".repeat(64);
    let output = "b".repeat(64);
    let content = "c".repeat(64);
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
        args_sha256: Some(&args),
        output_sha256: Some(&output),
        content_sha256: Some(&content),
    });
    let text = serde_json::to_string(&event).unwrap();
    assert!(
        text.contains(&format!("\"args_sha256\":\"{args}\"")),
        "args_sha256 missing: {text}"
    );
    assert!(
        text.contains(&format!("\"output_sha256\":\"{output}\"")),
        "output_sha256 missing: {text}"
    );
    assert!(
        text.contains(&format!("\"content_sha256\":\"{content}\"")),
        "content_sha256 missing: {text}"
    );
}

#[test]
fn tool_completed_event_omits_sha_fields_when_absent() {
    let config = AppConfig::default();
    let event = TelemetryEvent::tool_completed(ToolTelemetryReport {
        provider: &config.provider,
        model: &config.model,
        turn_index: 1,
        tool_sequence: 1,
        tool_name: "grep",
        status: ToolStatusKind::Success,
        duration: Duration::from_millis(1),
        cost: ToolCostProperties::default(),
        args_sha256: None,
        output_sha256: None,
        content_sha256: None,
    });
    let text = serde_json::to_string(&event).unwrap();
    assert!(!text.contains("args_sha256"));
    assert!(!text.contains("output_sha256"));
    assert!(!text.contains("content_sha256"));
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
        args_sha256: None,
        output_sha256: None,
        content_sha256: None,
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
            args_sha256: None,
            output_sha256: None,
            content_sha256: None,
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
        symbols: 77,
        edges: 42,
        language_distribution: LanguageDistribution {
            c_files: 2,
            csharp_files: 2,
            cpp_files: 3,
            go_files: 1,
            python_files: 4,
            rust_files: 8,
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
    assert!(text.contains("\"python_files\":4"));
    assert!(text.contains("\"rust_files\":8"));
    assert!(text.contains("\"unsupported_files\":3"));
    assert!(!text.contains("/Users/"));
}
