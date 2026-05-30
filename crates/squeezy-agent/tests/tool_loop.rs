use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use futures_core::Stream;
use futures_util::stream;
use serde_json::Value;
use squeezy_agent::{Agent, AgentEvent, ToolApprovalDecision};
use squeezy_core::{
    AppConfig, ContextCompactionConfig, CostSnapshot, PermissionAction, PermissionMode,
    PermissionPolicy, PermissionRule, PermissionRuleSource, PermissionScope, Result, SessionMode,
    TurnId,
};
use squeezy_hooks::{HookContext, HookEvent, HookHandler, HookRegistry, HookResult};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall};
use squeezy_store::SqueezyStore;
use squeezy_tools::sha256_hex;
use tokio_util::sync::CancellationToken;

struct ScriptedProvider {
    name: &'static str,
    responses: Mutex<VecDeque<Vec<Result<LlmEvent>>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<Vec<Result<LlmEvent>>>) -> Self {
        Self::named("scripted", responses)
    }

    fn named(name: &'static str, responses: Vec<Vec<Result<LlmEvent>>>) -> Self {
        Self {
            name,
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .expect("scripted response");
        let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
            Box::pin(stream::iter(events));
        stream
    }
}

#[tokio::test]
async fn parallel_read_and_search_outputs_return_to_model_by_call_id() {
    let root = temp_workspace("parallel_read_search");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "grep_call".to_string(),
                name: "grep".to_string(),
                arguments: serde_json::json!({"pattern": "needle", "include": ["*.rs"]}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "src.rs"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn("inspect needle".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].tools.iter().all(|tool| !tool.strict));
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].0, "grep_call");
    assert_eq!(outputs[1].0, "read_call");
    assert!(outputs[0].1["content"]["matches"][0]["path"] == "src.rs");
    assert!(outputs[1].1["content"]["content"] == "fn needle() {}\n");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_mode_advertises_only_read_only_tools() {
    let root = temp_workspace("plan_tools");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("planned".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_final".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let mut config = config_for(root.clone());
    config.session_mode = SessionMode::Plan;
    config.permissions.edit = PermissionMode::Allow;
    config.permissions.shell = PermissionMode::Allow;
    config.permissions.web = PermissionMode::Allow;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("plan only".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let tool_names = tool_names(&requests[0]);
    assert_eq!(
        tool_names,
        vec![
            "delegate",
            "explore",
            "load_tool_schema",
            "request_user_input",
            "glob",
            "grep",
            "read_file",
            "read_tool_output",
            "decl_search",
            "definition_search",
            "diff_context",
            "downstream_flow",
            "hierarchy",
            "plan_patch",
            "read_slice",
            "reference_search",
            "repo_map",
            "symbol_context",
            "upstream_flow",
        ]
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn exploration_compiler_prefetches_graph_context_before_model_request() {
    let root = temp_workspace("exploration_preflight");
    write_rust_crate(&root, "pub fn make_widget() {}\n");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(
            "src/lib.rs defines make_widget.".to_string(),
        )),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_final".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn(
        "Which file defines make_widget?".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let outputs = function_outputs(&requests[0]);
    let call_ids = outputs
        .iter()
        .map(|(call_id, _)| *call_id)
        .collect::<Vec<_>>();
    assert_eq!(call_ids, vec!["planner_definition_search"]);
    assert!(
        outputs
            .iter()
            .any(|(_, output)| output["tool_name"] == "definition_search")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cited_final_answer_is_preserved() {
    let root = temp_workspace("cited_final_answer");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(
            "Use the current docs at https://example.com/docs.".to_string(),
        )),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_final".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(config_for(root.clone()), provider);

    let mut rx = agent.start_turn("answer with citation".to_string(), CancellationToken::new());
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { message, .. } = event {
            completed = Some(message.content);
        }
    }

    assert_eq!(
        completed.as_deref(),
        Some("Use the current docs at https://example.com/docs.")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn build_mode_advertises_core_tool_set_and_compact_index() {
    let root = temp_workspace("build_tools");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("building".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_final".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn("build".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let tool_names = tool_names(&requests[0]);
    for expected in ["load_tool_schema", "apply_patch", "write_file", "shell"] {
        assert!(
            tool_names.contains(&expected),
            "build mode should advertise {expected}: {tool_names:?}"
        );
    }
    for hidden in ["verify", "webfetch", "websearch"] {
        assert!(
            !tool_names.contains(&hidden),
            "build mode should leave {hidden} in the compact index: {tool_names:?}"
        );
    }
    assert!(requests[0].instructions.contains("<tools_index>"));
    assert!(requests[0].instructions.contains("webfetch"));
    assert!(requests[0].instructions.contains("verify"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn discoverable_tool_schema_load_appends_full_schema_for_later_rounds() {
    let root = temp_workspace("lazy_schema_load");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "load_websearch".to_string(),
                name: "load_tool_schema".to_string(),
                arguments: serde_json::json!({"name": "websearch"}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "load_webfetch".to_string(),
                name: "load_tool_schema".to_string(),
                arguments: serde_json::json!({"name": "webfetch"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_load".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("schemas loaded".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Allow;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("load web tools".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let first_names = tool_names(&requests[0]);
    assert!(first_names.contains(&"load_tool_schema"));
    assert!(!first_names.contains(&"websearch"));
    assert!(!first_names.contains(&"webfetch"));
    assert!(requests[0].instructions.contains("websearch"));
    assert!(requests[0].instructions.contains("webfetch"));

    let second_names = tool_names(&requests[1]);
    assert!(
        second_names.starts_with(&first_names),
        "second request should append schemas without reordering: first={first_names:?} second={second_names:?}"
    );
    assert_eq!(
        &second_names[second_names.len() - 2..],
        &["websearch", "webfetch"]
    );
    let websearch_spec = requests[1]
        .tools
        .iter()
        .find(|tool| tool.name == "websearch")
        .expect("loaded websearch schema");
    let webfetch_spec = requests[1]
        .tools
        .iter()
        .find(|tool| tool.name == "webfetch")
        .expect("loaded webfetch schema");
    assert!(
        websearch_spec.parameters["properties"].is_object(),
        "loaded websearch schema must carry full parameters: {websearch_spec:?}"
    );
    assert!(
        webfetch_spec.parameters["properties"]["url"].is_object(),
        "loaded webfetch schema must carry the `url` property: {webfetch_spec:?}"
    );
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].1["content"]["status"], "attached");
    assert_eq!(outputs[0].1["content"]["position"], 0);
    assert_eq!(outputs[1].1["content"]["status"], "attached");
    assert_eq!(outputs[1].1["content"]["position"], 1);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "Windows default thread stack is 1MB; F10's buffer_unordered subagent dispatch \
              exceeds it on win-x86_64 only. Real fix is to spawn the subagent fan-out on a \
              dedicated tokio task with explicit Builder::new_multi_thread + thread_stack_size, \
              tracked as a follow-up to F10-pi-parallel-and-chain-modes."
)]
async fn explore_subagent_uses_cheap_model_and_hides_intermediate_tool_outputs() {
    let root = temp_workspace("explore_subagent_isolated");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "explore_call".to_string(),
                name: "explore".to_string(),
                arguments: serde_json::json!({
                    "prompt": "Find the needle entrypoint",
                    "scope": "src.rs",
                    "thoroughness": "quick"
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_tools".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(100),
                    output_tokens: Some(10),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "sub_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "src.rs"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("sub_tools".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(11),
                    output_tokens: Some(3),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "Brief: src.rs defines fn needle(). Read src.rs before editing.".to_string(),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("sub_final".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(7),
                    output_tokens: Some(5),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ready".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.model = "expensive-main".to_string();
    config.subagents.explore_model = Some("cheap-explore".to_string());
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("research first".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(&*requests[0].model, "expensive-main");
    assert_eq!(&*requests[1].model, "cheap-explore");
    assert_eq!(&*requests[2].model, "cheap-explore");
    assert_eq!(&*requests[3].model, "expensive-main");
    let subagent_tools = tool_names(&requests[1]);
    assert!(subagent_tools.contains(&"read_file"));
    assert!(subagent_tools.contains(&"repo_map"));
    for forbidden in [
        "delegate",
        "explore",
        "apply_patch",
        "write_file",
        "shell",
        "verify",
    ] {
        assert!(
            !subagent_tools.contains(&forbidden),
            "explore subagent should not advertise {forbidden}: {subagent_tools:?}"
        );
    }

    let parent_outputs = function_outputs(&requests[3]);
    assert_eq!(parent_outputs.len(), 1);
    assert_eq!(parent_outputs[0].0, "explore_call");
    let explore_content = &parent_outputs[0].1["content"];
    assert_eq!(explore_content["ok"], true);
    assert_eq!(explore_content["agent"], "explore");
    assert!(
        explore_content["summary"]
            .as_str()
            .expect("summary")
            .contains("needle")
    );
    assert!(
        !requests[3].input.iter().any(|item| matches!(
            item,
            LlmInputItem::FunctionCall { call_id, .. }
                | LlmInputItem::FunctionCallOutput { call_id, .. } if call_id == "sub_read"
        )),
        "subagent intermediate tool calls must not reach the parent request"
    );

    let snapshot = agent.session_accounting_snapshot().await;
    assert_eq!(snapshot.metrics.subagent_calls, 1);
    assert_eq!(snapshot.metrics.subagent_failures, 0);
    assert_eq!(snapshot.metrics.subagent_tool_calls, 1);
    assert!(snapshot.metrics.subagent_bytes_read > 0);
    assert_eq!(snapshot.metrics.subagent_provider.input_tokens, Some(18));
    assert_eq!(snapshot.metrics.subagent_provider.output_tokens, Some(8));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn disabled_explore_subagent_returns_structured_failure_without_child_requests() {
    let root = temp_workspace("explore_subagent_disabled");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "explore_call".to_string(),
                name: "explore".to_string(),
                arguments: serde_json::json!({"prompt": "scan"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("saw failure".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.subagents.explore_enabled = false;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("scan".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs.len(), 1);
    let content = &outputs[0].1["content"];
    assert_eq!(content["ok"], false);
    assert_eq!(content["status"], "disabled");
    assert!(
        content["error"]
            .as_str()
            .expect("error")
            .contains("disabled")
    );
    let snapshot = agent.session_accounting_snapshot().await;
    assert_eq!(snapshot.metrics.subagent_calls, 1);
    assert_eq!(snapshot.metrics.subagent_failures, 1);
    assert_eq!(snapshot.metrics.subagent_tool_calls, 0);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn delegate_subagent_uses_parent_model_for_natural_research() {
    let root = temp_workspace("delegate_subagent_parent_model");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "delegate_call".to_string(),
                name: "delegate".to_string(),
                arguments: serde_json::json!({
                    "prompt": "Summarize architecture",
                    "scope": "Rust crates"
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("Architecture summary".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("delegate_final".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(4),
                    output_tokens: Some(6),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.model = "main-model".to_string();
    config.subagents.explore_model = Some("cheap-explore".to_string());
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("delegate this".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(&*requests[1].model, "main-model");
    let outputs = function_outputs(&requests[2]);
    assert_eq!(outputs.len(), 1);
    let content = &outputs[0].1["content"];
    assert_eq!(content["ok"], true);
    assert_eq!(content["agent"], "delegate");
    assert_eq!(content["model"], "main-model");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn mixed_subagent_kinds_track_cost_per_kind() {
    let root = temp_workspace("subagent_per_kind_rollup");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Turn 1 parent: dispatch explore subagent.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "explore_call".to_string(),
                name: "explore".to_string(),
                arguments: serde_json::json!({
                    "prompt": "Find the needle entrypoint",
                    "scope": "src.rs"
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("turn1_parent_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Explore subagent final assistant message.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("found".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("explore_final".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(7),
                    output_tokens: Some(5),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Turn 1 parent final.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ready".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("turn1_parent_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Turn 2 parent: dispatch delegate subagent.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "delegate_call".to_string(),
                name: "delegate".to_string(),
                arguments: serde_json::json!({
                    "prompt": "Summarize architecture",
                    "scope": "Rust crates"
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("turn2_parent_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Delegate subagent final assistant message.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("Architecture summary".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("delegate_final".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(40),
                    output_tokens: Some(60),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Turn 2 parent final.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("turn2_parent_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.model = "expensive-main".to_string();
    config.subagents.explore_model = Some("cheap-explore".to_string());
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("first".to_string(), CancellationToken::new())).await;
    drain_turn(agent.start_turn("second".to_string(), CancellationToken::new())).await;

    let snapshot = agent.session_accounting_snapshot().await;
    // Aggregate counters still account for both kinds.
    assert_eq!(snapshot.metrics.subagent_calls, 2);
    assert_eq!(snapshot.metrics.subagent_failures, 0);

    // Per-kind buckets must split the cost: explore got 7/5 input/output,
    // delegate got 40/60. A regression that aggregates them would fail
    // either equality.
    let by_kind = &snapshot.metrics.subagent_by_kind;
    assert_eq!(by_kind.explore.calls, 1);
    assert_eq!(by_kind.explore.failures, 0);
    assert_eq!(by_kind.explore.provider.input_tokens, Some(7));
    assert_eq!(by_kind.explore.provider.output_tokens, Some(5));

    assert_eq!(by_kind.delegate.calls, 1);
    assert_eq!(by_kind.delegate.failures, 0);
    assert_eq!(by_kind.delegate.provider.input_tokens, Some(40));
    assert_eq!(by_kind.delegate.provider.output_tokens, Some(60));

    // Untouched buckets stay zeroed.
    assert_eq!(by_kind.plan.calls, 0);
    assert_eq!(by_kind.review.calls, 0);
    assert_eq!(by_kind.plan.provider.input_tokens, None);
    assert_eq!(by_kind.review.provider.input_tokens, None);

    let _ = fs::remove_dir_all(root);
}

// Regression test: previously the subagent's internal event channel had a
// buffer of 8 with no receiver, and every parallel tool call pushed two
// `AgentEvent` messages (`ToolCallStarted` and `ToolCallCompleted`). A
// subagent that emitted 5+ parallel tool calls in a single round would fill
// the buffer and block forever on `send().await`. The drained-channel fix
// must keep this case progressing.
#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "Windows default thread stack is 1MB; F10's buffer_unordered subagent dispatch \
              exceeds it on win-x86_64 only. Real fix is to spawn the subagent fan-out on a \
              dedicated tokio task with explicit Builder::new_multi_thread + thread_stack_size, \
              tracked as a follow-up to F10-pi-parallel-and-chain-modes."
)]
async fn explore_subagent_with_many_parallel_tool_calls_does_not_deadlock() {
    let root = temp_workspace("explore_subagent_high_fanout");
    for index in 0..12 {
        fs::write(root.join(format!("file{index}.rs")), b"// content\n").expect("write source");
    }
    let mut sub_round = vec![Ok(LlmEvent::Started)];
    for index in 0..12 {
        sub_round.push(Ok(LlmEvent::ToolCall(LlmToolCall {
            call_id: format!("sub_read_{index}"),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": format!("file{index}.rs")}),
        })));
    }
    sub_round.push(Ok(LlmEvent::Completed {
        response_id: Some("sub_tools".to_string()),
        cost: CostSnapshot::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    }));
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "explore_call".to_string(),
                name: "explore".to_string(),
                arguments: serde_json::json!({"prompt": "Inspect all files"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        sub_round,
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("brief".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("sub_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    let drain = drain_turn(agent.start_turn("research".to_string(), CancellationToken::new()));
    // Give the subagent a generous wall-clock budget; before the fix the
    // send().await inside the tool dispatcher blocked indefinitely.
    tokio::time::timeout(std::time::Duration::from_secs(30), drain)
        .await
        .expect("subagent must not deadlock under high fan-out");

    let snapshot = agent.session_accounting_snapshot().await;
    assert_eq!(snapshot.metrics.subagent_calls, 1);
    assert_eq!(snapshot.metrics.subagent_failures, 0);
    assert_eq!(snapshot.metrics.subagent_tool_calls, 12);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn loaded_tool_schemas_persist_across_turns() {
    let root = temp_workspace("lazy_schema_across_turns");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "load_webfetch".to_string(),
                name: "load_tool_schema".to_string(),
                arguments: serde_json::json!({"name": "webfetch"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_load".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done turn 1".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_done1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done turn 2".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_done2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Allow;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("turn one".to_string(), CancellationToken::new())).await;
    drain_turn(agent.start_turn("turn two".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let first_turn_round_zero = tool_names(&requests[0]);
    assert!(!first_turn_round_zero.contains(&"webfetch"));
    let second_turn_round_zero = tool_names(&requests[2]);
    assert!(
        second_turn_round_zero.contains(&"webfetch"),
        "tools loaded in turn 1 should already appear in round 0 of turn 2: {second_turn_round_zero:?}"
    );
    let webfetch_spec = requests[2]
        .tools
        .iter()
        .find(|tool| tool.name == "webfetch")
        .expect("loaded webfetch schema in turn 2");
    assert!(
        webfetch_spec.parameters["properties"]["url"].is_object(),
        "turn-2 webfetch schema must still carry full parameters: {webfetch_spec:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn lazy_schema_loading_disabled_sends_full_schema_set_without_tools_index() {
    let root = temp_workspace("lazy_schema_disabled");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("eager".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_final".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let mut config = config_for(root.clone());
    config.tools.lazy_schema_loading = false;
    config.permissions.web = PermissionMode::Allow;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("eager run".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let names = tool_names(&requests[0]);
    for expected in [
        "delegate",
        "explore",
        "grep",
        "webfetch",
        "websearch",
        "verify",
    ] {
        assert!(
            names.contains(&expected),
            "lazy=off should advertise the full set including {expected}: {names:?}"
        );
    }
    // `load_tool_schema` is a control tool that only exists in the lazy
    // path; with lazy=off it should not be advertised at all.
    assert!(
        !names.contains(&"load_tool_schema"),
        "lazy=off should not advertise the synthetic load_tool_schema control tool: {names:?}"
    );
    assert!(
        !requests[0].instructions.contains("<tools_index>"),
        "lazy=off should not emit a tools_index section: {:?}",
        requests[0].instructions
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_mode_refuses_disallowed_discoverable_schema_loads() {
    let root = temp_workspace("lazy_schema_plan_refusal");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "load_webfetch".to_string(),
                name: "load_tool_schema".to_string(),
                arguments: serde_json::json!({"name": "webfetch"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_load".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("refused".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.session_mode = SessionMode::Plan;
    config.permissions.web = PermissionMode::Allow;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("load webfetch".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(!tool_names(&requests[1]).contains(&"webfetch"));
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].1["status"], "Denied");
    assert_eq!(outputs[0].1["content"]["status"], "refused");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn parallel_read_batch_denies_remaining_calls_after_byte_budget() {
    let root = temp_workspace("parallel_byte_budget");
    fs::write(root.join("first.txt"), "first content").expect("write first");
    fs::write(root.join("second.txt"), "second content").expect("write second");
    fs::write(root.join("third.txt"), "third content").expect("write third");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "first_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "first.txt"}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "second.txt"}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "third_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "third.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("budgeted".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.max_parallel_tools = 8;
    config.max_tool_bytes_read_per_turn = 1;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("read all files".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs.len(), 3);
    assert_eq!(outputs[0].0, "first_read");
    assert_eq!(outputs[0].1["status"], "Success");
    assert_eq!(outputs[1].0, "second_read");
    assert_eq!(outputs[1].1["status"], "Denied");
    assert!(
        outputs[1].1["content"]["error"]
            .as_str()
            .expect("budget error")
            .contains("byte-read budget")
    );
    assert_eq!(outputs[2].0, "third_read");
    assert_eq!(outputs[2].1["status"], "Denied");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn glob_and_count_search_outputs_return_to_model() {
    let root = temp_workspace("glob_count");
    fs::write(root.join("one.rs"), "fn needle() {}\n").expect("write one");
    fs::write(root.join("two.rs"), "fn needle() {}\n").expect("write two");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "glob_call".to_string(),
                name: "glob".to_string(),
                arguments: serde_json::json!({"pattern": "*.rs"}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "count_call".to_string(),
                name: "grep".to_string(),
                arguments: serde_json::json!({
                    "pattern": "needle",
                    "include": ["*.rs"],
                    "output_mode": "count",
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("summarized".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn("summarize files".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "glob_call");
    assert_eq!(
        outputs[0].1["content"]["paths"],
        serde_json::json!(["one.rs", "two.rs"])
    );
    assert_eq!(outputs[1].0, "count_call");
    assert_eq!(outputs[1].1["content"]["count"], 2);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn denied_write_is_reported_to_model_and_does_not_touch_disk() {
    let root = temp_workspace("denied_write");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "write_call".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({"path": "created.txt", "content": "blocked"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("not written".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.edit = PermissionMode::Ask;
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("write blocked file".to_string(), CancellationToken::new());
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested { decision_tx, .. } = event {
            decision_tx
                .send(ToolApprovalDecision::Denied)
                .expect("send denial");
        }
    }

    assert!(!root.join("created.txt").exists());
    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "write_call");
    assert_eq!(outputs[0].1["status"], "Denied");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_mode_write_is_denied_without_approval_prompt() {
    let root = temp_workspace("plan_write_denied");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "write_call".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({"path": "created.txt", "content": "blocked"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("not written".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.session_mode = SessionMode::Plan;
    config.permissions.edit = PermissionMode::Allow;
    let agent = Agent::new(config, provider.clone());

    let mut approvals_seen = 0usize;
    let mut rx = agent.start_turn("write in plan mode".to_string(), CancellationToken::new());
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested { decision_tx, .. } = event {
            approvals_seen += 1;
            decision_tx
                .send(ToolApprovalDecision::Approved)
                .expect("send approval");
        }
    }

    assert_eq!(approvals_seen, 0);
    assert!(!root.join("created.txt").exists());
    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "write_call");
    assert_eq!(outputs[0].1["status"], "Denied");
    assert!(
        outputs[0].1["content"]["error"]
            .as_str()
            .expect("denial reason")
            .contains("plan mode refuses edit")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_mode_denies_hidden_shell_verify_and_webfetch_without_approval() {
    let cases = [
        (
            "shell_call",
            "shell",
            serde_json::json!({"command": "echo should-not-run", "description": "test shell"}),
            "plan mode refuses shell",
        ),
        (
            "git_call",
            "shell",
            serde_json::json!({"command": "git status", "description": "git status probe"}),
            "plan mode refuses git",
        ),
        (
            "verify_call",
            "verify",
            serde_json::json!({}),
            "plan mode refuses compiler",
        ),
        (
            "web_call",
            "webfetch",
            serde_json::json!({"url": "https://example.com"}),
            "plan mode refuses network",
        ),
    ];

    for (call_id, tool_name, arguments, expected_reason) in cases {
        let root = temp_workspace(&format!("plan_denies_{tool_name}"));
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: call_id.to_string(),
                    name: tool_name.to_string(),
                    arguments,
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_tools".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ],
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("denied".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_final".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ],
        ]));
        let mut config = config_for(root.clone());
        config.session_mode = SessionMode::Plan;
        config.permissions.shell = PermissionMode::Allow;
        config.permissions.web = PermissionMode::Allow;
        let agent = Agent::new(config, provider.clone());

        let mut approvals_seen = 0usize;
        let mut rx = agent.start_turn(
            format!("attempt hidden {tool_name}"),
            CancellationToken::new(),
        );
        while let Some(event) = rx.recv().await {
            if let AgentEvent::ApprovalRequested { decision_tx, .. } = event {
                approvals_seen += 1;
                decision_tx
                    .send(ToolApprovalDecision::Approved)
                    .expect("send approval");
            }
        }

        assert_eq!(approvals_seen, 0, "{tool_name} should not prompt");
        let requests = provider.requests();
        assert!(
            !tool_names(&requests[0]).contains(&tool_name),
            "{tool_name} should not be advertised in plan mode",
        );
        let outputs = function_outputs(&requests[1]);
        assert_eq!(outputs[0].0, call_id);
        assert_eq!(outputs[0].1["status"], "Denied");
        assert!(
            outputs[0].1["content"]["error"]
                .as_str()
                .expect("denial reason")
                .contains(expected_reason),
            "{tool_name} denial should contain {expected_reason}: {:?}",
            outputs[0].1
        );

        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn approved_write_edits_real_workspace_file() {
    let root = temp_workspace("approved_write");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let expected_sha256 = sha256_hex("before".as_bytes());
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "write_call".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "sample.txt",
                    "content": "after",
                    "expected_sha256": expected_sha256,
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("edited".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.edit = PermissionMode::Allow;
    let agent = Agent::new(config, provider);

    drain_turn(agent.start_turn("edit file".to_string(), CancellationToken::new())).await;

    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "after"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn large_read_result_returns_spill_handle_to_model() {
    let root = temp_workspace("agent_spill");
    fs::write(root.join("large.txt"), "a".repeat(30_000)).expect("write large");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "large.txt", "limit": 40_000}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("spilled".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn("read large".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "read_call");
    assert_eq!(outputs[0].1["content"]["spilled"], true);
    assert!(
        outputs[0].1["content"]["handle"]
            .as_str()
            .is_some_and(|handle| handle.len() == 64)
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn repeated_read_result_returns_receipt_stub_to_model() {
    let root = temp_workspace("receipt_stub");
    fs::write(root.join("sample.txt"), "same content\n").expect("write sample");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "first_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("deduped".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn("read twice".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[2]);
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].0, "first_read");
    assert_eq!(outputs[0].1["content"]["content"], "same content\n");
    assert_eq!(outputs[1].0, "second_read");
    assert_eq!(outputs[1].1["content"]["receipt_stub"], true);
    assert_eq!(outputs[1].1["content"]["same_as_call_id"], "first_read");
    assert_eq!(
        outputs[1].1["content"]["original_output_sha256"],
        outputs[0].1["receipt"]["output_sha256"]
    );
    assert!(outputs[1].1["content"]["content"].is_null());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn successful_read_result_persists_model_visible_snapshot() {
    let root = temp_workspace("read_snapshot");
    fs::write(root.join("sample.txt"), "visible content\n").expect("write sample");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_once".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider);

    drain_turn(agent.start_turn("read once".to_string(), CancellationToken::new())).await;
    drop(agent);

    let store = SqueezyStore::open(&root, None).expect("open store");
    let snapshot = store
        .read_snapshot("sample.txt")
        .expect("read snapshot")
        .expect("snapshot exists");
    assert_eq!(snapshot.call_id, "read_once");
    assert_eq!(snapshot.content, "visible content\n");
    let expected_hash = sha256_hex("visible content\n".as_bytes());
    assert_eq!(
        snapshot.content_sha256.as_deref(),
        Some(expected_hash.as_str())
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn repeated_read_result_returns_receipt_stub_across_sessions() {
    let root = temp_workspace("receipt_stub_cross_session");
    fs::write(root.join("sample.txt"), "durable content\n").expect("write sample");
    let first_provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "first_session_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let first_agent = Agent::new(config_for(root.clone()), first_provider);
    drain_turn(first_agent.start_turn("read once".to_string(), CancellationToken::new())).await;
    drop(first_agent);

    let second_provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "second_session_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("deduped".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let second_agent = Agent::new(config_for(root.clone()), second_provider.clone());
    drain_turn(second_agent.start_turn("read again".to_string(), CancellationToken::new())).await;

    let requests = second_provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].0, "second_session_read");
    assert_eq!(outputs[0].1["content"]["receipt_stub"], true);
    assert_eq!(
        outputs[0].1["content"]["same_as_call_id"],
        "first_session_read"
    );
    assert!(outputs[0].1["content"]["content"].is_null());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn agent_shares_state_store_with_tool_registry_for_graph_persistence() {
    // Regression for an earlier draft of the persistence change in which the
    // agent and the tool registry each opened their own `SqueezyStore` on
    // the workspace. redb forbids a second `Database` handle on the same
    // file, so the registry's open quietly failed and graph persistence
    // never ran. Asserting that the shared state store contains a graph
    // partition entry after one agent turn pins the contract that both
    // layers reuse the agent's `Arc<SqueezyStore>`.
    let root = temp_workspace("agent_shared_state_store");
    fs::create_dir_all(root.join("src")).expect("create src");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = 'shared-state-demo'\nversion = '0.1.0'\nedition = '2024'\n",
    )
    .expect("write cargo");
    fs::write(root.join("src/lib.rs"), "pub fn shared_state_demo() {}\n").expect("write lib");

    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("ok".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_only".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(config_for(root.clone()), provider);
    drain_turn(agent.start_turn("warm graph".to_string(), CancellationToken::new())).await;
    // Join the agent's background tasks (notably the MCP refresh from
    // `start_turn`) before dropping it so their `Arc<SqueezyStore>`
    // clones are released. Without this the redb lock can outlive
    // `drop(agent)` and the re-open below races a fire-and-forget spawn.
    agent.shutdown().await;
    drop(agent);

    let store = SqueezyStore::open(&root, None).expect("reopen state store");
    let partition: Option<serde_json::Value> = store
        .graph_partition(&squeezy_core::FileId::new("src/lib.rs"))
        .expect("graph_partition");
    assert!(
        partition.is_some(),
        "agent must persist graph partitions through the shared state store",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn repeated_read_result_in_same_round_returns_receipt_stub_to_model() {
    let root = temp_workspace("receipt_stub_same_round");
    fs::write(root.join("sample.txt"), "same round\n").expect("write sample");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "first_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("deduped".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn(
        "read same file twice in one round".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].0, "first_read");
    assert_eq!(outputs[0].1["content"]["content"], "same round\n");
    assert_eq!(outputs[1].0, "second_read");
    assert_eq!(outputs[1].1["content"]["receipt_stub"], true);
    assert_eq!(outputs[1].1["content"]["same_as_call_id"], "first_read");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn repeated_spilled_read_result_returns_receipt_stub_to_model() {
    let root = temp_workspace("receipt_stub_spill");
    fs::write(root.join("large.txt"), "x".repeat(30_000)).expect("write large");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "first_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "large.txt", "limit": 40_000}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "large.txt", "limit": 40_000}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("deduped spill".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn(
        "read large file twice in one round".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].0, "first_read");
    assert_eq!(outputs[0].1["content"]["spilled"], true);
    assert!(
        outputs[0].1["content"]["original_output_sha256"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
    );
    assert_eq!(outputs[1].0, "second_read");
    assert_eq!(outputs[1].1["content"]["receipt_stub"], true);
    assert_eq!(outputs[1].1["content"]["same_as_call_id"], "first_read");
    assert_eq!(
        outputs[1].1["content"]["original_output_sha256"],
        outputs[0].1["content"]["original_output_sha256"]
    );
    assert!(outputs[1].1["content"]["handle"].is_null());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn changed_read_result_is_not_receipt_stubbed() {
    let root = temp_workspace("receipt_stub_changed");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let before_hash = sha256_hex("before".as_bytes());
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "first_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "write_call".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "sample.txt",
                    "content": "after",
                    "expected_sha256": before_hash,
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_write".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "sample.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("changed".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn(
        "read, edit, read again".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[3]);
    assert_eq!(outputs.len(), 3);
    assert_eq!(outputs[2].0, "second_read");
    assert_eq!(outputs[2].1["content"]["content"], "after");
    assert_ne!(outputs[2].1["content"]["receipt_stub"], true);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn aggregate_tool_result_budget_compacts_later_outputs() {
    let root = temp_workspace("aggregate_budget");
    fs::write(root.join("small.txt"), "small").expect("write small");
    fs::write(root.join("large.txt"), "b".repeat(2_000)).expect("write large");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "small_call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "small.txt"}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "large_call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "large.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("budgeted".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.max_tool_result_bytes_per_round = 1_000;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("read both".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "small_call");
    assert_eq!(outputs[0].1["status"], "Success");
    assert_eq!(outputs[1].0, "large_call");
    assert_eq!(outputs[1].1["status"], "Error");
    assert!(
        outputs[1].1["content"]["error"]
            .as_str()
            .expect("budget error")
            .contains("aggregate tool-result budget")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn aggregate_budget_omission_is_not_remembered_as_seen_output() {
    let root = temp_workspace("aggregate_budget_receipts");
    fs::write(root.join("first.txt"), "a".repeat(900)).expect("write first");
    fs::write(root.join("second.txt"), "b".repeat(900)).expect("write second");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "first_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "first.txt"}),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "second.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "second_retry".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "second.txt"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_retry".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("not remembered".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.max_tool_result_bytes_per_round = 1_500;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn(
        "read both then retry omitted output".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    let first_round_outputs = function_outputs(&requests[1]);
    assert_eq!(first_round_outputs[0].1["status"], "Success");
    assert_eq!(first_round_outputs[1].1["status"], "Error");
    assert!(
        first_round_outputs[1].1["content"]["error"]
            .as_str()
            .expect("budget error")
            .contains("aggregate tool-result budget")
    );

    let retry_outputs = function_outputs(&requests[2]);
    assert_eq!(retry_outputs[2].0, "second_retry");
    assert_eq!(retry_outputs[2].1["status"], "Success");
    assert_eq!(retry_outputs[2].1["content"]["content"], "b".repeat(900));
    assert_ne!(retry_outputs[2].1["content"]["receipt_stub"], true);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn denied_webfetch_is_reported_and_does_not_open_network_connection() {
    let root = temp_workspace("denied_webfetch");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "web_call".to_string(),
                name: "webfetch".to_string(),
                arguments: serde_json::json!({"url": "https://example.com/blocked", "timeout_ms": 100}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("blocked".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Ask;
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("fetch denied url".to_string(), CancellationToken::new());
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested {
            request,
            decision_tx,
            ..
        } = event
        {
            assert_eq!(request.tool_name, "webfetch");
            assert_eq!(request.scope, PermissionScope::Web);
            assert!(request.summary().contains("example.com"));
            decision_tx
                .send(ToolApprovalDecision::Denied)
                .expect("send denial");
        }
    }

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "web_call");
    assert_eq!(outputs[0].1["status"], "Denied");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn disabled_web_permission_returns_denied_tool_result() {
    let root = temp_workspace("disabled_web_permission");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "web_call".to_string(),
                name: "webfetch".to_string(),
                arguments: serde_json::json!({"url": "https://example.com/docs"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("blocked".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Deny;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("fetch denied url".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "web_call");
    assert_eq!(outputs[0].1["status"], "Denied");
    assert_eq!(outputs[0].1["content"]["permission_denied"], true);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn blocked_web_domain_rule_returns_denied_tool_result() {
    let root = temp_workspace("blocked_web_domain_rule");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "web_call".to_string(),
                name: "webfetch".to_string(),
                arguments: serde_json::json!({"url": "https://example.com/docs"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("blocked".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Allow;
    config.permissions.rules.push(PermissionRule::new(
        "network",
        "domain:example.com",
        PermissionAction::Deny,
        PermissionRuleSource::Project,
        Some("blocked test domain".to_string()),
    ));
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("fetch blocked domain".to_string(), CancellationToken::new()))
        .await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "web_call");
    assert_eq!(outputs[0].1["status"], "Denied");
    let reason = outputs[0].1["content"]["reason"].as_str().expect("reason");
    assert_eq!(reason, "blocked test domain");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn silent_deny_omits_reason_from_tool_result() {
    // F04-cc-permission-decision-silent-vs-explained:
    // a deny rule with `silent = true` must replace the structured
    // `capability=...; target=...; risk=...` line in the tool-result with the
    // static `action denied by policy` placeholder. The model's tool-result
    // payload is the only place we assert here; the audit log is covered by
    // the unit-level test below in lib_tests.rs.
    let root = temp_workspace("silent_deny_omits_reason");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "web_call".to_string(),
                name: "webfetch".to_string(),
                arguments: serde_json::json!({"url": "https://example.com/docs"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ok".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Allow;
    config.permissions.rules.push(
        PermissionRule::new(
            "network",
            "domain:example.com",
            PermissionAction::Deny,
            PermissionRuleSource::Project,
            Some("absolute deny: example.com is forbidden by policy".to_string()),
        )
        .with_silent(true),
    );
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("fetch blocked domain".to_string(), CancellationToken::new()))
        .await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "web_call");
    assert_eq!(outputs[0].1["status"], "Denied");
    let reason = outputs[0].1["content"]["reason"]
        .as_str()
        .expect("reason on tool result");
    assert_eq!(
        reason, "action denied by policy",
        "silent rule must replace the structured reason on the tool-result",
    );
    assert!(
        !reason.contains("capability="),
        "model-facing reason must not include the structured per-call narrative",
    );
    assert!(
        !reason.contains("absolute deny"),
        "model-facing reason must not include the rule's own reason text",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn non_silent_deny_rule_still_carries_explanation_to_the_model() {
    // Counterpoint to silent_deny_omits_reason_from_tool_result: when the
    // rule does NOT set `silent = true`, the model still receives the rule's
    // reason text. This is the existing behavior; the test pins it so a
    // future change cannot silently flip the default.
    let root = temp_workspace("explained_deny_keeps_reason");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "web_call".to_string(),
                name: "webfetch".to_string(),
                arguments: serde_json::json!({"url": "https://example.com/docs"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ok".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Allow;
    config.permissions.rules.push(PermissionRule::new(
        "network",
        "domain:example.com",
        PermissionAction::Deny,
        PermissionRuleSource::Project,
        Some("explained: example.com is for staging only".to_string()),
    ));
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("fetch explained".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let outputs = function_outputs(&requests[1]);
    let reason = outputs[0].1["content"]["reason"]
        .as_str()
        .expect("reason on tool result");
    assert!(
        reason.contains("explained"),
        "non-silent rule must keep the rule's reason on the tool-result; got: {reason}",
    );
    assert_ne!(
        reason, "action denied by policy",
        "non-silent rule must NOT use the silent placeholder",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn silent_deny_does_not_emit_approval_requested() {
    // A silent deny is still a Deny verdict, so the user must not be
    // prompted via AgentEvent::ApprovalRequested. The test asserts the
    // approval channel is never woken for the silent-deny call.
    let root = temp_workspace("silent_deny_no_approval");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "web_call".to_string(),
                name: "webfetch".to_string(),
                arguments: serde_json::json!({"url": "https://example.com/x"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ok".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Allow;
    config.permissions.rules.push(
        PermissionRule::new(
            "network",
            "domain:example.com",
            PermissionAction::Deny,
            PermissionRuleSource::Project,
            Some("boilerplate".to_string()),
        )
        .with_silent(true),
    );
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("trigger silent deny".to_string(), CancellationToken::new());
    let mut saw_approval_request = false;
    while let Some(event) = rx.recv().await {
        if matches!(event, AgentEvent::ApprovalRequested { .. }) {
            saw_approval_request = true;
        }
    }
    assert!(
        !saw_approval_request,
        "silent-deny rules must not surface an approval prompt",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn approved_webfetch_validation_error_returns_to_model_and_web_tools_are_indexed() {
    let root = temp_workspace("approved_webfetch_validation");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "web_call".to_string(),
                name: "webfetch".to_string(),
                arguments: serde_json::json!({"url": "file:///tmp/secret"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("reported".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.permissions.web = PermissionMode::Allow;
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn(
        "fetch rejected non-http url".to_string(),
        CancellationToken::new(),
    ))
    .await;

    let requests = provider.requests();
    let tool_names = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(!tool_names.contains(&"webfetch"));
    assert!(!tool_names.contains(&"websearch"));
    assert!(requests[0].instructions.contains("webfetch"));
    assert!(requests[0].instructions.contains("websearch"));
    let outputs = function_outputs(&requests[1]);
    assert_eq!(outputs[0].0, "web_call");
    assert_eq!(outputs[0].1["status"], "Error");
    assert!(
        outputs[0].1["content"]["error"]
            .as_str()
            .expect("error")
            .contains("http:// or https://")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn resumed_session_restores_prior_conversation() {
    let root = temp_workspace("resume_conversation");
    let first_provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("first answer".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_first".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let first_agent = Agent::new(config_for(root.clone()), first_provider);
    drain_turn(first_agent.start_turn("first prompt".to_string(), CancellationToken::new())).await;
    let session_id = first_agent.session_id().expect("session id");

    let second_provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("second answer".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_second".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let (second_agent, transcript) = Agent::resume(
        config_for(root.clone()),
        second_provider.clone(),
        &session_id,
    )
    .expect("resume session");
    assert_eq!(transcript.len(), 2);

    drain_turn(second_agent.start_turn("second prompt".to_string(), CancellationToken::new()))
        .await;
    let requests = second_provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        matches!(&requests[0].input[0], LlmInputItem::UserText(text) if text == "first prompt")
    );
    assert!(
        matches!(&requests[0].input[1], LlmInputItem::AssistantText(text) if text == "first answer")
    );
    assert!(
        matches!(&requests[0].input[2], LlmInputItem::UserText(text) if text == "second prompt")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn resumed_session_preserves_cumulative_cost_and_metrics() {
    let root = temp_workspace("resume_cost_preserved");
    let first_provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("first answer".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_first".to_string()),
            cost: CostSnapshot {
                input_tokens: Some(100),
                output_tokens: Some(50),
                ..CostSnapshot::default()
            },
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let first_agent = Agent::new(config_for(root.clone()), first_provider);
    drain_turn(first_agent.start_turn("first prompt".to_string(), CancellationToken::new())).await;
    let session_id = first_agent.session_id().expect("session id");

    let mid_metadata = first_agent
        .show_session(&session_id)
        .expect("show first")
        .metadata;
    let first_turns = mid_metadata.metrics.turns;
    assert!(first_turns >= 1, "first turn should be recorded");
    assert_eq!(mid_metadata.cost.input_tokens, Some(100));

    let second_provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("second answer".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_second".to_string()),
            cost: CostSnapshot {
                input_tokens: Some(40),
                output_tokens: Some(20),
                ..CostSnapshot::default()
            },
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let (second_agent, _) = Agent::resume(
        config_for(root.clone()),
        second_provider.clone(),
        &session_id,
    )
    .expect("resume session");
    drain_turn(second_agent.start_turn("second prompt".to_string(), CancellationToken::new()))
        .await;

    let resumed_metadata = second_agent
        .show_session(&session_id)
        .expect("show resumed")
        .metadata;
    assert_eq!(
        resumed_metadata.cost.input_tokens,
        Some(140),
        "resumed cost should accumulate across the original and post-resume turns",
    );
    assert_eq!(resumed_metadata.cost.output_tokens, Some(70));
    assert_eq!(resumed_metadata.metrics.turns, first_turns + 1);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn empty_session_accounting_snapshot_reports_zero_completed_state() {
    let root = temp_workspace("empty_accounting");
    let provider = Arc::new(ScriptedProvider::named("openai", Vec::new()));
    let mut config = config_for(root.clone());
    config.model = squeezy_core::DEFAULT_OPENAI_MODEL.to_string();
    let agent = Agent::new(config, provider);

    let snapshot = agent.session_accounting_snapshot().await;

    assert_eq!(snapshot.provider, "openai");
    assert_eq!(snapshot.metrics.turns, 0);
    assert_eq!(snapshot.cost.input_tokens, None);
    assert_eq!(snapshot.transcript.items, 0);
    assert_eq!(snapshot.conversation.items, 0);
    assert_eq!(
        snapshot.transmitted_request.context_window_tokens,
        Some(400_000)
    );
    assert!(snapshot.transmitted_request.input_tokens > 0);
    assert!(!snapshot.provider_stored_context_active());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn automatic_context_compaction_replaces_old_raw_history() {
    let root = temp_workspace("auto_context_compaction");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(format!(
                "first answer {}",
                "plan ".repeat(500)
            ))),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("second answer".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.context_compaction = ContextCompactionConfig {
        enabled: true,
        estimated_tokens: 10,
        min_items: 3,
        recent_items: 1,
        max_summary_bytes: 1_200,
        ..ContextCompactionConfig::default()
    };
    let old_prompt = format!("first prompt {}", "raw-old-context ".repeat(400));
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn(old_prompt.clone(), CancellationToken::new())).await;
    drain_turn(agent.start_turn("second prompt".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].input.len(), 2);
    let LlmInputItem::UserText(summary) = &requests[1].input[0] else {
        panic!("first compacted item should be user summary");
    };
    assert!(summary.contains("Squeezy compacted conversation context"));
    assert!(summary.contains("Compacted 2 older model-visible item"));
    assert!(!summary.contains(&old_prompt));
    assert!(summary.len() < old_prompt.len());
    assert!(
        matches!(&requests[1].input[1], LlmInputItem::UserText(text) if text == "second prompt")
    );

    let record = agent
        .show_session(&agent.session_id().expect("session id"))
        .expect("show session");
    assert_eq!(
        record
            .resume_state
            .expect("resume")
            .context_compaction
            .generation,
        1
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn store_responses_accounting_marks_provider_stored_context_gap() {
    let root = temp_workspace("store_response_accounting");
    let provider = Arc::new(ScriptedProvider::named(
        "openai",
        vec![vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("stored answer".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_stored".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(100),
                    output_tokens: Some(25),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ]],
    ));
    let mut config = config_for(root.clone());
    config.model = squeezy_core::DEFAULT_OPENAI_MODEL.to_string();
    config.store_responses = true;
    let agent = Agent::new(config, provider);

    drain_turn(agent.start_turn("first prompt".to_string(), CancellationToken::new())).await;
    let snapshot = agent.session_accounting_snapshot().await;

    assert!(snapshot.provider_stored_context_active());
    assert_eq!(
        snapshot.previous_response_id.as_deref(),
        Some("resp_stored")
    );
    assert_eq!(snapshot.metrics.turns, 1);
    assert_eq!(snapshot.cost.input_tokens, Some(100));
    assert!(snapshot.full_history_request.input_tokens > snapshot.transmitted_request.input_tokens);
    assert_eq!(
        snapshot.transmitted_request.context_window_tokens,
        Some(400_000)
    );
    assert!(
        snapshot
            .transmitted_request
            .used_input_percent_x100
            .is_some()
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn manual_context_compaction_preserves_pins_in_resume_state() {
    let root = temp_workspace("manual_context_compaction");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(format!(
            "important decision {}",
            "must ".repeat(200)
        ))),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_first".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let mut config = config_for(root.clone());
    config.context_compaction = ContextCompactionConfig {
        enabled: true,
        estimated_tokens: 10_000,
        min_items: 99,
        recent_items: 1,
        max_summary_bytes: 1_200,
        ..ContextCompactionConfig::default()
    };
    let agent = Agent::new(config, provider);

    drain_turn(agent.start_turn("first prompt".to_string(), CancellationToken::new())).await;
    let pin = agent
        .pin_context_entry(
            "decision".to_string(),
            "Use deterministic compaction".to_string(),
            "test".to_string(),
        )
        .await
        .expect("pin");
    let report = agent.compact_context_manual().await.expect("compact");

    assert_eq!(report.record.trigger.as_str(), "manual");
    assert!(report.summary.contains("Use deterministic compaction"));
    let record = agent
        .show_session(&agent.session_id().expect("session id"))
        .expect("show session");
    let compaction = record.resume_state.expect("resume").context_compaction;
    assert_eq!(compaction.generation, 1);
    assert_eq!(compaction.pinned[0].id, pin.id);
    assert!(
        record
            .events
            .iter()
            .any(|event| event.kind == "context_compacted")
    );

    let _ = fs::remove_dir_all(root);
}

/// Manual `/compact` must broadcast `AgentEvent::ContextCompacted` on
/// the agent-level event channel so TUI overlays, eval capture, MCP
/// listeners, and any other `AgentEvent` subscriber observe a manual
/// compaction the same way they observe the automatic post-turn and
/// mid-turn micro-compaction paths. `turn_id` is `TurnId::INVALID`
/// because manual compaction runs between turns and has no active
/// per-call `mpsc::Sender<AgentEvent>` to attribute against.
#[tokio::test]
async fn manual_context_compaction_broadcasts_context_compacted_event() {
    let root = temp_workspace("manual_context_compaction_broadcast");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(format!(
            "important decision {}",
            "must ".repeat(200)
        ))),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_first".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let mut config = config_for(root.clone());
    // `min_items: 99` keeps the auto-compaction gate closed during
    // `start_turn` so only the explicit `compact_context_manual` call
    // below fires; `recent_items: 1` + `estimated_tokens: 10_000` lets a
    // single seeded turn produce a non-empty older slice once we run a
    // manual compact.
    config.context_compaction = ContextCompactionConfig {
        enabled: true,
        estimated_tokens: 10_000,
        min_items: 99,
        recent_items: 1,
        max_summary_bytes: 1_200,
        ..ContextCompactionConfig::default()
    };
    let agent = Agent::new(config, provider);

    // Subscribe before driving any work so the broadcast's lag-buffer
    // holds whatever fires before we reach `recv` below.
    let mut events = agent.subscribe_events();

    drain_turn(agent.start_turn("first prompt".to_string(), CancellationToken::new())).await;
    let _pin = agent
        .pin_context_entry(
            "decision".to_string(),
            "Use deterministic compaction".to_string(),
            "test".to_string(),
        )
        .await
        .expect("pin");
    let report = agent.compact_context_manual().await.expect("compact");
    assert_eq!(report.record.trigger.as_str(), "manual");

    let broadcast = events.recv().await.expect("broadcast event");
    match broadcast.as_ref() {
        AgentEvent::ContextCompacted { turn_id, report } => {
            assert_eq!(
                *turn_id,
                TurnId::INVALID,
                "manual /compact runs between turns so the broadcast carries TurnId::INVALID",
            );
            assert_eq!(report.record.trigger.as_str(), "manual");
            assert_eq!(report.record.generation, 1);
        }
        _ => panic!("expected ContextCompacted broadcast"),
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn auto_compaction_does_not_orphan_function_call_output() {
    let root = temp_workspace("auto_compaction_pair");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "src.rs" }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tool".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(format!(
                "first answer {}",
                "plan ".repeat(500)
            ))),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("second answer".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.store_responses = false;
    config.context_compaction = ContextCompactionConfig {
        enabled: true,
        estimated_tokens: 10,
        min_items: 3,
        recent_items: 2,
        max_summary_bytes: 1_200,
        ..ContextCompactionConfig::default()
    };
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("first prompt".to_string(), CancellationToken::new())).await;
    drain_turn(agent.start_turn("second prompt".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert!(
        requests.len() >= 3,
        "expected at least three provider requests, got {}",
        requests.len()
    );
    let compacted_input = &requests[2].input;
    let declared_calls: std::collections::HashSet<&str> = compacted_input
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    for item in compacted_input.iter() {
        if let LlmInputItem::FunctionCallOutput { call_id, .. } = item {
            assert!(
                declared_calls.contains(call_id.as_str()),
                "orphan function_call_output for call_id {} in compacted input: {:?}",
                call_id,
                compacted_input
            );
        }
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn pinned_context_is_visible_to_model_before_compaction() {
    let root = temp_workspace("pinned_visible");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ack".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ack2".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.context_compaction = ContextCompactionConfig {
        enabled: true,
        estimated_tokens: 1_000_000,
        min_items: 1_000,
        recent_items: 1,
        max_summary_bytes: 1_200,
        ..ContextCompactionConfig::default()
    };
    let agent = Agent::new(config, provider.clone());

    drain_turn(agent.start_turn("first".to_string(), CancellationToken::new())).await;
    agent
        .pin_context_entry(
            "decision".to_string(),
            "Use deterministic compaction".to_string(),
            "test".to_string(),
        )
        .await
        .expect("pin");
    drain_turn(agent.start_turn("second".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].instructions.contains("Pinned context"),
        "second-turn instructions must surface pinned block, got: {}",
        requests[1].instructions
    );
    assert!(
        requests[1]
            .instructions
            .contains("Use deterministic compaction"),
        "pinned summary text must reach the model: {}",
        requests[1].instructions
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cleanup_sessions_refuses_to_remove_the_active_session() {
    let root = temp_workspace("cleanup_active");
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let agent = Agent::new(config_for(root.clone()), provider);
    let session_id = agent.session_id().expect("session id");

    let result = agent.cleanup_sessions(std::slice::from_ref(&session_id));
    assert!(result.is_err(), "active session must not be cleanable");
    assert!(
        agent
            .show_session(&session_id)
            .is_ok_and(|record| record.metadata.session_id == session_id),
        "active session metadata should still be readable",
    );

    let _ = fs::remove_dir_all(root);
}

/// Records every hook context the agent dispatched. Used to assert
/// that PreToolUse and PostToolUse fire for each executed tool call,
/// in order, with payloads that name the tool and propagate its
/// terminal status.
struct RecordingHookHandler {
    seen: Arc<Mutex<Vec<HookContext>>>,
}

impl HookHandler for RecordingHookHandler {
    fn handle(&self, ctx: &HookContext) -> HookResult {
        self.seen.lock().expect("hook recorder").push(ctx.clone());
        HookResult::allow()
    }
}

#[tokio::test]
async fn pre_and_post_tool_use_hooks_fire_around_each_tool_call() {
    let root = temp_workspace("hook_pre_post_tool_use");
    fs::write(root.join("src.rs"), "fn hooked() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "src.rs"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_tools".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut agent = Agent::new(config_for(root.clone()), provider);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HookRegistry::new();
    registry.register(Box::new(RecordingHookHandler { seen: seen.clone() }));
    agent.set_hooks(Some(Arc::new(registry)));

    drain_turn(agent.start_turn("inspect hooked".to_string(), CancellationToken::new())).await;

    let captured = seen.lock().expect("seen").clone();
    let tool_events: Vec<_> = captured
        .iter()
        .filter(|ctx| matches!(ctx.event, HookEvent::PreToolUse | HookEvent::PostToolUse))
        .collect();
    assert_eq!(
        tool_events.len(),
        2,
        "expected one PreToolUse and one PostToolUse for the single read_file call: {captured:?}"
    );
    assert_eq!(tool_events[0].event, HookEvent::PreToolUse);
    let pre = tool_events[0].payload_json();
    assert_eq!(pre["tool_name"], "read_file");
    assert_eq!(pre["call_id"], "read_call");
    assert_eq!(tool_events[1].event, HookEvent::PostToolUse);
    let post = tool_events[1].payload_json();
    assert_eq!(post["tool_name"], "read_file");
    assert_eq!(post["call_id"], "read_call");
    assert_eq!(post["status"], "success");

    let _ = fs::remove_dir_all(root);
}

async fn drain_turn(mut rx: tokio::sync::mpsc::Receiver<AgentEvent>) {
    while rx.recv().await.is_some() {}
}

#[tokio::test]
async fn session_cost_cap_blocks_further_calls_once_exceeded() {
    // Cap deliberately tiny - the first round's reported cost (60 micros)
    // already lands at the cap, so the *second* round must short-circuit
    // into a Failed event before the provider is hit a second time.
    let root = temp_workspace("session_cost_cap");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("first answer".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(40),
                    output_tokens: Some(20),
                    estimated_usd_micros: Some(60),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("should never stream".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot {
                    estimated_usd_micros: Some(60),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.max_session_cost_usd_micros = Some(50);
    let agent = Agent::new(config, provider.clone());

    // First turn lands at $0.000060 == 120% of cap (already over) but is
    // permitted because the cap is only checked *before* each round.
    drain_turn(agent.start_turn("first prompt".to_string(), CancellationToken::new())).await;

    let mut second_rx = agent.start_turn("second prompt".to_string(), CancellationToken::new());
    let mut failed_with_cap = false;
    let mut saw_provider_call = false;
    while let Some(event) = second_rx.recv().await {
        match event {
            AgentEvent::Failed { error, .. } => {
                let message = error.to_string();
                if message.contains("cost cap") {
                    failed_with_cap = true;
                }
            }
            AgentEvent::AssistantDelta { .. } => {
                saw_provider_call = true;
            }
            _ => {}
        }
    }
    assert!(
        failed_with_cap,
        "the second turn must fail with a cost-cap message"
    );
    assert!(
        !saw_provider_call,
        "the cap check must short-circuit the round before any assistant delta streams"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn session_cost_warning_fires_once_at_threshold() {
    // Cap = 100 micros, warn at 50%; the first round consumes 60 micros (60%)
    // and must emit exactly one CostWarning. The second round consumes
    // another 30 (cumulative 90, 90%) and must *not* re-fire.
    let root = temp_workspace("session_cost_warning");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("first".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first".to_string()),
                cost: CostSnapshot {
                    estimated_usd_micros: Some(60),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("second".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot {
                    estimated_usd_micros: Some(30),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.max_session_cost_usd_micros = Some(100);
    config.cost_warn_percent = 50;
    let agent = Agent::new(config, provider);

    let mut warnings = 0;
    let mut rx = agent.start_turn("first prompt".to_string(), CancellationToken::new());
    while let Some(event) = rx.recv().await {
        if matches!(event, AgentEvent::CostWarning { .. }) {
            warnings += 1;
        }
    }
    assert_eq!(warnings, 1, "warning must fire exactly once on first round");

    let mut rx = agent.start_turn("second prompt".to_string(), CancellationToken::new());
    while let Some(event) = rx.recv().await {
        if matches!(event, AgentEvent::CostWarning { .. }) {
            warnings += 1;
        }
    }
    assert_eq!(
        warnings, 1,
        "warning must not re-fire even when the cumulative cost climbs further"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelled_turn_persists_partial_cost_and_metrics() {
    // The first round produces a real provider cost and a tool call, then
    // the second round is cancelled by the provider before any further
    // assistant text streams. The cancellation path must commit the
    // already-paid cost and the tool counter into the session snapshot so
    // `/cost` reflects what was actually spent.
    let root = temp_workspace("cancel_persists_partial");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "grep_call".to_string(),
                name: "grep".to_string(),
                arguments: serde_json::json!({"pattern": "needle", "include": ["*.rs"]}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(1_200),
                    output_tokens: Some(340),
                    estimated_usd_micros: Some(415_300),
                    ..CostSnapshot::default()
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![Ok(LlmEvent::Started), Ok(LlmEvent::Cancelled)],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    let mut rx = agent.start_turn("locate needle".to_string(), CancellationToken::new());
    let mut saw_cancelled = false;
    while let Some(event) = rx.recv().await {
        if matches!(event, AgentEvent::Cancelled { .. }) {
            saw_cancelled = true;
        }
    }
    assert!(saw_cancelled, "expected an AgentEvent::Cancelled");

    let snapshot = agent.session_accounting_snapshot().await;
    assert_eq!(
        snapshot.cost.estimated_usd_micros,
        Some(415_300),
        "cancelled turn must persist the provider-reported cost from the completed round",
    );
    assert_eq!(snapshot.cost.input_tokens, Some(1_200));
    assert_eq!(snapshot.cost.output_tokens, Some(340));
    assert!(
        snapshot.metrics.tool_calls >= 1,
        "expected at least the grep tool call to be persisted, got {}",
        snapshot.metrics.tool_calls,
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelled_turn_attributes_partial_round_cost_to_metrics() {
    // Reproduces wave2-11 / squeezy-llaj: a single round streams real
    // assistant text + reasoning before the provider stream is cut by
    // a `Cancelled` event. Before the fix the cancelled-turn frame
    // reported `input_tokens=0, output_tokens=0, cost_micro_usd=0`
    // because the broker only recorded usage on the `Completed` arm.
    // The cancel path now estimates the partial spend from the
    // request's input byte size and the streamed output bytes via
    // the same calibration the `Completed` path falls back to.
    let root = temp_workspace("cancel_partial_metrics");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let mut config = config_for(root.clone());
    // Force a model with pricing data so the cancel-path's estimate
    // resolves to a non-zero dollar figure. `gpt-5.4-mini` is in
    // `crates/squeezy-llm/src/models.json` and so resolves to a
    // pricing entry via `model_info_for`.
    config.model = "gpt-5.4-mini".to_string();
    let provider = Arc::new(ScriptedProvider::named(
        "openai",
        vec![vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ReasoningDelta {
                text: "thinking about the answer in some detail".to_string(),
                kind: squeezy_llm::ReasoningKind::Text,
            }),
            Ok(LlmEvent::TextDelta(
                "1. first item\n2. second item\n3. third item\n4. fourth item\n5.".to_string(),
            )),
            Ok(LlmEvent::Cancelled),
        ]],
    ));
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("make a list".to_string(), CancellationToken::new());
    let mut cancel_event_cost: Option<CostSnapshot> = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Cancelled { cost, .. } = event {
            cancel_event_cost = Some(cost);
        }
    }
    let cost = cancel_event_cost.expect("expected an AgentEvent::Cancelled event");
    assert!(
        cost.input_tokens.unwrap_or(0) > 0,
        "cancelled-turn event must carry non-zero input_tokens; got {:?}",
        cost.input_tokens,
    );
    assert!(
        cost.output_tokens.unwrap_or(0) > 0,
        "cancelled-turn event must carry non-zero output_tokens from streamed deltas; got {:?}",
        cost.output_tokens,
    );
    assert!(
        cost.estimated_usd_micros.unwrap_or(0) > 0,
        "cancelled-turn event must carry non-zero estimated_usd_micros; got {:?}",
        cost.estimated_usd_micros,
    );

    let snapshot = agent.session_accounting_snapshot().await;
    assert_eq!(
        snapshot.cost.input_tokens, cost.input_tokens,
        "session accounting must agree with the cancel-event snapshot on input_tokens",
    );
    assert_eq!(
        snapshot.cost.output_tokens, cost.output_tokens,
        "session accounting must agree with the cancel-event snapshot on output_tokens",
    );
    assert_eq!(
        snapshot.cost.estimated_usd_micros, cost.estimated_usd_micros,
        "session accounting must agree with the cancel-event snapshot on estimated_usd_micros",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn stop_no_action_retry_fires_when_model_promised_tool_use() {
    // Reproduces the Qwen3 "I'll do X then stop" pattern. Round 0 the
    // model successfully calls `read_file`. Round 1 it emits a chatty
    // preamble ("Let me scan the codebase") with finish_reason=stop and
    // zero tool calls. The agent should NOT end the turn here — it
    // should push a synthetic user nudge and retry, giving the model
    // one more shot. Round 2 the model finishes properly with a final
    // answer.
    let root = temp_workspace("stop_no_action_retry");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Round 0: legitimate tool call.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "src.rs"}),
            })),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_tools".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::ToolUse),
                false,
            )),
        ],
        // Round 1: chatty preamble + stop with no tool call. Agent
        // should detect this and retry.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "Let me scan the codebase to find more context.".to_string(),
            )),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_stopped".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                false,
            )),
        ],
        // Round 2: after the nudge, model gives the real answer.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "src.rs defines a single `needle` function.".to_string(),
            )),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_final".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                false,
            )),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    let mut rx = agent.start_turn(
        "look at src.rs and tell me what it defines".to_string(),
        CancellationToken::new(),
    );
    let mut final_message = String::new();
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { message, .. } = event {
            final_message = message.content;
        }
    }

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        3,
        "agent should have issued 3 LLM requests (round 0 tool + round 1 stop + retry)"
    );
    // The retry request must include the synthetic user nudge in its
    // input. When store_responses is off (the default for tests), the
    // full conversation is replayed.
    let retry_request = &requests[2];
    let has_nudge = retry_request.input.iter().any(|item| match item {
        LlmInputItem::UserText(text) => text.contains("did not call any tool"),
        _ => false,
    });
    assert!(
        has_nudge,
        "retry request must include the 'you described a follow-up action' nudge in its input",
    );
    assert!(
        final_message.contains("needle"),
        "final assistant message must come from round 2, got {final_message:?}",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn stop_no_action_retry_does_not_fire_when_model_answered_directly() {
    // Inverse of the previous test: round 0 the model calls a tool,
    // round 1 it gives the final answer (no intent text, no
    // unresolved promise). The retry latch must NOT fire — turn ends
    // after 2 LLM requests.
    let root = temp_workspace("stop_no_action_no_retry");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "src.rs"}),
            })),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_tools".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::ToolUse),
                false,
            )),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "src.rs defines a `needle` function and nothing else.".to_string(),
            )),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_final".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                false,
            )),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());
    drain_turn(agent.start_turn("describe src.rs".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "direct answer should NOT trigger the retry latch (expected 2 requests, got {})",
        requests.len()
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn reasoning_only_stop_retries_with_plan_mode_nudge() {
    // Plan-mode round 0: model reasons through the problem but emits
    // no `<proposed_plan>` block and finishes with `reasoning_only_stop`.
    // The retry latch should fire on the first round (no `round > 0`
    // guard for reasoning-only-stop) with a plan-mode nudge. Round 1
    // produces a real plan.
    let root = temp_workspace("reasoning_only_stop_plan");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_reasoning_only".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                true,
            )),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "<proposed_plan>\n## Context\nfoo\n</proposed_plan>".to_string(),
            )),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_plan".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                false,
            )),
        ],
    ]));
    let mut config = config_for(root.clone());
    config.session_mode = SessionMode::Plan;
    let agent = Agent::new(config, provider.clone());
    drain_turn(agent.start_turn("plan a refactor".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "reasoning-only stop should trigger one retry"
    );
    let retry = &requests[1];
    let nudge_present = retry.input.iter().any(|item| match item {
        LlmInputItem::UserText(text) => text.contains("<proposed_plan>") && text.contains("tag"),
        _ => false,
    });
    assert!(
        nudge_present,
        "plan-mode retry nudge must reference the <proposed_plan> tag",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn reasoning_only_stop_retries_with_build_mode_nudge() {
    let root = temp_workspace("reasoning_only_stop_build");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_reasoning_only".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                true,
            )),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "Here is the direct answer.".to_string(),
            )),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_final".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                false,
            )),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());
    drain_turn(agent.start_turn("answer the user".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "reasoning-only stop must trigger a retry"
    );
    // Find the nudge by looking for any UserText that contains the
    // distinctive "finished thinking" marker — the input list also
    // includes the original user prompt, which would match an
    // unfiltered `find_map`.
    let nudge_text = requests[1].input.iter().find_map(|item| match item {
        LlmInputItem::UserText(text) if text.contains("finished thinking") => Some(text.as_str()),
        _ => None,
    });
    assert!(
        nudge_text.is_some(),
        "retry must include a synthetic user nudge"
    );
    assert!(
        !nudge_text.unwrap().contains("<proposed_plan>"),
        "build-mode nudge must NOT mention <proposed_plan>",
    );
    assert!(
        nudge_text.unwrap().contains("visible content"),
        "build-mode nudge should ask for visible content"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn reasoning_only_stop_does_not_retry_twice_in_one_turn() {
    // Both rounds end with reasoning_only_stop. The agent must only
    // retry once — second reasoning-only-stop should NOT trigger
    // another retry. The turn ends with the agent's "completed
    // without content" notice as the assistant message.
    let root = temp_workspace("reasoning_only_no_loop");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_r1".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                true,
            )),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::completed_with_reason(
                Some("resp_r2".to_string()),
                CostSnapshot::default(),
                Some(squeezy_llm::StopReason::EndTurn),
                true,
            )),
        ],
    ]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());
    drain_turn(agent.start_turn("answer".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "second reasoning-only stop must NOT trigger another retry"
    );

    let _ = fs::remove_dir_all(root);
}

fn function_outputs(request: &LlmRequest) -> Vec<(&str, Value)> {
    request
        .input
        .iter()
        .filter_map(|item| {
            let LlmInputItem::FunctionCallOutput { call_id, output } = item else {
                return None;
            };
            Some((
                call_id.as_str(),
                serde_json::from_str(output).expect("tool output JSON"),
            ))
        })
        .collect()
}

fn tool_names(request: &LlmRequest) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

fn config_for(root: PathBuf) -> AppConfig {
    AppConfig {
        workspace_root: root,
        permissions: PermissionPolicy {
            edit: PermissionMode::Allow,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn temp_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy_agent_{name}_{nonce}"));
    fs::create_dir_all(&root).expect("create temp workspace");
    root
}

fn write_rust_crate(root: &std::path::Path, source: &str) {
    fs::create_dir_all(root.join("src")).expect("create src");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"case\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");
    fs::write(root.join("src/lib.rs"), source).expect("write source");
}
