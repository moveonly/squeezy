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
    let mut assistant_text_by_turn: BTreeMap<String, String> = BTreeMap::new();
    let mut turn_finish_states: BTreeMap<String, (u64, Option<squeezy_llm::StopReason>, bool)> =
        BTreeMap::new();
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
            EvalEventKind::AssistantDelta { delta } => {
                if let Some(turn) = &event.turn_id {
                    assistant_text_by_turn
                        .entry(turn.clone())
                        .or_default()
                        .push_str(delta);
                }
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
            EvalEventKind::TurnCompleted {
                cost,
                stop_reason,
                reasoning_only_stop,
                ..
            } => {
                if let Some(v) = cost.get("input_tokens").and_then(|v| v.as_u64()) {
                    total_input_tokens += v;
                }
                if let Some(turn) = &event.turn_id {
                    turn_finish_states.insert(
                        turn.clone(),
                        (event.sequence, stop_reason.clone(), *reasoning_only_stop),
                    );
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
        assistant_text_by_turn,
        turn_finish_states,
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
fn redundant_graph_lookup_quiet_for_definition_plus_context_pair() {
    let events = vec![
        tool_call(
            1,
            "T(1)",
            "definition_search",
            serde_json::json!({"query": "Agent"}),
        ),
        tool_call(
            2,
            "T(1)",
            "symbol_context",
            serde_json::json!({"query": "Agent", "max_results": 1}),
        ),
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
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
fn expensive_short_answer_over_fetch_flags_costly_short_answer() {
    let mut events = Vec::new();
    for seq in 1..=6 {
        events.push(tool_call(
            seq,
            "T(1)",
            if seq % 2 == 0 {
                "definition_search"
            } else {
                "grep"
            },
            serde_json::json!({"query": "Agent", "seq": seq}),
        ));
    }
    events.push(EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: 7,
        turn_id: Some("T(1)".into()),
        kind: EvalEventKind::TurnCompleted {
            metrics: serde_json::json!({}),
            cost: serde_json::json!({"output_tokens": 1300, "input_tokens": 69000}),
            stop_reason: None,
            reasoning_only_stop: false,
            message: None,
            response_id: None,
            context_estimate: None,
        },
    });
    let ctx = ctx_from_events(events);
    let out = ExpensiveShortAnswerOverFetch.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert!(out[0].summary.contains("69000 input tokens"));
    assert!(out[0].summary.contains("6 tool calls"));
}

#[test]
fn expensive_short_answer_over_fetch_quiet_under_token_threshold() {
    let events = vec![
        tool_call(1, "T(1)", "grep", serde_json::json!({"query": "A"})),
        tool_call(2, "T(1)", "grep", serde_json::json!({"query": "B"})),
        tool_call(3, "T(1)", "grep", serde_json::json!({"query": "C"})),
        tool_call(4, "T(1)", "grep", serde_json::json!({"query": "D"})),
        tool_call(5, "T(1)", "grep", serde_json::json!({"query": "E"})),
        tool_call(6, "T(1)", "grep", serde_json::json!({"query": "F"})),
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 7,
            turn_id: Some("T(1)".into()),
            kind: EvalEventKind::TurnCompleted {
                metrics: serde_json::json!({}),
                cost: serde_json::json!({"output_tokens": 1300, "input_tokens": 49000}),
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(
        ExpensiveShortAnswerOverFetch
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
            stop_reason: None,
            reasoning_only_stop: false,
            message: None,
            response_id: None,
            context_estimate: None,
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
            stop_reason: None,
            reasoning_only_stop: false,
            message: None,
            response_id: None,
            context_estimate: None,
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(UngroundedCitation.check(&ctx, &empty_scenario()).is_empty());
}

fn scenario_with_synthetic_render_prompt() -> Scenario {
    toml::from_str(
        r#"
id = "render"
title = "render"
description = "Terminal rendering fixture"
[workspace]
local = "/tmp/r"
[[steps]]
kind = "prompt"
text = "Do not inspect files. Produce a compact markdown sample for testing terminal rendering with paths and exact_syntax labels."
"#,
    )
    .unwrap()
}

fn scenario_with_ux_render_prompt_without_file_ban() -> Scenario {
    toml::from_str(
        r#"
id = "ux-render"
title = "ux-render"
description = "Offline fixture that renders a compact UX report"
[workspace]
local = "/tmp/r"
[[steps]]
kind = "prompt"
text = "Render a compact UX report with paths and exact_syntax labels."
"#,
    )
    .unwrap()
}

fn scenario_with_scripted_shell_denial() -> Scenario {
    toml::from_str(
        r#"
id = "deny"
title = "deny"

[workspace]
local = "/tmp/repo"

[[steps]]
kind = "action"
action = "deny"
match = { tool = "shell" }
reason = "expected denial"
"#,
    )
    .unwrap()
}

#[test]
fn ungrounded_citation_quiet_for_synthetic_render_sample() {
    let events = vec![
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 5,
            turn_id: Some("T(2)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "| Path |\n|---|\n| `/var/tmp/render/sample.md` |".into(),
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(
        UngroundedCitation
            .check(&ctx, &scenario_with_synthetic_render_prompt())
            .is_empty()
    );
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
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
            stop_reason: None,
            reasoning_only_stop: false,
            message: None,
            response_id: None,
            context_estimate: None,
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
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

#[test]
fn exact_syntax_without_source_quiet_for_synthetic_render_sample() {
    let events = vec![
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 4,
            turn_id: Some("T(3)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "Render label text `exact_syntax` in a synthetic table.".into(),
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(
        ExactSyntaxWithoutSource
            .check(&ctx, &scenario_with_synthetic_render_prompt())
            .is_empty()
    );
}

#[test]
fn exact_syntax_without_source_quiet_for_ux_render_fixture_prompt() {
    let events = vec![
        EvalEvent {
            schema_version: 2,
            ts_unix_ms: 0,
            sequence: 4,
            turn_id: Some("T(3)".into()),
            kind: EvalEventKind::AssistantDelta {
                delta: "Render `exact_syntax` and label_missing in a UX table.".into(),
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
                stop_reason: None,
                reasoning_only_stop: false,
                message: None,
                response_id: None,
                context_estimate: None,
            },
        },
    ];
    let ctx = ctx_from_events(events);
    assert!(
        ExactSyntaxWithoutSource
            .check(&ctx, &scenario_with_ux_render_prompt_without_file_ban())
            .is_empty()
    );
}

#[test]
fn denied_tool_call_ux_flags_denied_tool_completion() {
    let events = vec![EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: 8,
        turn_id: Some("T(2)".into()),
        kind: EvalEventKind::ToolCallCompleted {
            result: serde_json::json!({
                "tool_name": "shell",
                "status": "Denied",
                "error": "denied by scenario"
            }),
        },
    }];
    let ctx = ctx_from_events(events);
    let out = DeniedToolCallUx.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].rule_id, "denied_tool_call_ux");
    assert!(out[0].summary.contains("denial reason"));
}

#[test]
fn denied_tool_call_ux_quiet_when_scenario_scripts_denial() {
    let events = vec![EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: 8,
        turn_id: Some("T(2)".into()),
        kind: EvalEventKind::ToolCallCompleted {
            result: serde_json::json!({
                "tool_name": "shell",
                "status": "Denied",
                "error": "denied by scenario"
            }),
        },
    }];
    let ctx = ctx_from_events(events);
    assert!(
        DeniedToolCallUx
            .check(&ctx, &scenario_with_scripted_shell_denial())
            .is_empty()
    );
}

fn assistant_delta(seq: u64, turn: &str, text: &str) -> EvalEvent {
    EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: seq,
        turn_id: Some(turn.into()),
        kind: EvalEventKind::AssistantDelta { delta: text.into() },
    }
}

fn turn_failed(seq: u64, turn: &str, error: &str) -> EvalEvent {
    EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: seq,
        turn_id: Some(turn.into()),
        kind: EvalEventKind::TurnFailed {
            error: error.into(),
        },
    }
}

fn turn_completed_with(
    seq: u64,
    turn: &str,
    finish_reason: Option<&str>,
    reasoning_only_stop: bool,
) -> EvalEvent {
    // Translate the chat-completions style string the test writes
    // into the normalized `StopReason` the eval pipeline now stores.
    // Mirrors `compatible.rs::chat_stop_reason` so tests stay
    // independent of the live provider.
    let stop_reason = finish_reason.map(|raw| match raw {
        "stop" => squeezy_llm::StopReason::EndTurn,
        "tool_calls" | "function_call" => squeezy_llm::StopReason::ToolUse,
        "length" => squeezy_llm::StopReason::MaxTokens,
        "content_filter" => squeezy_llm::StopReason::Refusal,
        other => squeezy_llm::StopReason::Other(other.to_string()),
    });
    EvalEvent {
        schema_version: 2,
        ts_unix_ms: 0,
        sequence: seq,
        turn_id: Some(turn.into()),
        kind: EvalEventKind::TurnCompleted {
            metrics: serde_json::json!({}),
            cost: serde_json::json!({}),
            stop_reason,
            reasoning_only_stop,
            message: None,
            response_id: None,
            context_estimate: None,
        },
    }
}

#[test]
fn max_tokens_turn_failure_flags_failed_turn() {
    let events = vec![turn_failed(
        8,
        "T(1)",
        "agent error: model response stopped after max_tokens before completing",
    )];
    let ctx = ctx_from_events(events);
    let out = MaxTokensTurnFailure.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].rule_id, "max_tokens_turn_failure");
}

#[test]
fn glued_progress_preamble_flags_concatenated_status_text() {
    let events = vec![
        tool_call(1, "T(1)", "repo_map", serde_json::json!({})),
        tool_call(2, "T(1)", "read_slice", serde_json::json!({"path": "x"})),
        assistant_delta(
            3,
            "T(1)",
            "Locating `Agent`...Finding methods...Reading source...Checking references...",
        ),
    ];
    let ctx = ctx_from_events(events);
    let out = GluedProgressPreamble.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].category, "ux");
}

#[test]
fn failed_turn_missing_cost_flags_tool_heavy_failed_turn() {
    let events = vec![
        tool_call(1, "T(1)", "repo_map", serde_json::json!({})),
        tool_call(2, "T(1)", "read_slice", serde_json::json!({"path": "x"})),
        tool_call(3, "T(1)", "grep", serde_json::json!({"pattern": "x"})),
        turn_failed(4, "T(1)", "response truncated by max_tokens"),
    ];
    let ctx = ctx_from_events(events);
    let out = FailedTurnMissingCost.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].rule_id, "failed_turn_missing_cost");
}

