use super::*;
use serde_json::json;
use squeezy_core::{AppConfig, ContextCompactionConfig};
use squeezy_llm::LlmInputItem;

fn read_file_pair(n: usize, body_len: usize) -> Vec<LlmInputItem> {
    let call_id = format!("call_{n:03}");
    let body = "x".repeat(body_len);
    vec![
        LlmInputItem::FunctionCall {
            call_id: call_id.clone(),
            name: "read_file".to_string(),
            arguments: json!({ "path": format!("file_{n}.rs") }),
        },
        LlmInputItem::FunctionCallOutput {
            call_id,
            output: body,
        },
    ]
}

fn config_with_micro(window: u64, threshold: u8, keep_recent: usize) -> AppConfig {
    AppConfig {
        context_compaction: ContextCompactionConfig {
            enabled_mid_turn: true,
            model_context_window: Some(window),
            // Keep the full-tier gate above the micro threshold so micro
            // can demonstrably fire before full.
            threshold_percent: 95,
            micro_compaction_enabled: true,
            micro_compaction_threshold_percent: threshold,
            micro_compaction_keep_recent: keep_recent,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    }
}

#[test]
fn micro_compact_preserves_recent_n() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let report = maybe_micro_compact_mid_turn(&mut conversation, &config, Some(9_000))
        .expect("micro-compaction should fire at saturation");
    assert_eq!(
        report.cleared_call_ids.len(),
        7,
        "expected 12 outputs minus 5 keep_recent to be cleared, got {}",
        report.cleared_call_ids.len(),
    );
    let outputs: Vec<&str> = conversation
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCallOutput { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(outputs.len(), 12);
    for output in &outputs[..7] {
        assert!(
            output.starts_with(MICRO_COMPACT_CLEARED_PREFIX),
            "older output should be cleared: {output}",
        );
    }
    for output in &outputs[7..] {
        assert!(
            !output.starts_with(MICRO_COMPACT_CLEARED_PREFIX),
            "recent output should be verbatim, got cleared marker: {output}",
        );
        assert_eq!(output.len(), 4_000);
    }
}

#[test]
fn micro_compact_preserves_message_structure() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    conversation.push(LlmInputItem::UserText("kickoff prompt".to_string()));
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    conversation.push(LlmInputItem::AssistantText("thinking".to_string()));
    let original_len = conversation.len();
    let _ = maybe_micro_compact_mid_turn(&mut conversation, &config, Some(9_000))
        .expect("micro-compaction should fire");
    assert_eq!(
        conversation.len(),
        original_len,
        "micro-compaction must not change the conversation length",
    );
    let mut calls = std::collections::BTreeSet::new();
    let mut outputs = std::collections::BTreeSet::new();
    for item in &conversation {
        match item {
            LlmInputItem::FunctionCall { call_id, .. } => {
                calls.insert(call_id.clone());
            }
            LlmInputItem::FunctionCallOutput { call_id, .. } => {
                outputs.insert(call_id.clone());
            }
            _ => {}
        }
    }
    assert_eq!(
        calls, outputs,
        "every function call must keep its output pair",
    );
}

#[test]
fn micro_compact_then_full_compact_works() {
    // After a micro pass clears the heavy bodies, the full-tier compaction
    // path can still run on the resulting conversation and produce its
    // summary head plus the recent slice.
    let mut config = config_with_micro(10_000, 50, 5);
    config.context_compaction.recent_items = 4;
    config.context_compaction.min_items = 1;
    config.context_compaction.estimated_tokens = 1;
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let micro = maybe_micro_compact_mid_turn(&mut conversation, &config, Some(9_000))
        .expect("micro should fire");
    assert!(micro.bytes_saved > 0);

    let mut state = squeezy_core::ContextCompactionState::default();
    let report = crate::context_compaction::compact_conversation(
        &mut conversation,
        &mut state,
        &[],
        None,
        &config,
        squeezy_core::ContextCompactionTrigger::Auto,
        true,
    )
    .expect("full compaction should fire");
    assert!(matches!(
        conversation.first(),
        Some(LlmInputItem::UserText(_)),
    ));
    assert!(
        report.record.dropped_items > 0,
        "full compaction should report dropped items",
    );
    assert!(conversation.len() <= 5);
}

#[test]
fn micro_compact_skips_when_disabled() {
    let mut config = config_with_micro(10_000, 50, 5);
    config.context_compaction.micro_compaction_enabled = false;
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let report = maybe_micro_compact_mid_turn(&mut conversation, &config, Some(9_000));
    assert!(report.is_none());
}

#[test]
fn micro_compact_skips_below_threshold() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let report = maybe_micro_compact_mid_turn(&mut conversation, &config, Some(1_000));
    assert!(report.is_none());
}

#[test]
fn micro_compact_skips_non_compactable_tools() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    for n in 0..12 {
        let call_id = format!("call_{n:03}");
        let body = "x".repeat(4_000);
        conversation.push(LlmInputItem::FunctionCall {
            call_id: call_id.clone(),
            name: "notes_recall".to_string(),
            arguments: json!({}),
        });
        conversation.push(LlmInputItem::FunctionCallOutput {
            call_id,
            output: body,
        });
    }
    let report = maybe_micro_compact_mid_turn(&mut conversation, &config, Some(9_000));
    assert!(
        report.is_none(),
        "notes_recall is not in the compactable set; nothing should clear",
    );
}

#[test]
fn micro_compact_is_idempotent_on_already_cleared_outputs() {
    let config = config_with_micro(10_000, 50, 5);
    let mut conversation = Vec::new();
    for n in 0..12 {
        conversation.extend(read_file_pair(n, 4_000));
    }
    let _first = maybe_micro_compact_mid_turn(&mut conversation, &config, Some(9_000))
        .expect("first pass should fire");
    let second = maybe_micro_compact_mid_turn(&mut conversation, &config, Some(9_000));
    assert!(
        second.is_none(),
        "second micro pass should be a no-op when older outputs already carry the placeholder",
    );
}
