use std::{
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use squeezy_core::{AppConfig, CostSnapshot, DEFAULT_TELEMETRY_ENDPOINT, TurnMetrics};

use super::*;

#[test]
fn disabled_client_does_not_send() {
    assert!(!TelemetryClient::disabled().enabled());
}

#[test]
fn telemetry_disabled_when_install_id_cannot_be_persisted() {
    let root = telemetry_temp_root("install-block");
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
    let root = telemetry_temp_root("install-id");
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

#[tokio::test]
async fn record_buffers_events_for_periodic_batch_flush() {
    let root = telemetry_temp_root("batch-flush");
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
            rust_files: 8,
            supported_files: 8,
            unsupported_files: 3,
            unknown_files: 1,
        },
        error_kind: None,
    });
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains("squeezy_graph_build_completed"));
    assert!(text.contains("\"duration_ms\":125"));
    assert!(text.contains("\"rust_files\":8"));
    assert!(text.contains("\"unsupported_files\":3"));
    assert!(!text.contains("/Users/"));
}

fn telemetry_temp_root(name: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("squeezy-telemetry-{name}-{nonce}"))
}
