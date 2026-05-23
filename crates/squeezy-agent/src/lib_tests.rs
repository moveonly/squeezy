use std::{
    collections::{BTreeMap, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_core::Stream;
use futures_util::stream;
use serde_json::json;
use squeezy_core::{
    AppConfig, PermissionAction, PermissionCapability, PermissionMode, PermissionPolicy,
    PermissionRequest, PermissionRisk, PermissionRuleSource, Result, SessionMode, SkillsConfig,
};
use squeezy_llm::{
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, LlmToolSpec,
};
use squeezy_tools::{ToolStatus, sha256_hex};
use tracing_subscriber::fmt::MakeWriter;

use super::*;

struct MockProvider {
    responses: Mutex<VecDeque<Vec<Result<LlmEvent>>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl MockProvider {
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

impl LlmProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .unwrap_or_default();
        let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
            Box::pin(stream::iter(events));
        stream
    }
}

#[tokio::test]
async fn turn_stream_accumulates_assistant_text() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("hel".to_string())),
        Ok(LlmEvent::TextDelta("lo".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed {
            message,
            response_id,
            ..
        } = event
        {
            completed = Some((message.content, response_id));
        }
    }

    assert_eq!(
        completed,
        Some(("hello".to_string(), Some("resp_1".to_string())))
    );
}

#[tokio::test]
async fn turn_stream_reports_provider_error() {
    let provider = Arc::new(MockProvider::new(vec![vec![Err(
        SqueezyError::ProviderRequest("boom".to_string()),
    )]]));
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut saw_error = false;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Failed { error, .. } = event {
            saw_error = error.to_string().contains("boom");
        }
    }

    assert!(saw_error);
}

