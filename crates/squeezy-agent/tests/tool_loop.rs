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
    AppConfig, ContextCompactionConfig, CostSnapshot, PermissionMode, PermissionPolicy,
    PermissionScope, Result, SessionMode,
};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall};
use squeezy_store::SqueezyStore;
use squeezy_tools::sha256_hex;
use tokio_util::sync::CancellationToken;

struct ScriptedProvider {
    responses: Mutex<VecDeque<Vec<Result<LlmEvent>>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<Vec<Result<LlmEvent>>>) -> Self {
        Self {
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
        "scripted"
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            "update_task_state",
            "checkpoint_list",
            "checkpoint_show",
            "decl_search",
            "definition_search",
            "diff_context",
            "downstream_flow",
            "glob",
            "grep",
            "hierarchy",
            "list_skills",
            "load_skill",
            "plan_patch",
            "read_file",
            "read_slice",
            "read_tool_output",
            "reference_search",
            "repo_map",
            "symbol_context",
            "upstream_flow",
        ]
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn build_mode_advertises_full_tool_set() {
    let root = temp_workspace("build_tools");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("building".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_final".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(config_for(root.clone()), provider.clone());

    drain_turn(agent.start_turn("build".to_string(), CancellationToken::new())).await;

    let requests = provider.requests();
    let tool_names = tool_names(&requests[0]);
    for expected in [
        "apply_patch",
        "write_file",
        "shell",
        "verify",
        "webfetch",
        "websearch",
    ] {
        assert!(
            tool_names.contains(&expected),
            "build mode should advertise {expected}: {tool_names:?}"
        );
    }

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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("budgeted".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("summarized".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("not written".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("not written".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
                }),
            ],
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("denied".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_final".to_string()),
                    cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("edited".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("spilled".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("deduped".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_first_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("deduped".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second_final".to_string()),
                cost: CostSnapshot::default(),
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
        }),
    ]]));
    let agent = Agent::new(config_for(root.clone()), provider);
    drain_turn(agent.start_turn("warm graph".to_string(), CancellationToken::new())).await;
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("deduped".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("deduped spill".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("changed".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("budgeted".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("not remembered".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("blocked".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
async fn approved_webfetch_validation_error_returns_to_model_and_web_tools_are_advertised() {
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("reported".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_final".to_string()),
                cost: CostSnapshot::default(),
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
    assert!(tool_names.contains(&"webfetch"));
    assert!(tool_names.contains(&"websearch"));
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("second answer".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot::default(),
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
        }),
    ]]));
    let mut config = config_for(root.clone());
    config.context_compaction = ContextCompactionConfig {
        enabled: true,
        estimated_tokens: 10_000,
        min_items: 99,
        recent_items: 1,
        max_summary_bytes: 1_200,
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

#[tokio::test]
async fn auto_compaction_does_not_orphan_function_call_output() {
    let root = temp_workspace("auto_compaction_pair");
    fs::write(root.join("src.rs"), "fn needle() {}\n").expect("write source");
    let provider = Arc::new(ScriptedProvider::new(vec![
        // First turn: a single tool round (read_file) + a heavy assistant
        // reply so the next turn crosses the compaction threshold. The
        // resulting persisted conversation pattern is:
        //   [UserText, FunctionCall(read_call), FunctionCallOutput(read_call), AssistantText]
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
            }),
        ],
        // Second turn: a small text completion. The pre-turn compaction
        // must not split between the FunctionCall and its
        // FunctionCallOutput.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("second answer".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot::default(),
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
    for item in compacted_input {
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
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ack2".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_second".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
    ]));
    let mut config = config_for(root.clone());
    // Set thresholds high enough that compaction never auto-fires; the
    // pin must still reach the model.
    config.context_compaction = ContextCompactionConfig {
        enabled: true,
        estimated_tokens: 1_000_000,
        min_items: 1_000,
        recent_items: 1,
        max_summary_bytes: 1_200,
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

async fn drain_turn(mut rx: tokio::sync::mpsc::Receiver<AgentEvent>) {
    while rx.recv().await.is_some() {}
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