#[test]
fn stop_with_intent_text_fires_on_chatty_preamble_then_stop() {
    let events = vec![
        assistant_delta(
            1,
            "T(1)",
            "Let me scan the codebase to find a good candidate for modernization.",
        ),
        turn_completed_with(2, "T(1)", Some("stop"), false),
    ];
    let ctx = ctx_from_events(events);
    let out = StopWithIntentTextNoToolCall.check(&ctx, &empty_scenario());
    assert_eq!(out.len(), 1, "should flag the intent-text stop pattern");
    assert_eq!(out[0].rule_id, "stop_with_intent_text_no_tool_call");
}

#[test]
fn stop_with_intent_text_quiet_when_tool_call_fired() {
    let events = vec![
        tool_call(1, "T(1)", "grep", serde_json::json!({"pattern": "x"})),
        assistant_delta(
            2,
            "T(1)",
            "Let me scan the codebase to find a good candidate.",
        ),
        turn_completed_with(3, "T(1)", Some("stop"), false),
    ];
    let ctx = ctx_from_events(events);
    assert!(
        StopWithIntentTextNoToolCall
            .check(&ctx, &empty_scenario())
            .is_empty(),
        "intent text plus an actual tool call is not the bug"
    );
}

#[test]
fn stop_with_intent_text_quiet_on_pure_chitchat() {
    let events = vec![
        assistant_delta(1, "T(1)", "Hello there, happy to help."),
        turn_completed_with(2, "T(1)", Some("stop"), false),
    ];
    let ctx = ctx_from_events(events);
    assert!(
        StopWithIntentTextNoToolCall
            .check(&ctx, &empty_scenario())
            .is_empty(),
        "no intent verb means no flag"
    );
}

