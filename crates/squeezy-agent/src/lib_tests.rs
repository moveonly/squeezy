use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_core::Stream;
use futures_util::stream;
use serde_json::json;
use squeezy_core::{AppConfig, PermissionMode, PermissionPolicy, Result, SkillsConfig};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall};
use squeezy_tools::{ToolStatus, sha256_hex};

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
            summary = Some(request.summary.clone());
            let _ = decision_tx.send(ToolApprovalDecision::Denied);
        }
    }

    let summary = summary.expect("approval summary");
    assert!(
        !summary.contains("abcdefghijklmnopqrstuvwxyz"),
        "approval summary leaked bearer token: {summary:?}",
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
