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
    AppConfig, ContextAttachmentKind, PermissionAction, PermissionCapability, PermissionMode,
    PermissionPolicy, PermissionRequest, PermissionRisk, PermissionRuleSource, Result,
    SessionLogConfig, SessionMode, ShellSandboxMode, SkillsConfig, SubagentConfig, TaskStateStatus,
};
use squeezy_llm::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, LlmToolSpec,
};
use squeezy_tools::{ToolCall, ToolStatus, sha256_hex};
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

struct HangingProvider {
    requests: Mutex<Vec<LlmRequest>>,
}

impl HangingProvider {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl LlmProvider for HangingProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        self.requests.lock().expect("requests").push(request);
        Box::pin(stream::pending())
    }
}

async fn wait_for_job_status(jobs: &JobRegistry, id: JobId, expected: JobStatus) -> JobSnapshot {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(job) = jobs.get(id)
                && job.status == expected
            {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job reached expected status")
}

#[test]
fn job_registry_tracks_lifecycle_and_bounds_notifications() {
    let jobs = JobRegistry::new();
    let first = jobs.create(
        JobKind::Shell,
        "run shell",
        Some(TurnId::new(7)),
        Some("shell".to_string()),
        Some("call_1".to_string()),
        CancellationToken::new(),
    );

    assert_eq!(first.id, 1);
    assert_eq!(first.status, JobStatus::Queued);
    assert_eq!(jobs.snapshot().len(), 1);

    let running = jobs.start(first.id).expect("started");
    assert_eq!(running.status, JobStatus::Running);
    assert!(running.status.is_active());

    let done = jobs
        .finish(
            first.id,
            JobStatus::Completed,
            "shell Success output=2B",
            Some("handle-1".to_string()),
        )
        .expect("finished");
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.output_handle.as_deref(), Some("handle-1"));
    assert_eq!(jobs.notifications().len(), 1);

    let cancel = CancellationToken::new();
    let cancellable = jobs.create(
        JobKind::Tool,
        "cancellable",
        None,
        None,
        None,
        cancel.clone(),
    );
    assert!(jobs.cancel(cancellable.id));
    assert!(cancel.is_cancelled());
    assert_eq!(
        jobs.get(cancellable.id)
            .and_then(|job| job.progress)
            .map(|progress| progress.message),
        Some("cancellation requested".to_string())
    );

    for index in 0..25 {
        let job = jobs.create(
            JobKind::Tool,
            format!("job {index}"),
            None,
            None,
            None,
            CancellationToken::new(),
        );
        jobs.finish(job.id, JobStatus::Failed, "failed", None)
            .expect("finish job");
    }

    assert_eq!(jobs.notifications().len(), MAX_JOB_NOTIFICATIONS);
}

#[tokio::test]
async fn panicked_job_transitions_to_failed_status() {
    let jobs = JobRegistry::new();
    let job = jobs.create(
        JobKind::Tool,
        "panic job",
        None,
        None,
        None,
        CancellationToken::new(),
    );
    jobs.start(job.id).expect("started");
    let done = Arc::new(Notify::new());
    let handle = spawn_observed_job(jobs.clone(), job.id, done.clone(), async {
        panic!("intentional job panic");
    });
    assert!(jobs.attach_handle(job.id, handle.abort_handle(), done));

    let failed = wait_for_job_status(&jobs, job.id, JobStatus::Failed).await;
    assert_eq!(failed.result_summary.as_deref(), Some("job panicked"));
}

#[tokio::test]
async fn slow_job_is_hard_aborted_after_grace_window() {
    let jobs = JobRegistry::new();
    let cancel = CancellationToken::new();
    let job = jobs.create(JobKind::Tool, "slow job", None, None, None, cancel);
    jobs.start(job.id).expect("started");
    let done = Arc::new(Notify::new());
    let handle = spawn_observed_job(jobs.clone(), job.id, done.clone(), async {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    assert!(jobs.attach_handle(job.id, handle.abort_handle(), done));

    assert!(jobs.cancel(job.id));
    let cancelled = wait_for_job_status(&jobs, job.id, JobStatus::Cancelled).await;
    assert_eq!(
        cancelled.result_summary.as_deref(),
        Some("cancelled after grace window")
    );
}

#[test]
fn job_registry_prunes_completed_jobs_past_retention_cap() {
    let jobs = JobRegistry::new();
    let active_count = 5;
    let mut active_ids = Vec::new();
    for index in 0..active_count {
        let job = jobs.create(
            JobKind::Tool,
            format!("active {index}"),
            None,
            None,
            None,
            CancellationToken::new(),
        );
        active_ids.push(job.id);
    }

    let extra = MAX_JOBS_RETAINED + 50;
    for index in 0..extra {
        let job = jobs.create(
            JobKind::Tool,
            format!("done {index}"),
            None,
            None,
            None,
            CancellationToken::new(),
        );
        jobs.finish(job.id, JobStatus::Completed, "ok", None)
            .expect("finish job");
    }

    let snapshot = jobs.snapshot();
    assert!(
        snapshot.len() <= MAX_JOBS_RETAINED,
        "expected jobs map to stay bounded, got {}",
        snapshot.len()
    );
    for id in &active_ids {
        assert!(
            jobs.get(*id).is_some(),
            "active job {id} must not be pruned"
        );
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
async fn llm_stream_observes_cancellation_within_one_yield() {
    let provider = Arc::new(HangingProvider::new());
    let agent = Agent::new(AppConfig::default(), provider);
    let cancel = CancellationToken::new();
    let cancel_task = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        })
    };

    let mut rx = agent.start_turn("hi".to_string(), cancel);
    let saw_cancelled = tokio::time::timeout(Duration::from_millis(200), async {
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Cancelled { .. } => return true,
                AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
                _ => {}
            }
        }
        false
    })
    .await
    .expect("turn cancellation should not wait for another provider event");

    cancel_task.await.expect("cancel task");
    assert!(saw_cancelled);
}

