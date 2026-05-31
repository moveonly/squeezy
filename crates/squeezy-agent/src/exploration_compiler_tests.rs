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
        vec!["definition_search"],
        "plain definition lookup should not preflight relationship context"
    );
    assert!(plan.guard_raw_reads);
}

#[test]
fn list_methods_prompt_issues_single_symbol_context_call() {
    let plan = compile_exploration_plan("list methods on Widget").expect("plan");

    assert_eq!(plan.intent, ExplorationIntent::MethodListing);
    assert_eq!(plan.query.as_deref(), Some("Widget"));
    assert_eq!(
        plan.calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        vec!["symbol_context"],
        "list-methods should not fan out to flow/definition tools"
    );
    assert!(plan.guard_raw_reads);
}

#[test]
fn what_methods_does_have_phrasing_also_compiles_to_method_listing() {
    let plan = compile_exploration_plan("what methods does Widget have?").expect("plan");
    assert_eq!(plan.intent, ExplorationIntent::MethodListing);
    assert_eq!(plan.query.as_deref(), Some("Widget"));
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

#[test]
fn path_shaped_token_does_not_trigger_planner_preflight() {
    // Prompts that mention a repo path like `spf13/cobra` must not seed a
    // `definition_search(query="spf13/cobra")` preflight: the graph index
    // is keyed on symbols, not file paths, so the call returns ~9.5 KB of
    // unrelated data the model immediately ignores.
    assert!(
        compile_exploration_plan("Which file defines the root command in spf13/cobra?").is_none()
    );
    assert!(
        compile_exploration_plan("Where does the sink registry live under include/spdlog/sinks/?")
            .is_none()
    );
}

#[test]
fn source_file_extension_token_does_not_trigger_planner_preflight() {
    // A bare filename like `main.go` is also path-shaped and must be
    // rejected by the same gate.
    assert!(compile_exploration_plan("Which file defines main.go?").is_none());
}
#[test]
fn prompt_noise_words_are_rejected_by_is_useful_query() {
    // Capitalized English prompt scaffolding (`ONLY`, `OUTPUT`, `EXPECTED`,
    // ...) was being treated as a Rust-y symbol by `looks_like_rust_symbol`
    // (uppercase first char), so the planner fired `symbol_context "ONLY"`
    // and the like. The `is_useful_query` gate must reject these before they
    // ever reach query extraction.
    for noise in [
        "ONLY", "TODO", "NOTE", "OUTPUT", "RETURN", "ERROR", "WARNING", "STOP", "EXACTLY", "MUST",
        "EXPECT", "EXPECTED", "ACTUAL", "INPUT", "testing", "Only", "output",
    ] {
        assert!(
            !is_useful_query(noise),
            "is_useful_query({noise:?}) must be false but was true"
        );
    }

    // Sanity: a real identifier still passes.
    assert!(is_useful_query("Runner"));
    assert!(is_useful_query("make_widget"));
}

#[test]
fn capitalized_noise_word_does_not_drive_planner_query() {
    // Real-world Python prompts include phrases like "Output ONLY the file
    // path". Before the noise-word reject, the planner extracted `ONLY` (or
    // `Output`) as a Rust-y symbol and emitted `symbol_context "ONLY"`. With
    // the reject in place there is no symbolic token left, so the planner
    // either falls through entirely or finds a real identifier elsewhere.
    let plan =
        compile_exploration_plan("Which file defines the helper? Output ONLY the file path.");
    if let Some(plan) = plan {
        assert_ne!(
            plan.query.as_deref(),
            Some("ONLY"),
            "planner picked the noise word `ONLY` as a query"
        );
        assert_ne!(
            plan.query.as_deref(),
            Some("Output"),
            "planner picked the noise word `Output` as a query"
        );
    }
}

#[test]
fn quoted_noise_word_is_also_rejected() {
    // The quoted-literal path also runs through `is_useful_query`, so a
    // prompt that literally quotes a noise word must not compile to a plan
    // that drives a graph query on it.
    let plan = compile_exploration_plan("Where is `ONLY` defined?");
    if let Some(plan) = plan {
        assert_ne!(plan.query.as_deref(), Some("ONLY"));
    }
}

#[test]
fn planner_graph_max_results_caps_above_realistic_subclass_fanout() {
    // Real-world hierarchies (e.g. all `WidgetsBindingObserver` subclasses
    // in a Flutter app) reliably produce 15+ siblings. The cap must clear
    // that headroom by a wide margin so the planner doesn't silently
    // truncate the tail before the model ever sees it.
    const { assert!(PLANNER_GRAPH_MAX_RESULTS >= 32) };
}

#[test]
fn planner_calls_use_the_shared_graph_max_results_constant() {
    // Pin every planner-emitted graph call to the shared cap. If a future
    // edit hard-codes a smaller integer (the original bug was a literal
    // `8`), this assertion fires before recall regresses.
    let cap = PLANNER_GRAPH_MAX_RESULTS as u64;
    let prompts = [
        "Which file defines make_widget?",
        "list methods on Widget",
        "Who calls Runner::run?",
        "What is the change impact of Runner::run?",
        "Find tests for Runner::run coverage",
    ];
    for prompt in prompts {
        let plan = compile_exploration_plan(prompt)
            .unwrap_or_else(|| panic!("expected plan for prompt: {prompt}"));
        for call in &plan.calls {
            // Only assert on tools whose `max_results` the planner is
            // responsible for sizing; flow tools intentionally cap at 25.
            if !matches!(
                call.name.as_str(),
                "definition_search" | "decl_search" | "symbol_context" | "hierarchy"
            ) {
                continue;
            }
            let observed = call
                .arguments
                .get("max_results")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(|| {
                    panic!(
                        "planner call `{}` for prompt `{prompt}` missing max_results",
                        call.name
                    )
                });
            assert_eq!(
                observed, cap,
                "planner call `{}` for prompt `{prompt}` used max_results={observed}, expected the shared cap {cap}",
                call.name,
            );
        }
    }
}
