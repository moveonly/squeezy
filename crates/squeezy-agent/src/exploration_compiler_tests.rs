use super::*;

#[test]
fn definition_prompt_compiles_to_graph_first_tools() {
    let plan = compile_exploration_plan("Which file defines make_widget?").expect("plan");

    assert_eq!(plan.intent, ExplorationIntent::FindDefinition);
    assert_eq!(plan.query.as_deref(), Some("make_widget"));
    assert_eq!(
        plan.calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        vec!["definition_search", "symbol_context"]
    );
    assert!(plan.guard_raw_reads);
}

#[test]
fn callers_prompt_uses_upstream_flow() {
    let plan = compile_exploration_plan("Who calls Runner::run?").expect("plan");

    assert_eq!(plan.intent, ExplorationIntent::FindCallers);
    assert_eq!(plan.query.as_deref(), Some("Runner::run"));
    assert!(plan.calls.iter().any(|call| call.name == "upstream_flow"));
}

#[test]
fn ambiguous_prompt_does_not_compile() {
    assert!(compile_exploration_plan("Please explain the tradeoff").is_none());
}

#[test]
fn raw_read_guard_lifts_after_graph_evidence() {
    let plan = compile_exploration_plan("Where is make_widget?").expect("plan");
    let mut state = ExplorationTurnState::from_plan(Some(&plan));
    let read = ToolCall {
        call_id: "read".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({"path": "src/lib.rs"}),
    };

    assert!(state.read_denial_reason(&read).is_some());

    state.record_tool_result("symbol_context", true);
    assert!(state.read_denial_reason(&read).is_none());
}
