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

#[test]
fn raw_read_guard_lifts_after_preflight_even_without_success() {
    // Even if every planner-issued graph call returns a non-`Success` status
    // (error/stale/empty), the planner is advisory and its outputs are
    // already in the model's context. The turn loop calls
    // `mark_preflight_complete` after the preflight block runs to avoid
    // locking out every subsequent `read_file`.
    let plan = compile_exploration_plan("Where is make_widget?").expect("plan");
    let mut state = ExplorationTurnState::from_plan(Some(&plan));
    let read = ToolCall {
        call_id: "read".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({"path": "src/lib.rs"}),
    };

    assert!(state.read_denial_reason(&read).is_some());

    state.record_tool_result("definition_search", false);
    state.record_tool_result("symbol_context", false);
    assert!(
        state.read_denial_reason(&read).is_some(),
        "non-success graph results alone must not lift the guard"
    );

    state.mark_preflight_complete();
    assert!(
        state.read_denial_reason(&read).is_none(),
        "preflight completion must lift the guard so the turn can continue"
    );
}

#[test]
fn contraction_does_not_yield_junk_query() {
    // The first apostrophe in `What's` is not a paired quote opener, so the
    // extractor must fall back to identifier extraction and pick `make_widget`
    // instead of the trailing fragment after the apostrophe.
    let plan = compile_exploration_plan("What's the definition of make_widget?").expect("plan");
    assert_eq!(plan.query.as_deref(), Some("make_widget"));
}

#[test]
fn conversational_where_does_does_not_compile_plan() {
    // `where does` matches `definition_intent` but the prompt has no Rust-y
    // symbol to query, so the planner should fall through rather than compile
    // a plan with a garbage query.
    assert!(compile_exploration_plan("Where does this PR go?").is_none());
    assert!(compile_exploration_plan("Where is the bug?").is_none());
}

#[test]
fn quoted_literal_bypasses_symbol_heuristic() {
    // Properly quoted user input is treated as explicit intent and is allowed
    // through even when it doesn't look like a Rust-y identifier.
    let plan = compile_exploration_plan("Which file defines `the thing`?").expect("plan");
    assert_eq!(plan.query.as_deref(), Some("the thing"));
}

#[test]
fn intent_precedence_repo_map_wins_over_definition() {
    // `repo_map > test_pairing > change_impact > callers > route > definition`.
    // A prompt that mentions both `repository map` and `defines` should
    // compile to the repo-map plan, not the definition plan.
    let plan =
        compile_exploration_plan("Show me the repository map that defines Runner").expect("plan");
    assert_eq!(plan.intent, ExplorationIntent::RepoMap);
}

#[test]
fn identifier_extractor_prefers_rust_like_token_over_trailing_word() {
    // Without the rust-symbol preference, `extract_identifier` would pick
    // the trailing `main` token. The Rust-style `Runner::run` should win.
    let plan = compile_exploration_plan("Who calls Runner::run from main?").expect("plan");
    assert_eq!(plan.query.as_deref(), Some("Runner::run"));
}