#[tokio::test]
async fn task_state_tool_updates_visible_state_logs_snapshot_and_summary() {
    let root = temp_workspace("task_state_session");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "state_1".to_string(),
                name: TASK_STATE_TOOL_NAME.to_string(),
                arguments: json!({
                    "task": "Implement task UX",
                    "status": "running",
                    "steps": [
                        {"title": "Inspect TUI", "status": "completed"},
                        {"title": "Wire state panel", "status": "active", "detail": "render workflow state"}
                    ],
                    "next_action": "run focused tests",
                    "verification": "running",
                    "recent_changes": ["added state model"],
                    "replan_reason": "found existing status footer"
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
        vec![
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
    ]));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            session_logs: SessionLogConfig {
                log_dir: Some(PathBuf::from(".squeezy/sessions")),
                ..SessionLogConfig::default()
            },
            ..AppConfig::default()
        },
        provider.clone(),
    );

    let mut rx = agent.start_turn("implement task UX".to_string(), CancellationToken::new());
    let mut snapshots = Vec::new();
    let mut completed_metrics = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::TaskStateUpdated { snapshot, .. } => snapshots.push(snapshot),
            AgentEvent::Completed { metrics, .. } => completed_metrics = Some(metrics),
            _ => {}
        }
    }

    assert!(
        snapshots
            .iter()
            .any(|snapshot| snapshot.replan_reason.as_deref()
                == Some("found existing status footer")),
        "missing replan snapshot: {snapshots:?}",
    );
    assert_eq!(
        snapshots.last().map(|snapshot| snapshot.status),
        Some(TaskStateStatus::Completed)
    );
    assert_eq!(
        completed_metrics.expect("completed metrics").tool_calls,
        0,
        "control state updates must not consume the normal tool-call budget",
    );
    let request_names = provider
        .requests()
        .into_iter()
        .flat_map(|request| request.tools.into_iter().map(|tool| tool.name))
        .collect::<Vec<_>>();
    assert!(
        !request_names
            .iter()
            .any(|name| name == TASK_STATE_TOOL_NAME),
        "task-state progress must be runtime-derived, not advertised: {request_names:?}",
    );

    let session_id = agent.session_id().expect("session id");
    let exported = agent.export_session(&session_id).expect("export session");
    let events = exported["events"].as_array().expect("events array");
    let task_state_event = events
        .iter()
        .find(|event| {
            event["kind"] == "task_state"
                && event["payload"]["snapshot"]["replan_reason"] == "found existing status footer"
        })
        .expect("logged task_state event with replan reason");
    assert_eq!(
        task_state_event["payload"]["snapshot"]["steps"][1]["title"],
        "Wire state panel"
    );
    let latest_summary = exported["metadata"]["latest_summary"]
        .as_str()
        .expect("latest summary");
    assert!(
        latest_summary.contains("Implement task UX"),
        "{latest_summary}"
    );
    assert!(
        latest_summary.contains("status=completed"),
        "{latest_summary}"
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
async fn stalled_model_stream_fails_after_idle_timeout() {
    let provider = Arc::new(HangingProvider::new());
    let config = AppConfig {
        stream_idle_timeout: Duration::from_millis(10),
        ..Default::default()
    };
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut saw_timeout = false;
    tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Failed { error, .. } = event {
                saw_timeout = error
                    .to_string()
                    .contains("idle timeout waiting for model stream");
            }
        }
    })
    .await
    .expect("stalled model stream should fail promptly");

    assert!(saw_timeout);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn invalid_tool_arguments_are_returned_to_model_instead_of_failing_turn() {
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_bad".to_string(),
                name: "definition_search".to_string(),
                arguments: json!({
                    INVALID_TOOL_ARGUMENTS_KEY: true,
                    INVALID_TOOL_ARGUMENTS_ERROR_KEY: "EOF while parsing a string at line 1 column 59",
                    INVALID_TOOL_ARGUMENTS_RAW_KEY: "{\"query\":\"getFoo",
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: None,
                cost: Default::default(),
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("recovered".to_string())),
            Ok(LlmEvent::Completed {
                response_id: None,
                cost: Default::default(),
            }),
        ],
    ]));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.start_turn("where is getFoo?".to_string(), CancellationToken::new());
    let mut completed = None;
    let mut failed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            AgentEvent::Failed { error, .. } => failed = Some(error.to_string()),
            _ => {}
        }
    }

    assert_eq!(completed.as_deref(), Some("recovered"));
    assert!(failed.is_none(), "{failed:?}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1]
        .input
        .iter()
        .find_map(|item| match item {
            LlmInputItem::FunctionCallOutput { output, .. } => Some(output),
            _ => None,
        })
        .expect("tool output returned to model");
    assert!(
        output.contains("invalid tool arguments from model"),
        "{output}"
    );
    assert!(output.contains("call the same tool again"), "{output}");
}