#[test]
fn stop_with_intent_text_quiet_on_non_stop_finish() {
    // Even if intent text is present and no tool call fired, a
    // non-`stop` finish reason is a different bug class — the rule
    // should not claim those.
    let events = vec![
        assistant_delta(1, "T(1)", "Let me scan the source for the issue."),
        turn_completed_with(2, "T(1)", Some("length"), false),
    ];
    let ctx = ctx_from_events(events);
    assert!(
        StopWithIntentTextNoToolCall
            .check(&ctx, &empty_scenario())
            .is_empty()
    );
}

#[test]
fn expect_finish_reason_not_flags_literal_match() {
    // The mock helper accepts the chat-completions wire string
    // ("length"), but the rule now matches against the normalized
    // `StopReason` label ("max_tokens"). Scenarios written after the
    // StopReason migration use the normalized label directly.
    let events = vec![turn_completed_with(1, "T(1)", Some("length"), false)];
    let ctx = ctx_from_events(events);
    let mut scenario = empty_scenario();
    scenario.expect.finish_reason_not = vec!["max_tokens".into()];
    let out = ExpectFinishReasonNot.check(&ctx, &scenario);
    assert_eq!(out.len(), 1);
    assert!(out[0].summary.contains("max_tokens"));
}

#[test]
fn expect_finish_reason_not_flags_stop_no_action_sentinel() {
    // No tool call AND no assistant text — pure "model said nothing".
    let events = vec![turn_completed_with(1, "T(1)", Some("stop"), false)];
    let ctx = ctx_from_events(events);
    let mut scenario = empty_scenario();
    scenario.expect.finish_reason_not = vec!["stop_no_action".into()];
    let out = ExpectFinishReasonNot.check(&ctx, &scenario);
    assert_eq!(
        out.len(),
        1,
        "stop with zero tool calls AND no assistant text is forbidden"
    );
}

