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
            EvalEventKind::ToolCallStarted { call, .. } => {
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
            origin: "model".to_string(),
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

#[test]
fn redundant_graph_lookup_flags_cross_tool_overlap() {
    // Same query, two different graph tools — `args_sha256` would not
    // dedupe (args differ between calls), but the semantic question is
    // the same.
    let events = vec![
        tool_call(
            1,
            "T(1)",
            "decl_search",
            serde_json::json!({"query": "Command", "kind": "method"}),
        ),
        tool_call(
            2,
            "T(1)",
            "symbol_context",
            serde_json::json!({"query": "Command", "max_results": 8}),
        ),
    ];
    let ctx = ctx_from_events(events);
    let out = RedundantGraphLookup.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert!(out[0].summary.contains("decl_search"));
    assert!(out[0].summary.contains("symbol_context"));
    assert_eq!(out[0].evidence.len(), 2);
}

#[test]
fn redundant_graph_lookup_quiet_for_single_tool() {
    // Two calls to the *same* tool — `duplicate_tool_call` owns this.
    let events = vec![
        tool_call(1, "T(1)", "decl_search", serde_json::json!({"query": "X"})),
        tool_call(2, "T(1)", "decl_search", serde_json::json!({"query": "X"})),
    ];
    let ctx = ctx_from_events(events);
    let out = RedundantGraphLookup.check(&ctx, &empty_scenario());
    assert!(out.is_empty());
}

#[test]
fn deep_chain_expansion_flags_six_reads_and_grep() {
    let events = vec![
        tool_call(1, "T(1)", "read_slice", serde_json::json!({"path": "a.rs"})),
        tool_call(2, "T(1)", "read_slice", serde_json::json!({"path": "b.rs"})),
        tool_call(3, "T(1)", "grep", serde_json::json!({"pattern": "x"})),
        tool_call(4, "T(1)", "read_slice", serde_json::json!({"path": "c.rs"})),
        tool_call(5, "T(1)", "read_slice", serde_json::json!({"path": "d.rs"})),
    ];
    let ctx = ctx_from_events(events);
    let out = DeepChainExpansion.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert!(out[0].summary.contains("5 chain-trace calls"));
}

#[test]
fn deep_chain_expansion_quiet_under_threshold() {
    let events = vec![
        tool_call(1, "T(1)", "read_slice", serde_json::json!({"path": "a.rs"})),
        tool_call(2, "T(1)", "read_slice", serde_json::json!({"path": "b.rs"})),
    ];
    let ctx = ctx_from_events(events);
    let out = DeepChainExpansion.check(&ctx, &empty_scenario());
    assert!(out.is_empty());
}

#[test]
fn heavy_and_targeted_redundant_flags_repo_map_with_read_slice() {
    let events = vec![
        tool_call(1, "T(1)", "repo_map", serde_json::json!({"max_depth": 4})),
        tool_call(
            2,
            "T(1)",
            "read_slice",
            serde_json::json!({"path": "x.rs", "start_line": 100, "end_line": 130}),
        ),
    ];
    let ctx = ctx_from_events(events);
    let out = HeavyAndTargetedRedundant.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert!(out[0].summary.contains("repo_map"));
}

#[test]
fn heavy_and_targeted_redundant_quiet_for_pure_targeted() {
    let events = vec![
        tool_call(1, "T(1)", "read_slice", serde_json::json!({"path": "x.rs"})),
        tool_call(2, "T(1)", "read_slice", serde_json::json!({"path": "y.rs"})),
    ];
    let ctx = ctx_from_events(events);
    assert!(
        HeavyAndTargetedRedundant
            .check(&ctx, &empty_scenario())
            .is_empty()
    );
}

#[test]
fn trivial_answer_over_fetch_flags_short_output_with_burst() {
    let events = vec![
        tool_call(
            1,
            "T(1)",
            "definition_search",
            serde_json::json!({"query": "X"}),
        ),
        tool_call(
            2,
            "T(1)",
            "symbol_context",
            serde_json::json!({"query": "X"}),
        ),
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 3,
            turn_id: Some("T(1)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({"output_tokens": 6, "input_tokens": 9000}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    let out = TrivialAnswerOverFetch.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert!(out[0].summary.contains("2 tool calls"));
    assert!(out[0].summary.contains("6 output tokens"));
}

#[test]
fn trivial_answer_over_fetch_quiet_for_long_answers() {
    let events = vec![
        tool_call(
            1,
            "T(1)",
            "definition_search",
            serde_json::json!({"query": "X"}),
        ),
        tool_call(
            2,
            "T(1)",
            "symbol_context",
            serde_json::json!({"query": "X"}),
        ),
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 3,
            turn_id: Some("T(1)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({"output_tokens": 500, "input_tokens": 9000}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(
        TrivialAnswerOverFetch
            .check(&ctx, &empty_scenario())
            .is_empty()
    );
}

#[test]
fn expect_input_tokens_per_turn_flags_one_turn() {
    let events = vec![EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: 9,
        turn_id: Some("T(3)".into()),
        kind: EvalEventKind::TurnCompleted {
            metrics: serde_json::json!({}),
            cost: serde_json::json!({"input_tokens": 250000, "output_tokens": 200}),
        },
    }];
    let ctx = ctx_from_events(events);
    let mut scenario = empty_scenario();
    scenario.expect.max_input_tokens_per_turn = Some(100_000);
    let out = ExpectationsAsFindings.check(&ctx, &scenario);
    assert!(
        out.iter()
            .any(|f| f.rule_id == "expect_input_tokens_per_turn")
    );
}

#[test]
fn expect_input_tokens_per_turn_quiet_when_unset() {
    let events = vec![EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: 9,
        turn_id: Some("T(3)".into()),
        kind: EvalEventKind::TurnCompleted {
            metrics: serde_json::json!({}),
            cost: serde_json::json!({"input_tokens": 250000}),
        },
    }];
    let ctx = ctx_from_events(events);
    let scenario = empty_scenario(); // max_input_tokens_per_turn unset
    let out = ExpectationsAsFindings.check(&ctx, &scenario);
    assert!(
        !out.iter()
            .any(|f| f.rule_id == "expect_input_tokens_per_turn")
    );
}

#[test]
fn ungrounded_citation_flags_zero_tools_with_path() {
    let events = vec![
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 5,
            turn_id: Some("T(2)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "Look at `tests/foo/bar.py` line 42 for the answer.".into(),
            },
        },
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 6,
            turn_id: Some("T(2)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({"input_tokens": 9000, "output_tokens": 20}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    let out = UngroundedCitation.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert!(out[0].summary.contains("0 tool calls"));
}

#[test]
fn ungrounded_citation_quiet_when_tool_call_ran() {
    let events = vec![
        tool_call(1, "T(2)", "read_slice", serde_json::json!({"path": "x"})),
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 5,
            turn_id: Some("T(2)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "See `tests/foo/bar.py`.".into(),
            },
        },
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 6,
            turn_id: Some("T(2)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({"input_tokens": 9000, "output_tokens": 20}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(UngroundedCitation.check(&ctx, &empty_scenario()).is_empty());
}

#[test]
fn ungrounded_citation_quiet_for_no_path() {
    let events = vec![
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 5,
            turn_id: Some("T(2)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "Yes, the answer is 42.".into(),
            },
        },
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 6,
            turn_id: Some("T(2)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({"input_tokens": 9000, "output_tokens": 5}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(UngroundedCitation.check(&ctx, &empty_scenario()).is_empty());
}

fn scenario_with_label_prompt() -> Scenario {
    toml::from_str(
        r#"
id = "labels"
title = "labels"
[workspace]
local = "/tmp/r"
[[steps]]
kind = "prompt"
text = "Cite each piece of evidence with a confidence label (exact_syntax, import_resolved, ...)"
"#,
    )
    .unwrap()
}

#[test]
fn incomplete_confidence_labels_flags_partial_compliance() {
    let events = vec![
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 5,
            turn_id: Some("T(1)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "- claim one [exact_syntax]\n- claim two\n- claim three\n- claim four\n"
                    .into(),
            },
        },
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 6,
            turn_id: Some("T(1)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    let out = IncompleteConfidenceLabels.check(&ctx, &scenario_with_label_prompt());
    assert_eq!(out.len(), 1);
    assert!(
        out[0]
            .summary
            .contains("1 label tag(s) found for 4 claim-shaped")
    );
}

#[test]
fn incomplete_confidence_labels_quiet_when_no_prompt_asks() {
    let events = vec![
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 5,
            turn_id: Some("T(1)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "- claim one\n- claim two\n- claim three\n".into(),
            },
        },
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 6,
            turn_id: Some("T(1)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    // empty_scenario has no prompt that asks for labels.
    assert!(
        IncompleteConfidenceLabels
            .check(&ctx, &empty_scenario())
            .is_empty()
    );
}

#[test]
fn incomplete_confidence_labels_quiet_when_all_labeled() {
    let events = vec![EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: 5,
        turn_id: Some("T(1)".into()),
        kind: EvalEventKind::AssistantDelta {
            delta: "- claim one [exact_syntax]\n- claim two [import_resolved]\n- claim three [unknown]\n".into(),
        },
    }, EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: 6,
        turn_id: Some("T(1)".into()),
        kind: EvalEventKind::TurnCompleted {
            metrics: serde_json::json!({}),
            cost: serde_json::json!({}),
        },
    }];
    let ctx = ctx_from_events(events);
    assert!(
        IncompleteConfidenceLabels
            .check(&ctx, &scenario_with_label_prompt())
            .is_empty()
    );
}

#[test]
fn exact_syntax_without_source_flags_label_without_read_slice() {
    let events = vec![
        tool_call(
            1,
            "T(3)",
            "definition_search",
            serde_json::json!({"query": "X"}),
        ),
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 4,
            turn_id: Some("T(3)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "The decorator is Typer.command [exact_syntax].".into(),
            },
        },
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 5,
            turn_id: Some("T(3)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    let out = ExactSyntaxWithoutSource.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert!(out[0].summary.contains("never called `read_slice`"));
}

#[test]
fn exact_syntax_without_source_quiet_when_read_slice_ran() {
    let events = vec![
        tool_call(
            1,
            "T(3)",
            "read_slice",
            serde_json::json!({"path": "x.py", "start_line": 1, "end_line": 20}),
        ),
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 4,
            turn_id: Some("T(3)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "[exact_syntax] decorator is X".into(),
            },
        },
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 5,
            turn_id: Some("T(3)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({}),
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(
        ExactSyntaxWithoutSource
            .check(&ctx, &empty_scenario())
            .is_empty()
    );
}