#[tokio::test]
async fn session_replay_replays_recorded_model_response() {
    let root = temp_workspace("session_replay");
    let config = AppConfig {
        workspace_root: root.clone(),
        session_logs: SessionLogConfig {
            log_dir: Some(PathBuf::from(".squeezy/sessions")),
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("hello from replay".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_replay".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(config.clone(), provider);

    let mut rx = agent.start_turn("say hello".to_string(), CancellationToken::new());
    let mut final_answer = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::AssistantDelta { delta, .. } => final_answer.push_str(&delta),
            AgentEvent::Completed { message, .. } if final_answer.is_empty() => {
                final_answer = message.content
            }
            _ => {}
        }
    }
    assert_eq!(final_answer, "hello from replay");

    let session_id = agent.session_id().expect("session id");
    let record = agent.show_session(&session_id).expect("show session");
    let replay = record.replay.expect("replay tape");
    assert!(
        replay
            .events
            .iter()
            .any(|event| event.kind == SessionReplayEventKind::ModelRequest),
        "expected replay model request: {replay:?}",
    );

    let report = Agent::replay_session(config.clone(), &session_id)
        .await
        .expect("replay session");
    assert_eq!(report.turns, 1);
    assert_eq!(report.final_answer, "hello from replay");

    let mut drifted = config;
    drifted.instructions.push_str("\nextra replay drift");
    let error = Agent::replay_session(drifted, &session_id)
        .await
        .expect_err("drift should fail replay");
    assert!(
        error.to_string().contains("model request diverged"),
        "{error}",
    );
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
async fn pasted_context_is_redacted_deduped_and_sent_as_reference() {
    let root = temp_workspace("agent_context_attachment");
    let provider = Arc::new(MockProvider::new(vec![vec![Ok(LlmEvent::Completed {
        response_id: Some("resp_1".to_string()),
        cost: CostSnapshot::default(),
    })]]));
    let config = AppConfig {
        workspace_root: root.clone(),
        tool_preview_bytes: 96,
        ..Default::default()
    };
    let agent = Agent::new(config, provider.clone());
    let raw = "2026-05-24 ERROR failed\nOPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz\n";

    let first = agent
        .attach_pasted_context(raw.to_string())
        .await
        .expect("attach paste");
    let second = agent
        .attach_pasted_context(raw.to_string())
        .await
        .expect("dedupe paste");

    assert!(first.active);
    assert!(second.duplicate);
    assert_eq!(first.attachment.id, second.attachment.id);
    assert!(
        !first
            .attachment
            .preview
            .contains("sk-abcdefghijklmnopqrstuvwxyz")
    );
    assert!(first.attachment.preview.contains("<redacted:"));

    let mut rx = agent.start_turn("explain failure".to_string(), CancellationToken::new());
    while rx.recv().await.is_some() {}

    let requests = provider.requests();
    let LlmInputItem::UserText(request_text) = &requests[0].input[0] else {
        panic!("expected user text");
    };
    assert!(request_text.contains("explain failure"));
    assert!(request_text.contains(&format!("attachment://{}", first.attachment.id)));
    assert!(request_text.contains("redacted_preview"));
    assert!(!request_text.contains("sk-abcdefghijklmnopqrstuvwxyz"));

    let session_id = agent.session_id().expect("session id");
    let record = agent.show_session(&session_id).expect("show session");
    assert_eq!(record.attachments.len(), 1);
    assert_eq!(record.attachments[0].id, first.attachment.id);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn unsupported_image_attachment_is_not_active() {
    let root = temp_workspace("agent_context_image");
    let image = root.join("screenshot.png");
    fs::write(&image, b"\x89PNG\r\n\x1a\nimage bytes").expect("write image");
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let config = AppConfig {
        workspace_root: root.clone(),
        ..Default::default()
    };
    let agent = Agent::new(config, provider);

    let update = agent
        .attach_file_context(PathBuf::from("screenshot.png"))
        .await
        .expect("attach image");

    assert!(!update.active);
    assert_eq!(
        update.attachment.kind,
        ContextAttachmentKind::UnsupportedImage
    );
    assert!(agent.context_attachments_snapshot().await.is_empty());

    let _ = fs::remove_dir_all(root);
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
    let mut approval_payload: Option<(String, BTreeMap<String, String>)> = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested {
            request,
            decision_tx,
            ..
        } = event
        {
            approval_payload = Some((
                request.summary().to_string(),
                request.permission.metadata.clone(),
            ));
            let _ = decision_tx.send(ToolApprovalDecision::Denied);
        }
    }

    let (summary, metadata) = approval_payload.expect("approval payload");
    // The summary now only carries the description so secret bearing
    // commands never reach it in the first place. The command (and any
    // other metadata values) are still redacted before being surfaced.
    // Avoid interpolating any of the redacted values into the panic
    // messages so CodeQL doesn't flag cleartext logging.
    assert!(
        !summary.contains("abcdefghijklmnopqrstuvwxyz"),
        "approval summary leaked bearer token",
    );
    let command_meta = metadata
        .get("command")
        .expect("shell approval metadata must include command");
    assert!(
        !command_meta.contains("abcdefghijklmnopqrstuvwxyz"),
        "approval metadata leaked bearer token",
    );
    assert!(
        command_meta.contains("<redacted:bearer_token"),
        "approval metadata must carry the redaction marker",
    );

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

#[test]
fn provider_capability_gate_controls_native_reasoning_and_verbosity_fields() {
    let mut config = AppConfig {
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string(),
        reasoning_effort: Some(squeezy_core::ReasoningEffort::High),
        ..Default::default()
    };
    config.tui.response_verbosity = squeezy_core::ResponseVerbosity::Verbose;

    assert_eq!(
        request_response_verbosity(&config, "openai"),
        Some(squeezy_core::ResponseVerbosity::Verbose)
    );
    assert_eq!(
        request_reasoning_effort(&config, "openai"),
        Some(squeezy_core::ReasoningEffort::High)
    );
    assert_eq!(request_response_verbosity(&config, "anthropic"), None);
    assert_eq!(request_reasoning_effort(&config, "anthropic"), None);
}

#[test]
fn instructions_skip_prompt_hint_on_default_and_native_capable_models() {
    use squeezy_core::ResponseVerbosity;

    let base = "system rules";

    // Normal verbosity is the implicit default; never burn tokens
    // re-stating it in the prompt.
    assert_eq!(
        instructions_with_response_verbosity(base, ResponseVerbosity::Normal, false),
        base,
    );

    // Native-capable providers receive verbosity via the API
    // parameter; do not pay for a redundant prompt-side hint.
    assert_eq!(
        instructions_with_response_verbosity(base, ResponseVerbosity::Concise, true),
        base,
    );

    // For non-native providers, surface the hint so the model still
    // gets the signal.
    let concise = instructions_with_response_verbosity(base, ResponseVerbosity::Concise, false);
    assert!(concise.starts_with(base), "{concise}");
    assert!(concise.contains("Response verbosity: concise"), "{concise}");

    let verbose = instructions_with_response_verbosity(base, ResponseVerbosity::Verbose, false);
    assert!(verbose.contains("Response verbosity: verbose"), "{verbose}");
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
                    reasoning_output_tokens: Some(2),
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
                    reasoning_output_tokens: Some(3),
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
    assert_eq!(cost.reasoning_output_tokens, Some(5));
    assert_eq!(provider.requests().len(), 2);
    assert!(!provider.requests()[0].tools.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_tool_emits_job_events_and_session_events() {
    let root = temp_workspace("agent_shell_job");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf job-ok",
                    "description": "print a marker",
                    "timeout_ms": 10_000,
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
        vec![Ok(LlmEvent::Completed {
            response_id: Some("resp_2".to_string()),
            cost: CostSnapshot::default(),
        })],
    ]));
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            shell: PermissionMode::Allow,
            shell_sandbox: squeezy_core::ShellSandboxConfig {
                mode: ShellSandboxMode::Off,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider);
    let session_id = agent.session_id().expect("session id");

    let mut rx = agent.start_turn("run shell".to_string(), CancellationToken::new());
    let mut job_statuses = Vec::new();
    let mut notification = None;
    let mut shell_result = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::JobUpdated { job } => job_statuses.push(job.status),
            AgentEvent::JobNotification { notification: seen } => notification = Some(seen),
            AgentEvent::ToolCallCompleted { result, .. } => shell_result = Some(result),
            _ => {}
        }
    }

    assert_eq!(
        shell_result.expect("shell result").status,
        ToolStatus::Success
    );
    assert!(
        job_statuses.contains(&JobStatus::Running),
        "missing running job event: {job_statuses:?}"
    );
    assert!(
        job_statuses.contains(&JobStatus::Completed),
        "missing completed job event: {job_statuses:?}"
    );
    let notification = notification.expect("job notification");
    assert_eq!(notification.status, JobStatus::Completed);

    let jobs = agent.jobs_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, JobKind::Shell);
    assert_eq!(jobs[0].status, JobStatus::Completed);

    let record = agent.show_session(&session_id).expect("session record");
    let event_kinds = record
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect::<Vec<_>>();
    assert!(event_kinds.contains(&"job_queued"));
    assert!(event_kinds.contains(&"job_started"));
    assert!(event_kinds.contains(&"job_finished"));

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
    let agent = Agent::new(config, provider.clone());
    let cancel = CancellationToken::new();
    let mut rx = agent.start_turn("write file".to_string(), cancel.clone());
    let mut pending_decision = None;
    let mut saw_cancelled_tool = false;
    let mut saw_cancelled_turn = false;

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
                AgentEvent::Cancelled { .. } => {
                    saw_cancelled_turn = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("turn should not block on unanswered approval after cancellation");

    assert!(pending_decision.is_some());
    assert!(saw_cancelled_tool);
    assert!(saw_cancelled_turn);
    assert_eq!(
        provider.requests().len(),
        1,
        "cancelled approval must not feed a cancelled tool result into another model round",
    );
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
    assert!(request.instructions.contains("<available_skills>"));
    assert!(request.instructions.contains("rust-nav"));
    assert!(!request.instructions.contains("<active_skills>"));
    assert!(!request.instructions.contains("Rust Nav"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn squeezy_help_self_question_completes_without_provider_request() {
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.start_turn(
        "How do I configure Squeezy providers?".to_string(),
        CancellationToken::new(),
    );
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { message, .. } = event {
            completed = Some(message.content);
        }
    }

    assert!(provider.requests().is_empty());
    let completed = completed.expect("help turn should complete");
    assert!(completed.contains("Squeezy help"), "{completed}");
    assert!(
        completed.contains("docs/external/PROVIDERS.md"),
        "{completed}"
    );
    assert!(completed.contains("[model]"), "{completed}");
}

#[tokio::test]
async fn bang_command_completes_locally_without_provider_request() {
    let root = temp_workspace("agent_local_bang");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write cargo");
    fs::create_dir_all(root.join("src")).expect("mkdir src");
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        },
        provider.clone(),
    );

    let mut rx = agent.start_turn("!ls".to_string(), CancellationToken::new());
    let mut completed = None;
    let mut tool_result = None;
    let mut queued_tools = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ToolCallQueued { call, .. } => queued_tools.push(call),
            AgentEvent::ToolCallCompleted { result, .. } => tool_result = Some(result),
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            _ => {}
        }
    }

    assert!(provider.requests().is_empty());
    assert_eq!(queued_tools.len(), 1);
    assert_eq!(queued_tools[0].arguments["command"], "ls");
    let tool_result = tool_result.expect("ls should run through the shell tool");
    assert_eq!(tool_result.tool_name, "shell");
    assert_eq!(tool_result.status, ToolStatus::Success);
    assert_eq!(tool_result.content["policy"]["direct_user_shell"], true);
    assert_eq!(tool_result.content["sandbox"]["backend"], "none");
    let completed = completed.expect("ls turn should complete");
    assert!(completed.contains("Cargo.toml"), "{completed}");
    assert!(completed.contains("src"), "{completed}");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn natural_language_run_ls_phrase_goes_to_provider() {
    let root = temp_workspace("agent_natural_run_ls");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write cargo");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(
            "I would run `ls -la` for detailed output.".to_string(),
        )),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        },
        provider.clone(),
    );

    let mut rx = agent.start_turn("run ls with details".to_string(), CancellationToken::new());
    let mut completed = None;
    let mut queued_tools = Vec::new();
    let mut tool_results = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ToolCallQueued { call, .. } => queued_tools.push(call),
            AgentEvent::ToolCallCompleted { result, .. } => tool_results.push(result),
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            _ => {}
        }
    }

    assert_eq!(provider.requests().len(), 1);
    assert!(queued_tools.is_empty());
    assert!(tool_results.is_empty());
    let completed = completed.expect("provider turn should complete");
    assert!(completed.contains("ls -la"), "{completed}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn tool_loop_guard_stops_repeated_identical_failures() {
    let call = ToolCall {
        call_id: "patch-1".to_string(),
        name: "apply_patch".to_string(),
        arguments: json!({}),
    };
    let first = control_tool_result(
        &call,
        ToolStatus::Stale,
        json!({
            "error": "search text matched more than once",
            "path": "src/lib.rs"
        }),
    );
    let second = control_tool_result(
        &ToolCall {
            call_id: "patch-2".to_string(),
            ..call.clone()
        },
        ToolStatus::Stale,
        json!({
            "error": "search text matched more than once",
            "path": "src/lib.rs"
        }),
    );
    let mut guard = ToolLoopGuard::default();

    assert!(
        guard
            .observe_round(std::slice::from_ref(&call), &[first])
            .is_none()
    );
    let reason = guard
        .observe_round(&[call], &[second])
        .expect("second identical failure should stop");

    assert!(reason.contains("repeated apply_patch failure"), "{reason}");
}