#[test]
fn expect_finish_reason_not_quiet_when_stop_had_tool_call() {
    let events = vec![
        tool_call(1, "T(1)", "grep", serde_json::json!({"pattern": "x"})),
        turn_completed_with(2, "T(1)", Some("stop"), false),
    ];
    let ctx = ctx_from_events(events);
    let mut scenario = empty_scenario();
    scenario.expect.finish_reason_not = vec!["stop_no_action".into()];
    assert!(
        ExpectFinishReasonNot.check(&ctx, &scenario).is_empty(),
        "stop with a tool call is the OK path"
    );
}

#[test]
fn expect_finish_reason_not_quiet_when_stop_has_assistant_text() {
    // Plan-mode-style turn: no tool call but a real assistant message
    // (the `<proposed_plan>` block). The sentinel must NOT fire here —
    // the model emitted actionable output, just not via a tool call.
    let events = vec![
        assistant_delta(
            1,
            "T(1)",
            "<proposed_plan>\n## Context\nfoo\n</proposed_plan>",
        ),
        turn_completed_with(2, "T(1)", Some("stop"), false),
    ];
    let ctx = ctx_from_events(events);
    let mut scenario = empty_scenario();
    scenario.expect.finish_reason_not = vec!["stop_no_action".into()];
    assert!(
        ExpectFinishReasonNot.check(&ctx, &scenario).is_empty(),
        "stop with assistant text is the OK path even without tool calls"
    );
}
