use super::*;
use crate::capture::EvalEvent;
use crate::scenario::Scenario;

fn empty_scenario() -> Scenario {
    toml::from_str(
        r#"
id = "t"
title = "t"

[workspace]
local = "/tmp/repo"
"#,
    )
    .unwrap()
}

fn ctx_from_events(events: Vec<EvalEvent>) -> TraceContext {
    use std::collections::BTreeMap;
    let mut tool_calls_by_turn: BTreeMap<String, Vec<(String, String, u64)>> = BTreeMap::new();
    let mut turn_failures = Vec::new();
    let mut action_steps = Vec::new();
    let mut approvals = Vec::new();
    let mut total_input_tokens = 0u64;
    let mut turn_count = 0u64;
    for event in &events {
        match &event.kind {
            EvalEventKind::TurnStarted => turn_count += 1,
            EvalEventKind::ToolCallStarted { call } => {
                let turn = event.turn_id.clone().unwrap_or_default();
                let name = call
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = call
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let args_str = serde_json::to_string(&args).unwrap();
                let sha = sha256_hex(args_str.as_bytes());
                tool_calls_by_turn
                    .entry(turn)
                    .or_default()
                    .push((name, sha, event.sequence));
            }
            EvalEventKind::TurnFailed { error } => {
                turn_failures.push((event.sequence, error.clone()));
            }
            EvalEventKind::ActionStep { action, status } => {
                let kind = action
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                action_steps.push((event.sequence, kind, status.clone()));
            }
            EvalEventKind::Approval { decision, .. } => {
                approvals.push((event.sequence, decision.clone()));
            }
            EvalEventKind::TurnCompleted { cost, .. } => {
                if let Some(v) = cost.get("input_tokens").and_then(|v| v.as_u64()) {
                    total_input_tokens += v;
                }
            }
            _ => {}
        }
    }
    TraceContext {
        events,
        tool_calls_by_turn,
        turn_failures,
        action_steps,
        approvals,
        total_input_tokens,
        wall_clock_ms: 0,
        turn_count,
        last_assistant_text: String::new(),
        tool_error_count: 0,
    }
}

fn tool_call(seq: u64, turn: &str, name: &str, args: serde_json::Value) -> EvalEvent {
    EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: seq,
        turn_id: Some(turn.to_string()),
        kind: EvalEventKind::ToolCallStarted {
            call: serde_json::json!({"name": name, "arguments": args}),
        },
    }
}

#[test]
fn duplicate_tool_call_flags_two_identical() {
    let events = vec![
        tool_call(1, "T(1)", "grep", serde_json::json!({"pattern": "x"})),
        tool_call(2, "T(1)", "grep", serde_json::json!({"pattern": "x"})),
        tool_call(3, "T(1)", "grep", serde_json::json!({"pattern": "y"})),
    ];
    let ctx = ctx_from_events(events);
    let s = empty_scenario();
    let out = DuplicateToolCall.check(&ctx, &s);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].rule_id, "duplicate_tool_call");
    assert!(out[0].summary.contains("2 times"));
}

#[test]
fn stale_function_call_output_matches_provider_error() {
    let events = vec![EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: 5,
        turn_id: Some("T(2)".into()),
        kind: EvalEventKind::TurnFailed {
            error: "provider request failed: 400 Bad Request: No tool call found for function call output with call_id call_xyz."
                .into(),
        },
    }];
    let ctx = ctx_from_events(events);
    let out = StaleFunctionCallOutput.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].severity, Severity::Critical);
}

#[test]
fn repeated_turn_failure_flags_byte_equal_errors() {
    let events = vec![
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 10,
            turn_id: Some("T(2)".into()),
            kind: EvalEventKind::TurnFailed {
                error: "boom".into(),
            },
        },
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 11,
            turn_id: Some("T(3)".into()),
            kind: EvalEventKind::TurnFailed {
                error: "boom".into(),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    let out = RepeatedTurnFailure.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].evidence.len(), 2);
}