#[tokio::test]
async fn unsupported_squeezy_help_question_refuses_without_provider_request() {
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.start_turn(
        "Does Squeezy support quantum billing?".to_string(),
        CancellationToken::new(),
    );
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { message, .. } = event {
            completed = Some(message.content);
        }
    }

    assert!(provider.requests().is_empty());
    let completed = completed.expect("help turn should complete");
    assert!(completed.contains("won't guess"), "{completed}");
    assert!(
        completed.contains("https://squeezyagent.com/docs/"),
        "{completed}"
    );
    assert!(
        completed.contains("https://github.com/esqueezy/squeezy"),
        "{completed}"
    );
}

#[tokio::test]
async fn unrelated_questions_still_call_provider() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("ok".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        }),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.start_turn(
        "How do I configure serde?".to_string(),
        CancellationToken::new(),
    );
    while rx.recv().await.is_some() {}

    assert_eq!(provider.requests().len(), 1);
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

#[tokio::test]
async fn shell_implicit_skill_activation_reaches_next_model_request() {
    let root = temp_workspace("agent_skill_implicit_shell");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", &[]);
    let scripts = skill_dir.join("scripts");
    fs::create_dir_all(&scripts).expect("mkdir scripts");
    fs::write(scripts.join("init.sh"), "printf ok\n").expect("write script");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "sh .squeezy/skills/rust-nav/scripts/init.sh",
                    "description": "run skill script",
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ok".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
    ]));
    let mut config = config_with_skill_dirs(&root);
    config.permissions.shell = PermissionMode::Allow;
    config.permissions.shell_sandbox = squeezy_core::ShellSandboxConfig {
        mode: ShellSandboxMode::Off,
        ..Default::default()
    };
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("run setup".to_string(), CancellationToken::new());
    let mut shell_result = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolCallCompleted { result, .. } = event {
            shell_result = Some(result);
        }
    }

    let shell_result = shell_result.expect("shell result");
    assert_eq!(
        shell_result.content["implicit_skill_activation"]["name"],
        "rust-nav"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].instructions.contains("# Rust Nav"));
    assert!(requests[1].instructions.contains("<active_skills>"));
    assert!(requests[1].instructions.contains("# Rust Nav"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_approval_event_surfaces_new_sandbox_metadata() {
    // End-to-end: drive a shell call through the agent's approval path,
    // capture the AgentEvent::ApprovalRequested payload, and confirm the
    // new metadata keys (timeout_ms, output_byte_cap, sandbox,
    // sandbox_network, parser_backed) are present and that the env value
    // is a policy label, not a raw env-var dump.
    let root = temp_workspace("agent_approval_metadata");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::ToolCall(LlmToolCall {
            call_id: "call_meta".to_string(),
            name: "shell".to_string(),
            arguments: json!({
                "command": "cargo test --workspace",
                "description": "run tests",
                "timeout_ms": 45_000,
                "output_byte_cap": 16_000,
            }),
        })),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_meta".to_string()),
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
    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut captured = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested {
            request,
            decision_tx,
            ..
        } = event
        {
            captured = Some(request.permission.metadata.clone());
            let _ = decision_tx.send(ToolApprovalDecision::Denied);
        }
    }
    let metadata = captured.expect("approval metadata");
    for key in [
        "command",
        "cwd",
        "env",
        "network",
        "destructive",
        "timeout_ms",
        "output_byte_cap",
        "sandbox",
        "sandbox_network",
        "parser_backed",
    ] {
        assert!(metadata.contains_key(key), "missing approval key {key}");
    }
    assert_eq!(metadata["timeout_ms"], "45000");
    assert_eq!(metadata["output_byte_cap"], "16000");
    // The env label may never carry raw env values.
    let env_label = &metadata["env"];
    assert!(
        env_label.contains("allowlist"),
        "env label should reference allowlist policy",
    );
    assert!(
        !env_label.contains("PATH="),
        "env label must not include raw env values",
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn network_shell_command_is_denied_by_network_permission_policy() {
    let root = temp_workspace("agent_shell_network_policy");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "curl https://example.com",
                    "description": "fetch"
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
            }),
        ],
        vec![Ok(LlmEvent::Completed {
            response_id: Some("resp_2".to_string()),
            cost: CostSnapshot::default(),
        })],
    ]));
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            web: PermissionMode::Deny,
            shell: PermissionMode::Allow,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("fetch".to_string(), CancellationToken::new());
    let mut denied = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolCallCompleted { result, .. } = event
            && result.call_id == "call_1"
        {
            denied = Some(result);
        }
    }

    let denied = denied.expect("shell result");
    assert_eq!(denied.status, ToolStatus::Denied);
    assert_eq!(denied.content["permission_denied"], true);
    let error = denied.content["error"].as_str().expect("error");
    assert!(error.contains("default network permission is deny"));

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
        ("apply_patch", PermissionCapability::Edit),
        ("decl_search", PermissionCapability::Search),
        ("definition_search", PermissionCapability::Search),
        ("diff_context", PermissionCapability::Read),
        ("downstream_flow", PermissionCapability::Read),
        ("glob", PermissionCapability::Search),
        ("grep", PermissionCapability::Search),
        ("hierarchy", PermissionCapability::Read),
        ("list_skills", PermissionCapability::Read),
        ("load_skill", PermissionCapability::Read),
        ("plan_patch", PermissionCapability::Read),
        ("read_file", PermissionCapability::Read),
        ("read_slice", PermissionCapability::Read),
        ("read_tool_output", PermissionCapability::Read),
        ("reference_search", PermissionCapability::Search),
        ("repo_map", PermissionCapability::Read),
        ("shell", PermissionCapability::Shell),
        ("symbol_context", PermissionCapability::Read),
        ("upstream_flow", PermissionCapability::Read),
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
}