#[tokio::test]
async fn user_input_is_redacted_before_model_request_and_transcript() {
    let provider = Arc::new(MockProvider::new(vec![vec![Ok(LlmEvent::Completed {
        response_id: Some("resp_1".to_string()),
        cost: CostSnapshot::default(),
    })]]));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.start_turn(
        "use sk-abcdefghijklmnopqrstuvwxyz".to_string(),
        CancellationToken::new(),
    );
    let mut user_message = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::UserMessage { message, .. } = event {
            user_message = Some(message.content);
        }
    }

    let user_message = user_message.expect("user message");
    assert!(!user_message.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(user_message.contains("<redacted:openai_key"));
    let requests = provider.requests();
    let LlmInputItem::UserText(request_text) = &requests[0].input[0] else {
        panic!("expected user text");
    };
    assert!(!request_text.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(request_text.contains("<redacted:openai_key"));
}

#[tokio::test]
async fn assistant_text_is_redacted_in_streamed_deltas_and_completed_message() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("token sk-abcdefghijk".to_string())),
        Ok(LlmEvent::TextDelta("lmnopqrstuvwxyz".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut completed = None;
    let mut deltas: Vec<String> = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::AssistantDelta { delta, .. } => deltas.push(delta),
            AgentEvent::Completed {
                message, metrics, ..
            } => {
                completed = Some((message.content, metrics.redactions));
            }
            _ => {}
        }
    }

    let (message, redactions) = completed.expect("completed");
    let combined_delta = deltas.join("");
    // The secret is split across two TextDelta events; safe streaming
    // must redact at the seam, not after the fact.
    assert!(!combined_delta.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(!message.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(message.contains("<redacted:openai_key"));
    // The streamed deltas concatenate into the same message we publish
    // at completion; no drift between the live view and the transcript.
    assert_eq!(combined_delta, message);
    assert!(redactions > 0);
}

#[tokio::test]
async fn approval_summary_is_redacted_for_secret_bearing_shell_command() {
    let root = temp_workspace("agent_approval_redaction");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::ToolCall(LlmToolCall {
            call_id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: json!({
                "command": "curl -H 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz' https://example.com",
                "description": "fetch with token"
            }),
        })),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            shell: PermissionMode::Ask,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("run".to_string(), CancellationToken::new());
    let mut summary = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested {
            request,
            decision_tx,
            ..
        } = event
        {
            summary = Some(request.summary().to_string());
            let _ = decision_tx.send(ToolApprovalDecision::Denied);
        }
    }

    let summary = summary.expect("approval summary");
    // Avoid interpolating the redacted-summary value into the panic
    // message; CodeQL flags that as cleartext logging on assertions
    // whose fixture inputs look secret-shaped.
    assert!(
        !summary.contains("abcdefghijklmnopqrstuvwxyz"),
        "approval summary leaked bearer token",
    );
    assert!(summary.contains("<redacted:bearer_token"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn provider_errors_are_redacted_before_failure_event() {
    let provider = Arc::new(MockProvider::new(vec![vec![Err(
        SqueezyError::ProviderRequest(
            "failed with token sk-abcdefghijklmnopqrstuvwxyz".to_string(),
        ),
    )]]));
    let agent = Agent::new(AppConfig::default(), provider);
    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut failed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Failed { error, .. } = event {
            failed = Some(error.to_string());
        }
    }
    let failed = failed.expect("failed");
    assert!(!failed.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(failed.contains("<redacted:openai_key"));
}

#[test]
fn agent_new_falls_back_to_current_dir_for_invalid_workspace_root() {
    let root = temp_workspace("agent_invalid_root");
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.join("missing"),
            ..Default::default()
        },
        provider,
    );

    assert_eq!(agent.provider_name(), "mock");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn tool_loop_executes_fallback_tool_and_returns_observation() {
    let root = temp_workspace("agent_tool_loop");
    fs::write(root.join("sample.rs"), "fn needle() {}\n").expect("write sample");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle", "include": ["*.rs"]}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(10),
                    output_tokens: Some(1),
                    cached_input_tokens: None,
                    cache_write_input_tokens: None,
                    estimated_usd_micros: None,
                },
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("found it".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(4),
                    output_tokens: Some(2),
                    cached_input_tokens: None,
                    cache_write_input_tokens: None,
                    estimated_usd_micros: None,
                },
            }),
        ],
    ]));
    let config = AppConfig {
        workspace_root: root.clone(),
        ..Default::default()
    };
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("find needle".to_string(), CancellationToken::new());
    let mut tool_result = None;
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ToolCallCompleted { result, .. } => tool_result = Some(result),
            AgentEvent::Completed { message, cost, .. } => {
                completed = Some((message.content, cost));
            }
            _ => {}
        }
    }

    let tool_result = tool_result.expect("tool result");
    assert_eq!(tool_result.status, ToolStatus::Success);
    assert_eq!(tool_result.content["matches"][0]["path"], "sample.rs");
    let (message, cost) = completed.expect("completed");
    assert_eq!(message, "found it");
    assert_eq!(cost.input_tokens, Some(14));
    assert_eq!(provider.requests().len(), 2);
    assert!(!provider.requests()[0].tools.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn asks_for_edit_permission_before_write_tool() {
    let root = temp_workspace("agent_approval");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::ToolCall(LlmToolCall {
            call_id: "call_1".to_string(),
            name: "write_file".to_string(),
            arguments: json!({"path": "sample.txt", "content": "hello"}),
        })),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            edit: PermissionMode::Ask,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("write file".to_string(), CancellationToken::new());
    let mut saw_approval = false;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested {
            request,
            decision_tx,
            ..
        } = event
        {
            saw_approval = true;
            assert_eq!(request.tool_name, "write_file");
            decision_tx
                .send(ToolApprovalDecision::Denied)
                .expect("send decision");
        }
    }

    assert!(saw_approval);
    assert!(!root.join("sample.txt").exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelling_turn_unblocks_pending_approval() {
    let root = temp_workspace("agent_cancel_approval");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::ToolCall(LlmToolCall {
            call_id: "call_1".to_string(),
            name: "write_file".to_string(),
            arguments: json!({"path": "sample.txt", "content": "hello"}),
        })),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            edit: PermissionMode::Ask,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider);
    let cancel = CancellationToken::new();
    let mut rx = agent.start_turn("write file".to_string(), cancel.clone());
    let mut pending_decision = None;
    let mut saw_cancelled_tool = false;

    tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::ApprovalRequested { decision_tx, .. } => {
                    pending_decision = Some(decision_tx);
                    cancel.cancel();
                }
                AgentEvent::ToolCallCompleted { result, .. } => {
                    saw_cancelled_tool = result.status == ToolStatus::Cancelled;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("turn should not block on unanswered approval after cancellation");

    assert!(pending_decision.is_some());
    assert!(saw_cancelled_tool);
    assert!(!root.join("sample.txt").exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn tool_loop_can_edit_file_with_write_tool() {
    let root = temp_workspace("agent_write_tool");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let before_hash = sha256_hex("before".as_bytes());
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_1".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "after",
                    "expected_sha256": before_hash,
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("edited".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
    ]));
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            edit: PermissionMode::Allow,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("edit sample".to_string(), CancellationToken::new());
    let mut completed = false;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { .. } = event {
            completed = true;
        }
    }

    assert!(completed);
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "after"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn inactive_skills_are_not_eagerly_added_to_instructions() {
    let root = temp_workspace("agent_skill_inactive");
    write_skill(
        &root.join(".agents/skills/rust-nav"),
        "rust-nav",
        &["rust symbol"],
    );
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("ok".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(config_with_skill_dirs(&root), provider.clone());

    let mut rx = agent.start_turn("hello".to_string(), CancellationToken::new());
    while rx.recv().await.is_some() {}

    let request = provider.requests().pop().expect("request");
    assert!(!request.instructions.contains("<active_skills>"));
    assert!(!request.instructions.contains("Rust Nav"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn explicit_skill_activation_injects_body_and_rewrites_task() {
    let root = temp_workspace("agent_skill_explicit");
    write_skill(
        &root.join(".agents/skills/rust-nav"),
        "rust-nav",
        &["rust symbol"],
    );
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("ok".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(config_with_skill_dirs(&root), provider.clone());

    let mut rx = agent.start_turn(
        "/skill rust-nav inspect main".to_string(),
        CancellationToken::new(),
    );
    while rx.recv().await.is_some() {}

    let request = provider.requests().pop().expect("request");
    assert!(request.instructions.contains("<active_skills>"));
    assert!(request.instructions.contains("# Rust Nav"));
    assert_eq!(
        request.input,
        vec![LlmInputItem::UserText("inspect main".to_string())]
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn trigger_skill_activation_injects_body() {
    let root = temp_workspace("agent_skill_trigger");
    write_skill(
        &root.join(".agents/skills/rust-nav"),
        "rust-nav",
        &["rust symbol"],
    );
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("ok".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(config_with_skill_dirs(&root), provider.clone());

    let mut rx = agent.start_turn(
        "Find this Rust symbol".to_string(),
        CancellationToken::new(),
    );
    while rx.recv().await.is_some() {}

    let request = provider.requests().pop().expect("request");
    assert!(request.instructions.contains("<active_skills>"));
    assert!(request.instructions.contains("# Rust Nav"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn classifier_verdict_parses_strict_json_action_field() {
    let verdict =
        parse_classifier_verdict(r#"{"action": "deny", "reason": "curl piped to bash is unsafe"}"#);
    assert_eq!(verdict.action, PermissionAction::Deny);
    assert!(verdict.reason.contains("denied"));

    let permissive =
        parse_classifier_verdict(r#"{"action": "ask", "reason": "looks fine but confirm"}"#);
    assert_eq!(permissive.action, PermissionAction::Ask);
    assert!(permissive.reason.contains("requires approval"));
}

#[test]
fn classifier_verdict_defaults_to_ask_when_action_is_unknown_or_allow() {
    // Even if the model says "allow" we must not flip to Allow.
    let verdict = parse_classifier_verdict(r#"{"action": "allow", "reason": "x"}"#);
    assert_eq!(verdict.action, PermissionAction::Ask);

    let verdict = parse_classifier_verdict("not even json");
    assert_eq!(verdict.action, PermissionAction::Ask);

    // Loose action: "deny" but missing JSON braces is still recognized.
    let loose = parse_classifier_verdict("action: deny because rm -rf /");
    assert_eq!(loose.action, PermissionAction::Deny);
}

#[test]
fn classifier_verdict_does_not_match_action_inside_reason_text() {
    // The previous substring heuristic would have fired on this. The new
    // parser pulls the literal `action` field and ignores prose.
    let verdict = parse_classifier_verdict(
        r#"{"action": "ask", "reason": "if we later wanted to deny we could"}"#,
    );
    assert_eq!(verdict.action, PermissionAction::Ask);
}

#[test]
fn plan_mode_denies_mutating_capabilities_before_policy() {
    for capability in [
        PermissionCapability::Edit,
        PermissionCapability::Shell,
        PermissionCapability::Git,
        PermissionCapability::Network,
        PermissionCapability::Mcp,
        PermissionCapability::Compiler,
        PermissionCapability::Destructive,
    ] {
        let request = permission_request_for_capability(capability);
        let verdict = mode_permission_verdict(SessionMode::Plan, &request)
            .expect("plan mode should deny mutating capability");
        assert_eq!(verdict.action, PermissionAction::Deny);
        assert_eq!(verdict.matched_rule, None);
        assert_eq!(
            verdict.reason,
            format!("plan mode refuses {}", capability.as_str())
        );
    }
}

#[test]
fn plan_mode_keeps_read_and_search_on_normal_policy_path() {
    for capability in [PermissionCapability::Read, PermissionCapability::Search] {
        let request = permission_request_for_capability(capability);
        assert_eq!(mode_permission_verdict(SessionMode::Plan, &request), None);
        assert_eq!(mode_permission_verdict(SessionMode::Build, &request), None);
    }
}

#[test]
fn build_mode_never_adds_mode_denials() {
    for capability in [
        PermissionCapability::Read,
        PermissionCapability::Search,
        PermissionCapability::Edit,
        PermissionCapability::Shell,
        PermissionCapability::Git,
        PermissionCapability::Network,
        PermissionCapability::Mcp,
        PermissionCapability::Compiler,
        PermissionCapability::Destructive,
    ] {
        let request = permission_request_for_capability(capability);
        assert_eq!(mode_permission_verdict(SessionMode::Build, &request), None);
    }
}

#[test]
fn agent_session_mode_can_be_set_and_toggled() {
    let agent = Agent::new(
        AppConfig {
            session_mode: SessionMode::Plan,
            ..Default::default()
        },
        Arc::new(MockProvider::new(Vec::new())),
    );

    assert_eq!(agent.session_mode(), SessionMode::Plan);
    assert!(agent.set_session_mode(SessionMode::Build, "test"));
    assert_eq!(agent.session_mode(), SessionMode::Build);
    assert!(!agent.set_session_mode(SessionMode::Build, "test"));
    assert_eq!(agent.toggle_session_mode("test"), SessionMode::Plan);
    assert_eq!(agent.session_mode(), SessionMode::Plan);
}

#[test]
fn agent_session_mode_transition_logs_structured_fields() {
    let writer = SharedLogWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer.clone())
        .with_max_level(tracing::Level::INFO)
        .finish();
    let agent = Agent::new(
        AppConfig {
            session_mode: SessionMode::Plan,
            ..Default::default()
        },
        Arc::new(MockProvider::new(Vec::new())),
    );

    tracing::subscriber::with_default(subscriber, || {
        assert!(agent.set_session_mode(SessionMode::Build, "test_transition"));
    });

    let logs = writer.contents();
    assert!(logs.contains("from_mode=plan"), "missing from_mode: {logs}");
    assert!(logs.contains("to_mode=build"), "missing to_mode: {logs}");
    assert!(
        logs.contains("source=\"test_transition\"") || logs.contains("source=test_transition"),
        "missing source: {logs}",
    );
    assert!(
        logs.contains("session mode transition"),
        "missing message: {logs}",
    );
}

#[test]
fn advertised_tool_specs_are_mode_aware() {
    let tools = [
        ("diff_context", PermissionCapability::Read),
        ("glob", PermissionCapability::Search),
        ("grep", PermissionCapability::Search),
        ("list_skills", PermissionCapability::Read),
        ("load_skill", PermissionCapability::Read),
        ("read_file", PermissionCapability::Read),
        ("read_tool_output", PermissionCapability::Read),
        ("shell", PermissionCapability::Shell),
        ("symbol_context", PermissionCapability::Read),
        ("verify", PermissionCapability::Compiler),
        ("webfetch", PermissionCapability::Network),
        ("websearch", PermissionCapability::Network),
        ("write_file", PermissionCapability::Edit),
    ]
    .map(|(name, capability)| test_advertised_tool(name, capability));

    let build_specs = advertised_tool_specs(&tools, SessionMode::Build);
    let build_names = advertised_tool_names(&build_specs);
    assert_eq!(
        build_names,
        tools
            .iter()
            .map(|tool| tool.spec.name.as_str())
            .collect::<Vec<_>>()
    );

    let plan_specs = advertised_tool_specs(&tools, SessionMode::Plan);
    let plan_names = advertised_tool_names(&plan_specs);
    assert_eq!(
        plan_names,
        vec![
            "diff_context",
            "glob",
            "grep",
            "list_skills",
            "load_skill",
            "read_file",
            "read_tool_output",
            "symbol_context",
        ]
    );
}

#[test]
fn registry_specs_carry_capability_aligned_with_permission_request() {
    let tools = ToolRegistry::new("/tmp").expect("registry");
    for spec in tools.specs() {
        let call = ToolCall {
            call_id: "probe".to_string(),
            name: spec.name.clone(),
            arguments: serde_json::json!({}),
        };
        let runtime_capability = tools.permission_request(&call).capability;
        let advertised = !mode_refuses_capability(SessionMode::Plan, spec.capability);
        let runtime = !mode_refuses_capability(SessionMode::Plan, runtime_capability);
        assert_eq!(
            advertised, runtime,
            "{}: advertised_capability={:?} runtime_capability={:?} disagree on plan-mode admittance",
            spec.name, spec.capability, runtime_capability,
        );
    }
}

#[test]
fn persistence_guard_refuses_allow_on_destructive_capability() {
    let request = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Destructive,
        target: "rm:*".to_string(),
        risk: PermissionRisk::Critical,
        summary: "rm -rf node_modules".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    };
    assert!(
        permission_rule_for_persistence(
            &request,
            PermissionRuleSource::User,
            PermissionAction::Allow
        )
        .is_none(),
        "destructive Allow rules must never be persisted",
    );

    // Deny is allowed - users can permanently block a destructive prefix.
    let deny = permission_rule_for_persistence(
        &request,
        PermissionRuleSource::User,
        PermissionAction::Deny,
    );
    assert!(deny.is_some());
    assert_eq!(deny.expect("deny rule").action, PermissionAction::Deny);
}

#[test]
fn persistence_guard_refuses_allow_with_star_target() {
    let request = PermissionRequest {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Shell,
        target: "*".to_string(),
        risk: PermissionRisk::High,
        summary: "any shell".to_string(),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    };
    assert!(
        permission_rule_for_persistence(
            &request,
            PermissionRuleSource::User,
            PermissionAction::Allow
        )
        .is_none(),
        "Allow rules with a catch-all target must not be persisted",
    );
}

#[tokio::test]
async fn allow_project_rule_takes_effect_within_the_same_session_and_writes_squeezy_toml() {
    let root = temp_workspace("agent_session_rule");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let expected_sha256 = sha256_hex("before".as_bytes());

    let first_call = LlmToolCall {
        call_id: "write_1".to_string(),
        name: "write_file".to_string(),
        arguments: json!({
            "path": "sample.txt",
            "content": "after-1",
            "expected_sha256": expected_sha256,
        }),
    };
    let second_call = LlmToolCall {
        call_id: "write_2".to_string(),
        name: "write_file".to_string(),
        arguments: json!({
            "path": "sample.txt",
            "content": "after-2",
            "expected_sha256": sha256_hex("after-1".as_bytes()),
        }),
    };

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(first_call)),
            Ok(LlmEvent::ToolCall(second_call)),
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

    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            edit: PermissionMode::Ask,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("edit sample twice".to_string(), CancellationToken::new());
    let mut approvals_seen = 0usize;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested { decision_tx, .. } = event {
            approvals_seen += 1;
            decision_tx
                .send(ToolApprovalDecision::AllowRuleProject)
                .expect("send AllowRuleProject");
        }
    }

    assert_eq!(
        approvals_seen, 1,
        "session rule should auto-approve the second matching write_file",
    );
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).expect("read sample"),
        "after-2",
        "second write must run after the session rule takes effect",
    );

    let session_rules = agent.session_rules_snapshot();
    assert_eq!(session_rules.len(), 1, "exactly one rule should be cached");
    assert_eq!(session_rules[0].source, PermissionRuleSource::Project);
    assert_eq!(session_rules[0].capability, "edit");
    assert_eq!(session_rules[0].target, "path:sample.txt");

    let written = fs::read_to_string(root.join("squeezy.toml")).expect("read project settings");
    assert!(written.contains("[[permissions.rules]]"));
    assert!(written.contains("target = \"path:sample.txt\""));

    let _ = fs::remove_dir_all(root);
}

fn permission_request_for_capability(capability: PermissionCapability) -> PermissionRequest {
    PermissionRequest {
        call_id: "call".to_string(),
        tool_name: capability.as_str().to_string(),
        capability,
        target: "target:*".to_string(),
        risk: PermissionRisk::Medium,
        summary: format!("{} request", capability.as_str()),
        metadata: BTreeMap::new(),
        suggested_rules: Vec::new(),
    }
}

fn test_advertised_tool(name: &str, capability: PermissionCapability) -> AdvertisedTool {
    advertised_tool(ToolSpec {
        name: name.to_string(),
        description: format!("{name} test tool"),
        capability,
        parameters: json!({"type": "object"}),
    })
}

fn advertised_tool_names(specs: &[LlmToolSpec]) -> Vec<&str> {
    specs.iter().map(|spec| spec.name.as_str()).collect()
}

#[derive(Clone, Default)]
struct SharedLogWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl SharedLogWriter {
    fn contents(&self) -> String {
        let bytes = self.buffer.lock().expect("log buffer").clone();
        String::from_utf8(bytes).expect("logs are UTF-8")
    }
}

impl<'writer> MakeWriter<'writer> for SharedLogWriter {
    type Writer = SharedLogWrite;

    fn make_writer(&'writer self) -> Self::Writer {
        SharedLogWrite {
            buffer: self.buffer.clone(),
        }
    }
}

struct SharedLogWrite {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for SharedLogWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer
            .lock()
            .expect("log buffer")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn temp_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy_{name}_{nonce}"));
    fs::create_dir_all(&root).expect("create temp workspace");
    root
}

fn config_with_skill_dirs(root: &Path) -> AppConfig {
    AppConfig {
        workspace_root: root.to_path_buf(),
        skills: SkillsConfig {
            user_dir: root.join("user-skills"),
            compat_user_dir: root.join("compat-skills"),
        },
        ..Default::default()
    }
}

fn write_skill(dir: &Path, name: &str, triggers: &[&str]) {
    fs::create_dir_all(dir).expect("mkdir skill");
    let triggers = triggers
        .iter()
        .map(|trigger| format!("  - {trigger}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: Rust navigation skill\ntriggers:\n{triggers}\n---\n# Rust Nav\n\nUse graph tools.\n"
        ),
    )
    .expect("write skill");
}