#[test]
fn control_tools_are_advertised_in_build_and_plan_modes() {
    let tools = core_control_tools(&SubagentConfig::default());

    let expected = vec![
        DELEGATE_TOOL_NAME,
        EXPLORE_TOOL_NAME,
        DELEGATE_PLAN_TOOL_NAME,
        DELEGATE_REVIEW_TOOL_NAME,
    ];

    let build_specs = advertised_tool_specs(&tools, SessionMode::Build);
    let build_names = advertised_tool_names(&build_specs);
    assert_eq!(build_names, expected);

    let plan_specs = advertised_tool_specs(&tools, SessionMode::Plan);
    let plan_names = advertised_tool_names(&plan_specs);
    assert_eq!(plan_names, expected);
}

#[test]
fn core_control_tools_filter_subagents_when_disabled() {
    let subagents = SubagentConfig {
        enabled: false,
        ..SubagentConfig::default()
    };
    let names: Vec<_> = core_control_tools(&subagents)
        .into_iter()
        .map(|tool| tool.spec.name)
        .collect();
    assert!(names.is_empty());

    let explore_only_off = SubagentConfig {
        explore_enabled: false,
        ..SubagentConfig::default()
    };
    let names: Vec<_> = core_control_tools(&explore_only_off)
        .into_iter()
        .map(|tool| tool.spec.name)
        .collect();
    assert_eq!(
        names,
        vec![
            DELEGATE_TOOL_NAME.to_string(),
            DELEGATE_PLAN_TOOL_NAME.to_string(),
            DELEGATE_REVIEW_TOOL_NAME.to_string(),
        ]
    );
}

#[test]
fn warn_unknown_tool_schema_names_emits_warning_for_typo_and_skips_known() {
    let writer = SharedLogWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();

    let tools = [
        test_advertised_tool("grep", PermissionCapability::Search),
        test_advertised_tool("webfetch", PermissionCapability::Network),
    ];
    let schema_config = squeezy_core::ToolSchemaConfig {
        lazy_schema_loading: true,
        core: vec![
            "grep".to_string(),
            "webfectch".to_string(), // intentional typo
            "load_tool_schema".to_string(),
            "delegate".to_string(),
            "explore".to_string(),
        ],
        discoverable: vec!["totally_made_up".to_string()],
    };

    tracing::subscriber::with_default(subscriber, || {
        warn_unknown_tool_schema_names(&tools, &schema_config);
    });

    let logs = writer.contents();
    assert!(
        logs.contains("webfectch"),
        "typo in [tools].core should appear in warn output: {logs}"
    );
    assert!(
        logs.contains("totally_made_up"),
        "typo in [tools].discoverable should appear in warn output: {logs}"
    );
    assert!(
        !logs.contains("\"grep\"") && !logs.contains("tool=grep"),
        "known names must not trigger a warning: {logs}"
    );
    assert!(
        !logs.contains("load_tool_schema"),
        "synthetic always-core names must not trigger a warning: {logs}"
    );
    assert!(
        !logs.contains("delegate") && !logs.contains("explore"),
        "subagent control tools must not trigger a warning: {logs}"
    );
}

#[test]
fn lazy_request_tool_specs_keep_core_first_and_mcp_discoverable_by_default() {
    let mut tools = core_control_tools(&SubagentConfig::default());
    tools.extend([
        test_advertised_tool("grep", PermissionCapability::Search),
        test_advertised_tool("webfetch", PermissionCapability::Network),
        test_advertised_tool("mcp__docs__lookup", PermissionCapability::Mcp),
    ]);
    let config = ToolSchemaConfig::default();

    let initial_specs = request_tool_specs(&tools, SessionMode::Build, &config, &[]);
    let initial_names = advertised_tool_names(&initial_specs);
    assert_eq!(
        initial_names,
        vec![
            DELEGATE_TOOL_NAME,
            EXPLORE_TOOL_NAME,
            LOAD_TOOL_SCHEMA_TOOL_NAME,
            "grep"
        ]
    );

    let loaded_specs = request_tool_specs(
        &tools,
        SessionMode::Build,
        &config,
        &[
            "mcp__docs__lookup".to_string(),
            "webfetch".to_string(),
            "mcp__docs__lookup".to_string(),
        ],
    );
    let loaded_names = advertised_tool_names(&loaded_specs);
    assert_eq!(
        loaded_names,
        vec![
            DELEGATE_TOOL_NAME,
            EXPLORE_TOOL_NAME,
            LOAD_TOOL_SCHEMA_TOOL_NAME,
            "grep",
            "mcp__docs__lookup",
            "webfetch",
        ]
    );
}

#[test]
fn request_tool_specs_skips_disabled_subagent_control_tools() {
    let subagents = SubagentConfig {
        enabled: false,
        ..SubagentConfig::default()
    };
    let mut tools = core_control_tools(&subagents);
    tools.push(test_advertised_tool("grep", PermissionCapability::Search));
    let config = ToolSchemaConfig::default();

    let specs = request_tool_specs(&tools, SessionMode::Build, &config, &[]);
    let names = advertised_tool_names(&specs);
    assert!(
        !names.contains(&DELEGATE_TOOL_NAME),
        "delegate must not be advertised when subagents.enabled=false: {names:?}"
    );
    assert!(
        !names.contains(&EXPLORE_TOOL_NAME),
        "explore must not be advertised when subagents.enabled=false: {names:?}"
    );
    assert!(names.contains(&"grep"));

    let explore_only = SubagentConfig {
        explore_enabled: false,
        ..SubagentConfig::default()
    };
    let mut tools = core_control_tools(&explore_only);
    tools.push(test_advertised_tool("grep", PermissionCapability::Search));
    let specs = request_tool_specs(&tools, SessionMode::Build, &config, &[]);
    let names = advertised_tool_names(&specs);
    assert!(names.contains(&DELEGATE_TOOL_NAME));
    assert!(
        !names.contains(&EXPLORE_TOOL_NAME),
        "explore must not be advertised when explore_enabled=false: {names:?}"
    );
}

#[test]
fn lazy_tools_index_lists_discoverable_tools_without_core_schemas() {
    let tools = [
        test_advertised_tool("grep", PermissionCapability::Search),
        test_advertised_tool("webfetch", PermissionCapability::Network),
        test_advertised_tool("mcp__docs__lookup", PermissionCapability::Mcp),
    ];
    let config = ToolSchemaConfig::default();

    let index = tool_schema_index(&tools, SessionMode::Build, &config).expect("index");

    assert!(index.contains("webfetch | capability=network"));
    assert!(index.contains("mcp__docs__lookup | capability=mcp"));
    assert!(!index.contains("grep | capability=search"));
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

#[tokio::test]
async fn shell_ask_approver_routes_in_flight_commands_through_permission_policy() {
    let root = temp_workspace("agent_shell_ask_policy");
    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(Vec::new()));
    let tools = ToolRegistry::new(&root).expect("registry");
    let jobs = JobRegistry::new();
    let (tx, _rx) = mpsc::channel(4);
    let advertised = Vec::<AdvertisedTool>::new();

    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            shell: PermissionMode::Deny,
            ..Default::default()
        },
        ..Default::default()
    };
    let context = ToolExecutionContext {
        turn_id: TurnId::new(1),
        provider,
        tools: &tools,
        jobs: &jobs,
        config: &config,
        telemetry: TelemetryClient::disabled(),
        redactor: Arc::new(Redactor::default()),
        tx,
        cancel: CancellationToken::new(),
        approval_ids: Arc::new(AtomicU64::new(1)),
        session_rules: Arc::new(RwLock::new(Vec::new())),
        session_mode: Arc::new(AtomicU8::new(SessionMode::Build.to_u8())),
        session_log: None,
        task_state: Arc::new(tokio::sync::Mutex::new(None)),
        all_tool_specs: &advertised,
        loaded_tool_schemas: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        exploration_state: Arc::new(tokio::sync::Mutex::new(ExplorationTurnState::from_plan(
            None,
        ))),
    };

    let approver = shell_ask_approver_for_context(&context);
    let decision = approver(ShellAskRequest {
        call_id: "shell_parent".to_string(),
        parent_command: "script.sh".to_string(),
        command: "rm -rf target".to_string(),
        justification: "nested cleanup".to_string(),
        workdir: root.clone(),
    })
    .await;

    assert!(!decision.allow);
    assert!(
        !decision.reason.is_empty(),
        "denied in-flight ask should carry the policy reason",
    );

    let _ = fs::remove_dir_all(root);
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

#[test]
fn redact_json_payload_walks_nested_string_values_and_preserves_structure() {
    let redactor = squeezy_core::RedactionConfig::default()
        .redactor()
        .expect("built-in redactor");
    // The literal token values below are synthetic placeholders that the
    // built-in redactor patterns (`secret_assignment` and `aws_access_key`)
    // match shape-wise; they are intentionally chosen to not look like any
    // real provider's credential format so that repository-level secret
    // scanners (gitleaks etc.) don't see them as live secrets.
    let synthetic_assignment = "OPENAI_API_KEY=plaintestvaluedoesnotexist0001";
    let synthetic_aws_access_key = "AKIAEXAMPLEEXAMPLEXX";
    let payload = json!({
        "tool": "shell",
        "args": {
            "command": format!("echo {synthetic_assignment}"),
            "env_count": 7,
            "destructive": false,
        },
        "matches": [
            {"path": "src/main.rs", "snippet": synthetic_aws_access_key},
            {"path": "README.md", "snippet": "plain text"},
        ],
        "null_field": serde_json::Value::Null,
    });

    let redacted = redact_json_payload(payload, &redactor);

    // Shape must survive untouched: the function must not collapse on a
    // failed reparse the way the string-roundtrip version could.
    assert_eq!(redacted["tool"], "shell");
    assert_eq!(redacted["args"]["env_count"], 7);
    assert_eq!(redacted["args"]["destructive"], false);
    assert!(redacted["null_field"].is_null());
    assert_eq!(redacted["matches"][1]["path"], "README.md");

    // Secret-bearing string values are scrubbed in place, while non-secret
    // sibling strings (paths, keys) are left alone so downstream consumers
    // can still navigate the payload.
    let command = redacted["args"]["command"]
        .as_str()
        .expect("command remains a string");
    assert!(
        !command.contains("plaintestvaluedoesnotexist0001"),
        "{command}",
    );
    assert!(command.contains("echo"), "{command}");
    let snippet = redacted["matches"][0]["snippet"]
        .as_str()
        .expect("snippet remains a string");
    assert!(!snippet.contains(synthetic_aws_access_key), "{snippet}");
}

#[test]
fn tool_round_failure_summary_names_repeated_invalid_tool_arguments() {
    let results = vec![
        control_tool_result(
            &ToolCall {
                call_id: "call-1".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({}),
            },
            ToolStatus::Error,
            json!({"error": "invalid tool arguments: missing field `query`"}),
        ),
        control_tool_result(
            &ToolCall {
                call_id: "call-2".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({}),
            },
            ToolStatus::Error,
            json!({"error": "invalid tool arguments: missing field `query`"}),
        ),
    ];

    assert_eq!(
        tool_round_failure_summary(&results).as_deref(),
        Some("repeated invalid decl_search arguments (2x)")
    );
}

#[test]
fn tool_round_failure_summary_uses_shell_exit_details() {
    let results = vec![control_tool_result(
        &ToolCall {
            call_id: "call-1".to_string(),
            name: "shell".to_string(),
            arguments: json!({"command": "cargo check --bin sonar-arch-graph"}),
        },
        ToolStatus::Error,
        json!({
            "command": "cargo check --bin sonar-arch-graph",
            "exit_code": 101,
            "stdout": "",
            "stderr": "",
            "error": null,
        }),
    )];

    assert_eq!(
        tool_round_failure_summary(&results).as_deref(),
        Some("last shell failure: exit 101")
    );
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
            ..Default::default()
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

#[test]
fn subagent_registry_caps_concurrency() {
    let registry = SubagentRegistry::default();
    let cancel = CancellationToken::new();
    let mut leases = Vec::new();
    for slot in 0..SUBAGENT_MAX_CONCURRENT {
        let lease = registry
            .start(
                roles::SubagentRole::Explorer,
                cancel.child_token(),
                SUBAGENT_MAX_CONCURRENT,
                format!("slot {slot}"),
            )
            .expect("starting under the cap should succeed");
        leases.push(lease);
    }
    let err = registry
        .start(
            roles::SubagentRole::Explorer,
            cancel.child_token(),
            SUBAGENT_MAX_CONCURRENT,
            "overflow",
        )
        .expect_err("starting past the cap should fail");
    assert!(
        err.contains(&SUBAGENT_MAX_CONCURRENT.to_string()),
        "cap error should mention the limit: {err}"
    );
    drop(leases.pop());
    let lease = registry
        .start(
            roles::SubagentRole::Explorer,
            cancel.child_token(),
            SUBAGENT_MAX_CONCURRENT,
            "after drop",
        )
        .expect("dropping a lease frees a slot");
    drop(lease);
    drop(leases);
}

#[test]
fn core_control_tools_includes_new_delegate_planner_reviewer() {
    let config = SubagentConfig {
        enabled: true,
        explore_enabled: true,
        ..SubagentConfig::default()
    };
    let names: Vec<_> = core_control_tools(&config)
        .into_iter()
        .map(|tool| tool.spec.name)
        .collect();
    assert!(names.iter().any(|n| n == DELEGATE_TOOL_NAME));
    assert!(names.iter().any(|n| n == EXPLORE_TOOL_NAME));
    assert!(names.iter().any(|n| n == DELEGATE_PLAN_TOOL_NAME));
    assert!(names.iter().any(|n| n == DELEGATE_REVIEW_TOOL_NAME));
}

#[test]
fn core_control_tools_drops_all_when_subagents_disabled() {
    let config = SubagentConfig {
        enabled: false,
        ..SubagentConfig::default()
    };
    assert!(core_control_tools(&config).is_empty());
}

#[test]
fn subagent_kind_role_does_not_overlay_delegate() {
    // Delegate's broader research tool set is intentionally not constrained
    // by the Worker role (which is roadmap). Other kinds must map to an
    // active role overlay.
    assert!(SubagentKind::Delegate.role().is_none());
    assert_eq!(
        SubagentKind::Explore.role(),
        Some(roles::SubagentRole::Explorer)
    );
    assert_eq!(
        SubagentKind::Plan.role(),
        Some(roles::SubagentRole::Planner)
    );
    assert_eq!(
        SubagentKind::Review.role(),
        Some(roles::SubagentRole::Reviewer)
    );
}

#[test]
fn parse_subagent_request_requires_goal_for_plan_and_allows_empty_review() {
    let plan_call = ToolCall {
        call_id: "c1".to_string(),
        name: DELEGATE_PLAN_TOOL_NAME.to_string(),
        arguments: json!({}),
    };
    let err = parse_subagent_request(&plan_call, SubagentKind::Plan)
        .expect_err("plan without goal must error");
    assert!(err.contains("goal"), "error should mention goal: {err}");

    let plan_call = ToolCall {
        call_id: "c2".to_string(),
        name: DELEGATE_PLAN_TOOL_NAME.to_string(),
        arguments: json!({ "goal": "add tracing to ingest pipeline" }),
    };
    let request = parse_subagent_request(&plan_call, SubagentKind::Plan).expect("plan args valid");
    assert!(request.prompt.contains("ingest"));

    let review_call = ToolCall {
        call_id: "c3".to_string(),
        name: DELEGATE_REVIEW_TOOL_NAME.to_string(),
        arguments: json!({}),
    };
    let request =
        parse_subagent_request(&review_call, SubagentKind::Review).expect("review accepts empty");
    assert!(
        !request.prompt.is_empty(),
        "review should synthesize a default prompt"
    );
}
