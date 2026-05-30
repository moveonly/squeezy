use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
    AppConfig, CompactionStrategy, ContextAttachmentKind, ContextCompactionConfig,
    ContextCompactionState, ContextCompactionTrigger, CostSnapshot, PermissionAction,
    PermissionCapability, PermissionMode, PermissionPolicy, PermissionRequest, PermissionRisk,
    PermissionRuleSource, Result, SessionLogConfig, SessionMode, ShellSandboxMode, SkillsConfig,
    SubagentConfig, TaskStateStatus,
};
use squeezy_llm::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, LlmToolSpec,
    StopReason,
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
            stop_reason: None,
            reasoning_only_stop: false,
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
async fn cancel_preserves_partial_assistant_text_in_conversation_state() {
    // Streaming cancel: the model streams a few text deltas, then the
    // provider raises `LlmEvent::Cancelled` (the synthesised event the
    // turn runner emits when the cancel token tripped mid-stream). The
    // partial assistant text accumulated from `AgentEvent::AssistantDelta`
    // must land in `conversation_state.conversation` so the next turn's
    // prompt assembly can reference it, and a `cancelled = true`
    // transcript item must mirror it on the display/persistence path so
    // `(cancelled)` markers and `/diff`/`/undo` can see what was streamed
    // before the user pressed Esc. Wave-2-11 eval bug squeezy-3hr4.
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("1. Add tests\n".to_string())),
        Ok(LlmEvent::TextDelta("2. Wire fix\n".to_string())),
        Ok(LlmEvent::TextDelta("3. Land it\n".to_string())),
        Ok(LlmEvent::Cancelled),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("draft a plan".to_string(), CancellationToken::new());
    let mut saw_cancelled = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Cancelled { .. } => saw_cancelled = true,
            AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    assert!(
        saw_cancelled,
        "cancel branch must emit AgentEvent::Cancelled"
    );

    let state = agent.conversation_state.lock().await;
    let partial = "1. Add tests\n2. Wire fix\n3. Land it\n";

    // Conversation slice: the next turn's prompt assembly must see the
    // user message AND the partial assistant text. Without the fix the
    // assistant item is silently dropped (only the user item remains).
    let assistant_in_conversation = state
        .conversation
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::AssistantText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_in_conversation,
        vec![partial],
        "partial assistant text must be pushed onto `conversation_state.conversation` on cancel; \
         next turn's prompt build assembles the wire request from this slice"
    );

    // Transcript: the display/persistence side carries the partial text
    // tagged `cancelled = true` so renderers can append a `(cancelled)`
    // marker and `/undo`/`/diff` can find the cut-off turn. The transcript
    // path trims a trailing newline (renderers append `(cancelled)` after
    // the body) while the conversation slice keeps the raw stream — both
    // are correct for their consumer.
    let cancelled_assistant = state
        .transcript
        .iter()
        .find(|item| item.role == squeezy_core::Role::Assistant)
        .expect("transcript must record the cancelled assistant turn");
    assert_eq!(cancelled_assistant.content, partial.trim_end_matches('\n'));
    assert!(
        cancelled_assistant.cancelled,
        "cancelled assistant transcript item must carry the `cancelled` flag so renderers \
         can mark it (cancelled) and the next turn can reference it"
    );
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
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
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
        .flat_map(|request| {
            request
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>()
        })
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
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("recovered".to_string())),
            Ok(LlmEvent::Completed {
                response_id: None,
                cost: Default::default(),
                stop_reason: None,
                reasoning_only_stop: false,
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
            stop_reason: None,
            reasoning_only_stop: false,
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
        stop_reason: None,
        reasoning_only_stop: false,
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
        stop_reason: None,
        reasoning_only_stop: false,
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
async fn label_only_image_attachment_stays_unsupported() {
    // `.heic` extension with non-canonical bytes is recognised as
    // image-shaped by the label heuristic but its magic bytes never
    // surface a vision-routable MIME, so the attachment stays
    // `UnsupportedImage` and never reaches the next request.
    let root = temp_workspace("agent_context_image_heic");
    let image = root.join("snap.heic");
    fs::write(&image, b"not real heic content").expect("write image");
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let config = AppConfig {
        workspace_root: root.clone(),
        ..Default::default()
    };
    let agent = Agent::new(config, provider);

    let update = agent
        .attach_file_context(PathBuf::from("snap.heic"))
        .await
        .expect("attach image");

    assert!(!update.active);
    assert_eq!(
        update.attachment.kind,
        ContextAttachmentKind::UnsupportedImage
    );
    assert!(update.attachment.image_data_base64.is_none());
    assert!(agent.context_attachments_snapshot().await.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn png_file_attachment_routes_to_image_kind() {
    // PNG magic bytes flip the detection from the legacy
    // `UnsupportedImage` reject to the F18 `Image` kind so the file
    // attachment becomes active and carries the bytes through to the
    // next request.
    let root = temp_workspace("agent_context_image_png");
    let image = root.join("screenshot.png");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes.extend_from_slice(b"trailing png payload");
    fs::write(&image, &bytes).expect("write image");
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

    assert!(update.active);
    assert_eq!(update.attachment.kind, ContextAttachmentKind::Image);
    assert_eq!(
        update.attachment.image_media_type.as_deref(),
        Some("image/png")
    );
    assert!(update.attachment.image_data_base64.is_some());
    let snapshot = agent.context_attachments_snapshot().await;
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].kind, ContextAttachmentKind::Image);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn pasted_png_bytes_route_into_llm_image_input_item() {
    // F18: bytes that arrive through the paste path (`attach_pasted_bytes`)
    // and trip the PNG magic-byte sniff materialise as a
    // `LlmInputItem::Image` on the next request — same wire shape the
    // file-attached path produces, so every provider's multimodal
    // encoder sees a single uniform input regardless of whether the
    // image came from disk or the clipboard.
    let provider = Arc::new(MockProvider::new(vec![vec![Ok(LlmEvent::Completed {
        response_id: Some("resp_1".to_string()),
        cost: CostSnapshot::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })]]));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    png.extend_from_slice(b"clipboard-png-payload");
    let update = agent
        .attach_pasted_bytes(png.clone())
        .await
        .expect("attach pasted png");
    assert!(update.active);
    assert_eq!(update.attachment.kind, ContextAttachmentKind::Image);
    assert_eq!(
        update.attachment.image_media_type.as_deref(),
        Some("image/png")
    );

    let mut rx = agent.start_turn(
        "describe the clipboard image".to_string(),
        CancellationToken::new(),
    );
    while rx.recv().await.is_some() {}

    let requests = provider.requests();
    let input = &requests[0].input;
    // The user text leads the request, the image rides immediately
    // after — this is the order the per-provider encoders rely on to
    // coalesce text + image into one multimodal user turn.
    let LlmInputItem::UserText(text) = &input[0] else {
        panic!("expected user text item, got {:?}", input[0]);
    };
    assert!(text.contains("describe the clipboard image"));
    let image_item = input
        .iter()
        .find(|item| matches!(item, LlmInputItem::Image { .. }))
        .expect("request must carry a LlmInputItem::Image for the pasted PNG");
    let LlmInputItem::Image { media_type, bytes } = image_item else {
        unreachable!("matched on Image above");
    };
    assert_eq!(media_type, "image/png");
    assert_eq!(bytes.as_ref(), png.as_slice());
}

#[tokio::test]
async fn non_vision_model_rejects_pasted_image_with_clear_error() {
    // The provider boundary owns the vision gate: `ensure_vision_support`
    // runs as the first step of every provider's `stream_response`. We
    // build the same request shape `start_turn` would emit for a
    // pasted PNG and call the gate against a text-only provider/model
    // pair (deepseek-chat is registered as `vision: false`) to lock in
    // the rejection contract end-to-end — the agent's request fans
    // image bytes through, and a text-only model surfaces the
    // structured `ProviderRequest` error before any HTTP traffic.
    let provider = Arc::new(MockProvider::new(vec![vec![Ok(LlmEvent::Completed {
        response_id: Some("resp_1".to_string()),
        cost: CostSnapshot::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })]]));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    png.extend_from_slice(b"clipboard-png-payload");
    agent
        .attach_pasted_bytes(png)
        .await
        .expect("attach pasted png");

    let mut rx = agent.start_turn("describe it".to_string(), CancellationToken::new());
    while rx.recv().await.is_some() {}

    let request = provider.requests().into_iter().next().expect("one request");
    let err = request
        .ensure_vision_support("deepseek")
        .expect_err("non-vision model must reject image inputs");
    let message = err.to_string();
    assert!(
        message.contains("does not support image inputs"),
        "error must explain the rejection: got {message}"
    );
    assert!(
        message.contains("pick a vision-capable model"),
        "error must guide the user to a vision model: got {message}"
    );
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
            stop_reason: None,
            reasoning_only_stop: false,
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
            stop_reason: None,
            reasoning_only_stop: false,
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
                stop_reason: None,
                reasoning_only_stop: false,
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
                stop_reason: None,
                reasoning_only_stop: false,
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
                    "command": "echo job-ok",
                    "description": "print a marker",
                    "timeout_ms": 10_000,
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![Ok(LlmEvent::Completed {
            response_id: Some("resp_2".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
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

/// F04 coverage lock: a shell tool whose stdout contains a token-shaped
/// secret must NOT leak the raw bytes through any of the three downstream
/// surfaces — the transcript event (`AgentEvent::ToolCallCompleted.result`),
/// the persisted session log (`SessionStore::show.events` + replay tape),
/// or the next provider request (`LlmInputItem::FunctionCallOutput.output`
/// in `provider.requests()[1].input`). Each surface has its own call-site
/// pass through the `Redactor`, so this test pins all three at once.
#[tokio::test]
async fn shell_tool_output_secrets_are_redacted_across_transcript_session_log_and_provider_resend()
{
    const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz";

    let root = temp_workspace("agent_shell_output_secret_redaction");
    // Round 1 emits the shell tool call; round 2 lets the turn terminate
    // once the tool output has been re-sent to the provider. The second
    // request is what we inspect for the resend path.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command":
                        "echo 'leak ghp_abcdefghijklmnopqrstuvwxyz here'",
                    "description": "print a fake github token",
                    "timeout_ms": 10_000,
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![Ok(LlmEvent::Completed {
            response_id: Some("resp_2".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
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
    let agent = Agent::new(config, provider.clone());
    let session_id = agent.session_id().expect("session id");

    let mut rx = agent.start_turn("run shell".to_string(), CancellationToken::new());
    let mut shell_result = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolCallCompleted { result, .. } = event
            && result.tool_name == "shell"
        {
            shell_result = Some(result);
        }
    }

    let shell_result = shell_result.expect("shell ToolCallCompleted event must fire");
    assert_eq!(shell_result.status, ToolStatus::Success);

    // Path 1: transcript render. The TUI consumes `result.content` from
    // `AgentEvent::ToolCallCompleted`; that JSON must already be redacted.
    let transcript_payload =
        serde_json::to_string(&shell_result.content).expect("serialize tool result content");
    assert!(
        !transcript_payload.contains(SECRET),
        "transcript ToolCallCompleted result leaked the raw secret",
    );
    assert!(
        transcript_payload.contains("<redacted:github_token"),
        "transcript tool result must carry the github_token redaction marker",
    );
    assert!(
        shell_result.cost_hint.redactions > 0,
        "tool result cost hint must report at least one redaction",
    );

    // Path 2: provider resend. After the tool runs the agent loops back
    // and re-sends the result as a `FunctionCallOutput`. Pull that item
    // out of the recorded second-round request input.
    let requests = provider.requests();
    assert!(
        requests.len() >= 2,
        "provider must receive a second-round request that re-sends the tool output",
    );
    let resent_output = requests
        .iter()
        .flat_map(|request| request.input.iter().cloned())
        .find_map(|item| match item {
            LlmInputItem::FunctionCallOutput { output, .. } => Some(output),
            _ => None,
        })
        .expect("expected a FunctionCallOutput in provider request input");
    assert!(
        !resent_output.contains(SECRET),
        "FunctionCallOutput.output leaked the raw secret to the provider",
    );
    assert!(
        resent_output.contains("<redacted:github_token"),
        "FunctionCallOutput.output must carry the github_token redaction marker",
    );

    // Path 3: persisted session log + replay tape on disk. Both surfaces
    // are flushed by `show_session`; nothing in either may contain the
    // raw secret. Spot-check that the tool_result lifecycle event also
    // carries the redaction marker so a regression that drops the
    // log_session_event redactor pass would be caught here.
    let record = agent.show_session(&session_id).expect("session record");
    for event in &record.events {
        let payload_json =
            serde_json::to_string(&event.payload).expect("serialize session event payload");
        assert!(
            !payload_json.contains(SECRET),
            "session log event {} leaked the raw secret",
            event.kind,
        );
        if let Some(summary) = &event.summary {
            assert!(
                !summary.contains(SECRET),
                "session log event {} summary leaked the raw secret",
                event.kind,
            );
        }
    }
    let tool_result_event = record
        .events
        .iter()
        .find(|event| event.kind == "tool_result")
        .expect("session log must contain a tool_result event");
    assert!(
        serde_json::to_string(&tool_result_event.payload)
            .expect("serialize tool_result payload")
            .contains("<redacted:github_token"),
        "tool_result session event must carry the redaction marker",
    );
    if let Some(replay) = &record.replay {
        for event in &replay.events {
            let payload_json =
                serde_json::to_string(&event.payload).expect("serialize replay event payload");
            assert!(
                !payload_json.contains(SECRET),
                "session replay event {:?} leaked the raw secret",
                event.kind,
            );
        }
    }

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
            stop_reason: None,
            reasoning_only_stop: false,
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
async fn session_approval_installs_in_memory_rule_without_persisting() {
    let root = temp_workspace("agent_session_approval");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_1".to_string(),
                name: "write_file".to_string(),
                arguments: json!({"path": "sample.txt", "content": "hello"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
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
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested { decision_tx, .. } = event {
            decision_tx
                .send(ToolApprovalDecision::AllowSession)
                .expect("send decision");
        }
    }

    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "hello"
    );
    assert!(
        !root.join("squeezy.toml").exists(),
        "session approvals must not persist to project settings"
    );
    let session_rules = agent.session_rules_snapshot();
    assert_eq!(session_rules.len(), 1);
    assert_eq!(session_rules[0].source, PermissionRuleSource::Session);
    assert_eq!(session_rules[0].action, PermissionAction::Allow);
    assert_eq!(session_rules[0].target, "path:sample.txt");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ai_reviewer_allows_allowlisted_read_without_user_prompt() {
    let root = temp_workspace("agent_ai_reviewer_allow");
    fs::write(root.join("README.md"), "hello\n").expect("write readme");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                r#"{"action":"allow","reason":"read is in scope"}"#.to_string(),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("reviewer".to_string()),
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
    let mut config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            read: PermissionMode::Ask,
            ..Default::default()
        },
        ..Default::default()
    };
    config.permissions.ai_reviewer.enabled = true;
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("read the README".to_string(), CancellationToken::new());
    let mut approvals_seen = 0usize;
    let mut read_result = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ApprovalRequested { decision_tx, .. } => {
                approvals_seen += 1;
                let _ = decision_tx.send(ToolApprovalDecision::Denied);
            }
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "read_1" => {
                read_result = Some(result);
            }
            _ => {}
        }
    }

    assert_eq!(approvals_seen, 0);
    assert_eq!(
        read_result.expect("read result").status,
        ToolStatus::Success
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        matches!(&requests[1].input[0], LlmInputItem::UserText(text) if text.contains("Approval policy") && text.contains("\"tool_name\":\"read_file\"")),
        "reviewer prompt should carry policy and request: {:?}",
        requests[1].input
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ai_reviewer_denies_without_user_prompt() {
    let root = temp_workspace("agent_ai_reviewer_deny");
    fs::write(root.join("README.md"), "hello\n").expect("write readme");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                r#"{"action":"deny","reason":"too broad"}"#.to_string(),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("reviewer".to_string()),
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
    let mut config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            read: PermissionMode::Ask,
            ..Default::default()
        },
        ..Default::default()
    };
    config.permissions.ai_reviewer.enabled = true;
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("read everything".to_string(), CancellationToken::new());
    let mut approvals_seen = 0usize;
    let mut read_result = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ApprovalRequested { decision_tx, .. } => {
                approvals_seen += 1;
                let _ = decision_tx.send(ToolApprovalDecision::Denied);
            }
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "read_1" => {
                read_result = Some(result);
            }
            _ => {}
        }
    }

    assert_eq!(approvals_seen, 0);
    let read_result = read_result.expect("read result");
    assert_eq!(read_result.status, ToolStatus::Denied);
    assert!(
        read_result.content["error"]
            .as_str()
            .is_some_and(|error| error.contains("AI reviewer denied")),
        "{:?}",
        read_result.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ai_reviewer_allow_for_non_allowlisted_edit_escalates_to_user() {
    let root = temp_workspace("agent_ai_reviewer_edit_escalates");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "write_1".to_string(),
                name: "write_file".to_string(),
                arguments: json!({"path": "sample.txt", "content": "hello"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                r#"{"action":"allow","reason":"looks okay"}"#.to_string(),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("reviewer".to_string()),
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
    let mut config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            edit: PermissionMode::Ask,
            ..Default::default()
        },
        ..Default::default()
    };
    config.permissions.ai_reviewer.enabled = true;
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("write file".to_string(), CancellationToken::new());
    let mut approvals_seen = 0usize;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested { decision_tx, .. } = event {
            approvals_seen += 1;
            decision_tx
                .send(ToolApprovalDecision::Denied)
                .expect("send decision");
        }
    }

    assert_eq!(approvals_seen, 1);
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
            stop_reason: None,
            reasoning_only_stop: false,
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
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("edited".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
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
            stop_reason: None,
            reasoning_only_stop: false,
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
async fn known_help_topic_short_circuits_without_provider_request() {
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.start_turn("/help providers".to_string(), CancellationToken::new());
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { message, .. } = event {
            completed = Some(message.content);
        }
    }

    assert!(
        provider.requests().is_empty(),
        "known-topic /help should be answered locally without calling the provider"
    );
    let completed = completed.expect("help turn should complete");
    assert!(
        completed.contains("docs/external/PROVIDERS.md"),
        "{completed}"
    );
    assert!(completed.contains("[model]"), "{completed}");
}

#[tokio::test]
async fn unknown_help_topic_routes_to_doc_subagent_with_inlined_corpus() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(
            "Squeezy does not document a quantum-billing flow; see docs/external/CONFIGURATION.md."
                .to_string(),
        )),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.start_turn(
        "/help quantum billing rules".to_string(),
        CancellationToken::new(),
    );
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { message, .. } = event {
            completed = Some(message.content);
        }
    }

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        request
            .instructions
            .contains("hidden documentation subagent"),
        "{:?}",
        request.instructions
    );
    assert!(
        request.instructions.contains("inlined bundled doc corpus"),
        "{:?}",
        request.instructions
    );
    assert!(
        request.tools.is_empty(),
        "doc help subagent must have no tools: {:?}",
        request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    );
    let user_prompt = request
        .input
        .iter()
        .find_map(|item| match item {
            squeezy_llm::LlmInputItem::UserText(text) => Some(text.as_str()),
            _ => None,
        })
        .expect("subagent user prompt");
    assert!(
        user_prompt.contains("PATH: docs/external/PROVIDERS.md"),
        "subagent prompt must inline bundled docs: {user_prompt:?}"
    );

    let completed = completed.expect("help turn should complete");
    assert!(completed.contains("quantum-billing"), "{completed}");
    assert!(!completed.contains("won't guess"), "{completed}");
}

#[tokio::test]
async fn doc_help_subagent_gets_its_own_output_budget_not_summary_cap() {
    // Reasoning models burn many tokens before emitting visible content. The
    // DocHelp path *must not* inherit the tool-summary cap (sized for
    // Explore/Delegate synopses) — otherwise the OpenAI Responses API errors
    // with `response.incomplete: max_output_tokens` and the help turn falls
    // back to the unsupported message. See the comment on
    // `DEFAULT_DOC_HELP_MAX_OUTPUT_TOKENS`.
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("ok".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_doc_help_budget".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    // Construct a config where `max_summary_tokens` is deliberately tiny so
    // we can prove DocHelp does *not* read its budget from that knob. If
    // DocHelp ever silently regresses to the summary cap, this assertion
    // fires loudly.
    let mut config = AppConfig::default();
    config.subagents.max_summary_tokens = 800;
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn(
        "/help changing the model".to_string(),
        CancellationToken::new(),
    );
    while rx.recv().await.is_some() {}

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "exactly one doc-help subagent request");
    let request = &requests[0];
    let max_output = request
        .max_output_tokens
        .expect("doc-help subagent must set max_output_tokens");
    assert!(
        max_output >= squeezy_core::DEFAULT_DOC_HELP_MAX_OUTPUT_TOKENS,
        "doc-help budget {max_output} must be >= DOC_HELP floor {}",
        squeezy_core::DEFAULT_DOC_HELP_MAX_OUTPUT_TOKENS
    );
    assert_ne!(
        max_output, 800,
        "doc-help must not read its budget from subagents.max_summary_tokens",
    );
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
    let mut assistant_deltas = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::AssistantDelta { delta, .. } => assistant_deltas.push(delta),
            AgentEvent::ToolCallQueued { call, .. } => queued_tools.push(call),
            AgentEvent::ToolCallCompleted { result, .. } => tool_result = Some(result),
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            _ => {}
        }
    }

    assert!(provider.requests().is_empty());
    assert!(
        assistant_deltas.is_empty(),
        "local bang commands should not stream assistant text: {assistant_deltas:?}"
    );
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

#[test]
fn local_shell_command_parses_single_and_double_bang_prefixes() {
    let single = local_shell_command("!ls").expect("`!ls` parses as a local shell command");
    assert_eq!(single.command, "ls");
    assert!(
        !single.exclude_from_context,
        "single-bang must keep the exchange in LLM context",
    );

    let single_with_lead =
        local_shell_command("   !git status   ").expect("leading/trailing whitespace ok");
    assert_eq!(single_with_lead.command, "git status");
    assert!(!single_with_lead.exclude_from_context);

    let double = local_shell_command("!!git status").expect("`!!git status` is a quiet bang");
    assert_eq!(double.command, "git status");
    assert!(
        double.exclude_from_context,
        "double-bang must skip the LLM context",
    );

    let double_with_lead =
        local_shell_command("  !!  echo hi ").expect("leading/inner whitespace ok");
    assert_eq!(double_with_lead.command, "echo hi");
    assert!(double_with_lead.exclude_from_context);

    assert!(local_shell_command("ls").is_none(), "no bang prefix");
    assert!(local_shell_command("!").is_none(), "bare bang");
    assert!(local_shell_command("!!").is_none(), "bare double bang");
    assert!(
        local_shell_command("!!  ").is_none(),
        "double bang with no command",
    );
    assert!(
        local_shell_command("!ls\nrm -rf /").is_none(),
        "multi-line prompts are not local shell commands",
    );
}

#[tokio::test]
async fn double_bang_command_runs_locally_and_skips_llm_context() {
    let root = temp_workspace("agent_local_double_bang");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write cargo");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("acknowledged".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        },
        provider.clone(),
    );

    // Quiet bang still runs through the shell tool and renders in the TUI…
    let mut quiet_rx = agent.start_turn("!!ls".to_string(), CancellationToken::new());
    let mut quiet_tool_result = None;
    let mut quiet_completed = None;
    while let Some(event) = quiet_rx.recv().await {
        match event {
            AgentEvent::ToolCallCompleted { result, .. } => quiet_tool_result = Some(result),
            AgentEvent::Completed { message, .. } => quiet_completed = Some(message.content),
            _ => {}
        }
    }
    let quiet_tool_result =
        quiet_tool_result.expect("!!ls should still execute the shell tool locally");
    assert_eq!(quiet_tool_result.status, ToolStatus::Success);
    assert_eq!(
        quiet_tool_result.content["policy"]["direct_user_shell"], true,
        "double-bang must keep the direct-user-shell sandbox bypass",
    );
    assert!(
        quiet_completed
            .as_deref()
            .is_some_and(|text| text.contains("Cargo.toml")),
        "!!ls output must still surface in the TUI transcript: {quiet_completed:?}",
    );
    assert!(
        provider.requests().is_empty(),
        "!!ls must not trigger an LLM round itself",
    );

    // …but the next LLM turn must not replay the quiet exchange.
    let mut llm_rx = agent.start_turn("summarise".to_string(), CancellationToken::new());
    while llm_rx.recv().await.is_some() {}

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "follow-up turn should hit the provider");
    let input_texts: Vec<&str> = requests[0]
        .input
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::UserText(text) | LlmInputItem::AssistantText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        input_texts.contains(&"summarise"),
        "follow-up user prompt must reach the LLM: {input_texts:?}",
    );
    assert!(
        input_texts.iter().all(|text| !text.contains("!!ls")),
        "double-bang prompt must not appear in the LLM input: {input_texts:?}",
    );
    assert!(
        input_texts.iter().all(|text| !text.contains("Cargo.toml")),
        "double-bang shell output must not appear in the LLM input: {input_texts:?}",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn single_bang_command_records_exchange_in_llm_context() {
    let root = temp_workspace("agent_local_single_bang_context");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write cargo");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("ok".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        },
        provider.clone(),
    );

    let mut bang_rx = agent.start_turn("!ls".to_string(), CancellationToken::new());
    while bang_rx.recv().await.is_some() {}
    assert!(
        provider.requests().is_empty(),
        "single-bang itself does not call the model",
    );

    let mut next_rx = agent.start_turn("recap".to_string(), CancellationToken::new());
    while next_rx.recv().await.is_some() {}

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let input_texts: Vec<&str> = requests[0]
        .input
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::UserText(text) | LlmInputItem::AssistantText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        input_texts.contains(&"!ls"),
        "single-bang prompt must be replayed in the LLM input: {input_texts:?}",
    );
    assert!(
        input_texts.iter().any(|text| text.contains("Cargo.toml")),
        "single-bang shell output must be replayed in the LLM input: {input_texts:?}",
    );

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
            stop_reason: None,
            reasoning_only_stop: false,
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

#[test]
fn tool_loop_guard_distinguishes_shell_failures_by_command() {
    let make_shell = |call_id: &str, command: &str| {
        let call = ToolCall {
            call_id: call_id.to_string(),
            name: "shell".to_string(),
            arguments: json!({"command": command}),
        };
        let mut result = control_tool_result(
            &call,
            ToolStatus::Error,
            json!({
                "command": command,
                "exit_code": 101,
                "stderr": "",
                "stdout": "",
            }),
        );
        result.tool_name = "shell".to_string();
        (call, result)
    };

    let (call_a, result_a) = make_shell("shell-1", "cargo check -p sonar-arch-graph");
    let (call_b, result_b) = make_shell("shell-2", "cargo build --workspace");
    let mut guard = ToolLoopGuard::default();

    assert!(
        guard
            .observe_round(std::slice::from_ref(&call_a), &[result_a])
            .is_none()
    );
    assert!(
        guard
            .observe_round(std::slice::from_ref(&call_b), &[result_b])
            .is_none(),
        "different commands with same exit code must not be conflated"
    );

    let (call_c, result_c) = make_shell("shell-3", "cargo check -p sonar-arch-graph");
    let reason = guard
        .observe_round(&[call_c], &[result_c])
        .expect("genuine repeat of the same shell command should still stop");
    assert!(reason.contains("repeated shell failure"), "{reason}");
}

#[tokio::test]
async fn unsupported_squeezy_help_question_falls_back_after_doc_subagent_failure() {
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    // Use an explicit `/help <unknown-topic>` so the help interceptor takes the
    // turn (it always does for slash-commands) and the curated layer returns
    // `Unsupported`. The natural-language form is now intentionally routed to
    // the model when no curated topic matches.
    let mut rx = agent.start_turn(
        "/help quantum_billing".to_string(),
        CancellationToken::new(),
    );
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { message, .. } = event {
            completed = Some(message.content);
        }
    }

    assert!(
        !provider.requests().is_empty(),
        "help should try the doc subagent before falling back"
    );
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
            stop_reason: None,
            reasoning_only_stop: false,
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
            stop_reason: None,
            reasoning_only_stop: false,
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
        request.input.as_ref(),
        vec![LlmInputItem::UserText("inspect main".to_string())].as_slice()
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
            stop_reason: None,
            reasoning_only_stop: false,
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
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("ok".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
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
            stop_reason: None,
            reasoning_only_stop: false,
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
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![Ok(LlmEvent::Completed {
            response_id: Some("resp_2".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
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
        PermissionCapability::Shell,
        PermissionCapability::Git,
        PermissionCapability::Network,
        PermissionCapability::Mcp,
        PermissionCapability::Compiler,
        PermissionCapability::Destructive,
    ] {
        let request = permission_request_for_capability(capability);
        let verdict = mode_permission_verdict(SessionMode::Plan, &request, None)
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
fn plan_mode_denies_edit_with_no_active_plan() {
    let request = permission_request_for_capability(PermissionCapability::Edit);
    let verdict = mode_permission_verdict(SessionMode::Plan, &request, None)
        .expect("plan mode with no active plan should deny edits");
    assert_eq!(verdict.action, PermissionAction::Deny);
    assert!(
        verdict.reason.contains("no active plan file to edit"),
        "denial reason should reference missing plan; got: {}",
        verdict.reason
    );
}

#[test]
fn plan_mode_allows_edit_when_target_matches_active_plan() {
    let root = temp_workspace("plan_mode_edit_active");
    let plans_dir = root.join(super::plan_mode::PLAN_DIR);
    std::fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let active = plans_dir.join("plan-abc.md");
    std::fs::write(&active, "step 1\n").expect("write plan");

    // request.target is the active plan path → verdict must be None (allow).
    let mut request = permission_request_for_capability(PermissionCapability::Edit);
    request.target = active.display().to_string();
    assert_eq!(
        mode_permission_verdict(SessionMode::Plan, &request, Some(active.as_path())),
        None,
        "edit to active plan path must be allowed"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn plan_mode_denies_edit_when_target_is_unrelated_file() {
    let root = temp_workspace("plan_mode_edit_other");
    let plans_dir = root.join(super::plan_mode::PLAN_DIR);
    std::fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let active = plans_dir.join("plan-abc.md");
    std::fs::write(&active, "step 1\n").expect("write plan");
    let unrelated = root.join("src.rs");
    std::fs::write(&unrelated, "// some file\n").expect("write unrelated");

    let mut request = permission_request_for_capability(PermissionCapability::Edit);
    request.target = unrelated.display().to_string();
    let verdict = mode_permission_verdict(SessionMode::Plan, &request, Some(active.as_path()))
        .expect("plan mode must deny edits to unrelated files");
    assert_eq!(verdict.action, PermissionAction::Deny);
    assert!(
        verdict.reason.contains("only the active plan file"),
        "denial should explain plan-only scope; got: {}",
        verdict.reason
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn plan_mode_denies_edit_when_sibling_plan_file_targeted() {
    let root = temp_workspace("plan_mode_edit_sibling");
    let plans_dir = root.join(super::plan_mode::PLAN_DIR);
    std::fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let active = plans_dir.join("plan-current.md");
    let sibling = plans_dir.join("plan-old.md");
    std::fs::write(&active, "active\n").expect("write active");
    std::fs::write(&sibling, "old\n").expect("write sibling");

    let mut request = permission_request_for_capability(PermissionCapability::Edit);
    request.target = sibling.display().to_string();
    let verdict = mode_permission_verdict(SessionMode::Plan, &request, Some(active.as_path()))
        .expect("write exception is exact-match only — siblings must be denied");
    assert_eq!(verdict.action, PermissionAction::Deny);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn plan_mode_keeps_read_and_search_on_normal_policy_path() {
    for capability in [PermissionCapability::Read, PermissionCapability::Search] {
        let request = permission_request_for_capability(capability);
        assert_eq!(
            mode_permission_verdict(SessionMode::Plan, &request, None),
            None
        );
        assert_eq!(
            mode_permission_verdict(SessionMode::Build, &request, None),
            None
        );
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
        assert_eq!(
            mode_permission_verdict(SessionMode::Build, &request, None),
            None
        );
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

    let build_specs = advertised_tool_specs(&tools, SessionMode::Build, false);
    let build_names = advertised_tool_names(&build_specs);
    assert_eq!(
        build_names,
        tools
            .iter()
            .map(|tool| tool.spec.name.as_str())
            .collect::<Vec<_>>()
    );

    let plan_specs = advertised_tool_specs(&tools, SessionMode::Plan, false);
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
    let tools = core_control_tools(&SubagentConfig::default(), SessionMode::Build);

    let expected = vec![
        DELEGATE_TOOL_NAME,
        EXPLORE_TOOL_NAME,
        DELEGATE_PLAN_TOOL_NAME,
        DELEGATE_REVIEW_TOOL_NAME,
        DELEGATE_CHAIN_TOOL_NAME,
    ];

    let build_specs = advertised_tool_specs(&tools, SessionMode::Build, false);
    let build_names = advertised_tool_names(&build_specs);
    assert_eq!(build_names, expected);

    let plan_specs = advertised_tool_specs(&tools, SessionMode::Plan, false);
    let plan_names = advertised_tool_names(&plan_specs);
    assert_eq!(plan_names, expected);
}

#[test]
fn core_control_tools_filter_subagents_when_disabled() {
    let subagents = SubagentConfig {
        enabled: false,
        ..SubagentConfig::default()
    };
    let names: Vec<_> = core_control_tools(&subagents, SessionMode::Build)
        .into_iter()
        .map(|tool| tool.spec.name.clone())
        .collect();
    assert!(names.is_empty());

    let explore_only_off = SubagentConfig {
        explore_enabled: false,
        ..SubagentConfig::default()
    };
    let names: Vec<_> = core_control_tools(&explore_only_off, SessionMode::Build)
        .into_iter()
        .map(|tool| tool.spec.name.clone())
        .collect();
    assert_eq!(
        names,
        vec![
            DELEGATE_TOOL_NAME.to_string(),
            DELEGATE_PLAN_TOOL_NAME.to_string(),
            DELEGATE_REVIEW_TOOL_NAME.to_string(),
            DELEGATE_CHAIN_TOOL_NAME.to_string(),
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
    let mut tools = core_control_tools(&SubagentConfig::default(), SessionMode::Build);
    tools.extend([
        test_advertised_tool("grep", PermissionCapability::Search),
        test_advertised_tool("webfetch", PermissionCapability::Network),
        test_advertised_tool("mcp__docs__lookup", PermissionCapability::Mcp),
    ]);
    let config = ToolSchemaConfig::default();

    let initial_specs = request_tool_specs(&tools, SessionMode::Build, &config, &[], false);
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
        false,
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
    let mut tools = core_control_tools(&subagents, SessionMode::Build);
    tools.push(test_advertised_tool("grep", PermissionCapability::Search));
    let config = ToolSchemaConfig::default();

    let specs = request_tool_specs(&tools, SessionMode::Build, &config, &[], false);
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
    let mut tools = core_control_tools(&explore_only, SessionMode::Build);
    tools.push(test_advertised_tool("grep", PermissionCapability::Search));
    let specs = request_tool_specs(&tools, SessionMode::Build, &config, &[], false);
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

    let index = tool_schema_index(&tools, SessionMode::Build, &config, false).expect("index");

    assert!(index.contains("webfetch | capability=network"));
    assert!(index.contains("mcp__docs__lookup | capability=mcp"));
    assert!(!index.contains("grep | capability=search"));
}

#[test]
fn registry_specs_carry_capability_aligned_with_permission_request() {
    let root = temp_workspace("agent_registry_specs");
    let tools = ToolRegistry::new(&root).expect("registry");
    let specs = tools.specs();
    for spec in specs.iter() {
        let call = ToolCall {
            call_id: "probe".to_string(),
            name: spec.name.clone(),
            arguments: serde_json::json!({}),
        };
        let runtime_capability = tools.permission_request(&call).capability;
        let advertised = !mode_refuses_capability(SessionMode::Plan, spec.capability, false);
        let runtime = !mode_refuses_capability(SessionMode::Plan, runtime_capability, false);
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
        origin: ToolOrigin::Model,
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
        ai_reviewer_state: Arc::new(Mutex::new(ai_reviewer::AiReviewerState::default())),
        session_mode: Arc::new(AtomicU8::new(SessionMode::Build.to_u8())),
        session_log: None,
        conversation_state: None,
        task_state: Arc::new(tokio::sync::Mutex::new(None)),
        all_tool_specs: &advertised,
        loaded_tool_schemas: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        exploration_state: Arc::new(tokio::sync::Mutex::new(ExplorationTurnState::from_plan(
            None,
        ))),
        subagents: SubagentRegistry::default(),
        hooks: None,
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
        parallel_safe: false,
        parameters: squeezy_tools::parse_strict_tool_parameters(json!({"type": "object"}))
            .expect("typed tool schema"),
        prepare_arguments: None,
    })
}

fn advertised_tool_names(specs: &[Arc<LlmToolSpec>]) -> Vec<&str> {
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
            // Skill-activation tests below assert the full skill body
            // reaches the system prompt; opt into the inline render so
            // those assertions keep exercising the legacy path now that
            // metadata-only is the default.
            inline: true,
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
fn ingest_agents_md_walks_from_repo_root_to_cwd() {
    let root = temp_workspace("ingest_agents_md");
    fs::create_dir_all(root.join(".git")).expect("create .git marker");
    fs::write(root.join("AGENTS.md"), "root-level convention").expect("write root AGENTS.md");
    let nested = root.join("crates").join("alpha");
    fs::create_dir_all(&nested).expect("create nested dir");
    fs::write(nested.join("AGENTS.md"), "nested crate rule").expect("write nested AGENTS.md");
    let combined = super::ingest_agents_md(&nested, 16_384).expect("ingest");
    assert!(combined.contains("root-level convention"));
    assert!(combined.contains("nested crate rule"));
    let root_idx = combined
        .find("root-level convention")
        .expect("root present");
    let nested_idx = combined.find("nested crate rule").expect("nested present");
    assert!(root_idx < nested_idx, "root-first ordering: {combined:?}");
}

#[test]
fn ingest_agents_md_returns_none_when_absent() {
    let root = temp_workspace("ingest_agents_md_absent");
    fs::create_dir_all(root.join(".git")).expect("create .git marker");
    assert!(super::ingest_agents_md(&root, 16_384).is_none());
}

#[test]
fn ingest_agents_md_disabled_when_max_bytes_zero() {
    let root = temp_workspace("ingest_agents_md_disabled");
    fs::create_dir_all(root.join(".git")).expect("create .git marker");
    fs::write(root.join("AGENTS.md"), "ignored").expect("write");
    assert!(super::ingest_agents_md(&root, 0).is_none());
}

#[test]
fn ingest_agents_md_truncates_at_byte_cap() {
    let root = temp_workspace("ingest_agents_md_truncate");
    fs::create_dir_all(root.join(".git")).expect("create .git marker");
    let body = "x".repeat(1_000);
    fs::write(root.join("AGENTS.md"), &body).expect("write");
    let combined = super::ingest_agents_md(&root, 64).expect("ingest");
    assert!(combined.len() <= 64 + "\n[truncated]".len());
    assert!(combined.ends_with("[truncated]"));
}

#[test]
fn ingest_user_memory_reads_from_home_squeezy() {
    let home = temp_workspace("ingest_user_memory");
    fs::create_dir_all(home.join(".squeezy")).expect("mkdir .squeezy");
    fs::write(
        home.join(".squeezy").join("memory.md"),
        "user-level preference body",
    )
    .expect("write memory.md");
    let previous = std::env::var_os("HOME");
    // SAFETY: tests run single-threaded by default in this crate; the
    // surrounding nextest configuration also isolates env mutations.
    unsafe {
        std::env::set_var("HOME", &home);
    }
    let result = super::ingest_user_memory(8_192);
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    let body = result.expect("memory present");
    assert!(body.contains("user-level preference body"));
}

#[test]
fn ingest_user_memory_returns_none_when_missing() {
    let home = temp_workspace("ingest_user_memory_missing");
    let previous = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", &home);
    }
    let result = super::ingest_user_memory(8_192);
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    assert!(result.is_none());
}

#[test]
fn ingest_user_memory_reads_uppercase() {
    let home = temp_workspace("ingest_user_memory_uppercase_only");
    let dir = home.join(".squeezy");
    fs::create_dir_all(&dir).expect("mkdir .squeezy");
    fs::write(dir.join("MEMORY.md"), "uppercase-only body").expect("write MEMORY.md");
    let previous = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", &home);
    }
    let result = super::ingest_user_memory(8_192);
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    let body = result.expect("memory present");
    assert!(body.contains("uppercase-only body"));
}

#[tokio::test]
async fn agent_build_stitches_agents_md_into_instructions() {
    let root = temp_workspace("agent_build_agents_md");
    fs::create_dir_all(root.join(".git")).expect("create .git marker");
    let marker = "session-init AGENTS.md preamble marker";
    fs::write(root.join("AGENTS.md"), marker).expect("write AGENTS.md");
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        },
        provider,
    );
    let instructions = &agent.config().instructions;
    assert!(
        instructions.contains(marker),
        "AGENTS.md not stitched into base instructions: {instructions}"
    );
    assert!(
        instructions.contains("Project conventions from AGENTS.md"),
        "missing AGENTS.md preamble header: {instructions}"
    );
}

// AGENTS.md is workspace-level prose for the user-facing agent. A spawned
// subagent has its own narrow role instructions (`subagent_instructions`)
// and must not inherit the parent's AGENTS.md preamble — it would burn
// context on conventions the read-only research role can't act on. Regress
// the property that the subagent's LlmRequest.instructions is the per-kind
// briefing only, never the parent's stitched `config.instructions`.
#[tokio::test]
async fn subagent_request_instructions_omit_agents_md() {
    let root = temp_workspace("subagent_omits_agents_md");
    fs::create_dir_all(root.join(".git")).expect("create .git marker");
    let marker = "PARENT-ONLY-AGENTS-MD-SENTINEL";
    fs::write(root.join("AGENTS.md"), marker).expect("write AGENTS.md");
    let provider = Arc::new(MockProvider::new(vec![
        // Parent round 1: spawn a `delegate` subagent.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "del_omit_1".to_string(),
                name: "delegate".to_string(),
                arguments: json!({"prompt": "investigate something"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_parent_omit_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Subagent round 1: end immediately with a short text answer.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_sub_omit_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Parent round 2: close the turn after consuming the subagent result.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("noted".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_parent_omit_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        },
        provider.clone(),
    );
    assert!(
        agent.config().instructions.contains(marker),
        "precondition: parent must have AGENTS.md stitched in"
    );

    let mut rx = agent.start_turn("delegate then close".to_string(), CancellationToken::new());
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Completed { .. } | AgentEvent::Failed { .. } = event {
            break;
        }
    }

    let requests = provider.requests();
    assert!(
        requests.len() >= 2,
        "expected at least parent + subagent requests, got {}",
        requests.len()
    );
    // Subagent request is the second stream the provider serves. Its
    // instructions field must contain the per-kind delegate briefing and
    // must not carry the parent's AGENTS.md sentinel.
    let subagent_request = &requests[1];
    assert!(
        subagent_request
            .instructions
            .contains("isolated Squeezy research subagent"),
        "subagent request must use the per-kind delegate instructions: {:?}",
        subagent_request.instructions
    );
    assert!(
        !subagent_request.instructions.contains(marker),
        "subagent must not inherit AGENTS.md from parent.config.instructions: {:?}",
        subagent_request.instructions
    );
    assert!(
        !subagent_request
            .instructions
            .contains("Project conventions from AGENTS.md"),
        "subagent must not inherit the AGENTS.md preamble header: {:?}",
        subagent_request.instructions
    );
}

fn mid_turn_test_conversation() -> Vec<LlmInputItem> {
    let mut items = Vec::new();
    for n in 0..16 {
        items.push(LlmInputItem::UserText(format!(
            "user message {n} with a moderate amount of context to keep tokens nontrivial",
        )));
        items.push(LlmInputItem::AssistantText(format!(
            "assistant reply {n} also containing enough text to estimate as real tokens",
        )));
    }
    items
}

fn config_with_mid_turn(window: u64, threshold: u8) -> AppConfig {
    AppConfig {
        context_compaction: ContextCompactionConfig {
            enabled_mid_turn: true,
            model_context_window: Some(window),
            threshold_percent: threshold,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    }
}

#[test]
fn mid_turn_compaction_skips_when_disabled() {
    let mut config = config_with_mid_turn(100_000, 80);
    config.context_compaction.enabled_mid_turn = false;
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let report = super::maybe_compact_mid_turn(
        &mut conversation,
        &mut state,
        &[],
        None,
        &config,
        Some(90_000),
    );
    assert!(report.is_none());
}

#[test]
fn mid_turn_compaction_skips_without_window() {
    let mut config = config_with_mid_turn(100_000, 80);
    config.context_compaction.model_context_window = None;
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let report = super::maybe_compact_mid_turn(
        &mut conversation,
        &mut state,
        &[],
        None,
        &config,
        Some(90_000),
    );
    assert!(report.is_none());
}

#[test]
fn mid_turn_compaction_skips_below_threshold() {
    let config = config_with_mid_turn(100_000, 80);
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let report = super::maybe_compact_mid_turn(
        &mut conversation,
        &mut state,
        &[],
        None,
        &config,
        Some(50_000),
    );
    assert!(report.is_none());
}

#[test]
fn mid_turn_compaction_fires_at_threshold() {
    let config = config_with_mid_turn(100_000, 80);
    let mut conversation = mid_turn_test_conversation();
    let original_len = conversation.len();
    let mut state = ContextCompactionState::default();
    let report = super::maybe_compact_mid_turn(
        &mut conversation,
        &mut state,
        &[],
        None,
        &config,
        Some(80_001),
    )
    .expect("mid-turn compaction should fire");
    assert!(matches!(
        report.record.trigger,
        ContextCompactionTrigger::Auto
    ));
    assert!(
        conversation.len() < original_len,
        "conversation should shrink after compaction: {} -> {}",
        original_len,
        conversation.len(),
    );
    assert!(state.last.is_some(), "history should record the run");
}

#[tokio::test]
async fn mid_turn_compaction_fires_when_provider_reports_high_usage() {
    // End-to-end acceptance for F12-mid-turn-cw-aware-compaction: a real
    // turn loop with a provider that streams `usage.total = 80_001` on the
    // first response observes mid-turn compaction firing before the next
    // sample with `trigger=Auto`. Matches the audit acceptance literally.
    let root = temp_workspace("mid_turn_e2e");
    fs::write(root.join("sample.rs"), "fn marker() {}\n").expect("write sample");
    let provider = Arc::new(MockProvider::new(vec![
        // Turn-loop round 1: assistant calls `grep`, then `Completed` carries
        // a usage snapshot whose total (input + output + reasoning) crosses
        // the 80% threshold of a 100_000 window.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "marker", "include": ["*.rs"]}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(80_000),
                    output_tokens: Some(1),
                    reasoning_output_tokens: None,
                    cached_input_tokens: None,
                    cache_write_input_tokens: None,
                    estimated_usd_micros: None,
                },
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Turn-loop round 2: assistant finalizes with plain text after the
        // mid-turn compaction has rewritten the conversation.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let config = AppConfig {
        workspace_root: root.clone(),
        context_compaction: ContextCompactionConfig {
            enabled_mid_turn: true,
            model_context_window: Some(100_000),
            threshold_percent: 80,
            // Keep the function-call/output pair together in `recent` and
            // let the seed user message land in `older`. With `recent_items=1`
            // the snap-split absorbs the function-call output back into the
            // older slice and produces an empty split, so the compaction
            // never fires on a 3-item conversation.
            recent_items: 2,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("find marker".to_string(), CancellationToken::new());
    let mut compaction_report = None;
    let mut completed_message = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ContextCompacted { report, .. } => compaction_report = Some(report),
            AgentEvent::Completed { message, .. } => completed_message = Some(message.content),
            _ => {}
        }
    }

    let report = compaction_report.expect("mid-turn compaction should fire");
    assert!(
        matches!(report.record.trigger, ContextCompactionTrigger::Auto),
        "mid-turn trigger should be Auto, got {:?}",
        report.record.trigger,
    );
    assert!(
        report.record.before.estimated_tokens >= 80_000
            || report.record.before.estimated_tokens > 0,
        "before.estimated_tokens should reflect the pre-compaction estimate, got {}",
        report.record.before.estimated_tokens,
    );
    assert_eq!(completed_message.as_deref(), Some("done"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn total_tokens_from_cost_sums_present_fields() {
    let cost = CostSnapshot {
        input_tokens: Some(1_000),
        output_tokens: Some(2_000),
        reasoning_output_tokens: Some(500),
        ..CostSnapshot::default()
    };
    assert_eq!(super::total_tokens_from_cost(&cost), Some(3_500));
}

#[test]
fn total_tokens_from_cost_returns_none_when_no_fields() {
    let cost = CostSnapshot::default();
    assert!(super::total_tokens_from_cost(&cost).is_none());
}

#[test]
fn mid_turn_compaction_will_fire_matches_maybe_compact_mid_turn_gate() {
    let config = config_with_mid_turn(100_000, 80);
    let conversation = mid_turn_test_conversation();

    // Below the configured threshold, the predicate must report `false`
    // so the agent does not fire a `PreCompact` hook on a turn that
    // never reaches the rewrite path.
    assert!(!super::mid_turn_compaction_will_fire(
        &config,
        &conversation,
        Some(50_000),
    ));

    // At/above the threshold, the predicate must report `true` so the
    // hook fires before `maybe_compact_mid_turn` mutates conversation.
    assert!(super::mid_turn_compaction_will_fire(
        &config,
        &conversation,
        Some(80_001),
    ));

    // Mid-turn disabled disables the predicate too.
    let mut disabled = config.clone();
    disabled.context_compaction.enabled_mid_turn = false;
    assert!(!super::mid_turn_compaction_will_fire(
        &disabled,
        &conversation,
        Some(80_001),
    ));

    // Missing window short-circuits the predicate.
    let mut no_window = config;
    no_window.context_compaction.model_context_window = None;
    assert!(!super::mid_turn_compaction_will_fire(
        &no_window,
        &conversation,
        Some(80_001),
    ));
}

/// HookHandler that counts how many times each variant fires and
/// snapshots the last payload it saw. Drives the end-to-end test that
/// verifies the pre-turn compaction site dispatches `PreCompact` and
/// `PostCompact` with the documented `{ before_tokens, after_tokens }`
/// payload.
struct CompactionHookRecorder {
    pre_count: std::sync::atomic::AtomicUsize,
    post_count: std::sync::atomic::AtomicUsize,
    last_post_payload: std::sync::Mutex<Option<serde_json::Value>>,
}

impl CompactionHookRecorder {
    fn new() -> Self {
        Self {
            pre_count: std::sync::atomic::AtomicUsize::new(0),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            last_post_payload: std::sync::Mutex::new(None),
        }
    }

    fn pre(&self) -> usize {
        self.pre_count.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn post(&self) -> usize {
        self.post_count.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn last_post_payload(&self) -> Option<serde_json::Value> {
        self.last_post_payload.lock().unwrap().clone()
    }
}

struct CompactionHookRecorderRef(Arc<CompactionHookRecorder>);

impl squeezy_hooks::HookHandler for CompactionHookRecorderRef {
    fn handle(&self, ctx: &squeezy_hooks::HookContext) -> squeezy_hooks::HookResult {
        match ctx.event {
            squeezy_hooks::HookEvent::PreCompact => {
                self.0
                    .pre_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            squeezy_hooks::HookEvent::PostCompact => {
                self.0
                    .post_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *self.0.last_post_payload.lock().unwrap() = Some(ctx.payload_json());
            }
            _ => {}
        }
        squeezy_hooks::HookResult::allow()
    }
}

#[tokio::test]
async fn pre_turn_compaction_dispatches_pre_and_post_compact_hooks() {
    use squeezy_hooks::HookRegistry;

    // Two MockProvider responses: turn 1 grows the conversation past
    // the compaction floor, turn 2 trips the auto trigger because
    // estimated_tokens=0 + min_items=1 + recent_items=1 means a
    // conversation of three items will be split into [older..1] /
    // [recent..2] and compacted.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "first reply long enough to estimate as tokens. ".repeat(40),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("second reply".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));

    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            enabled: true,
            min_items: 1,
            recent_items: 1,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };

    let mut agent = Agent::new(config, provider);
    let recorder = Arc::new(CompactionHookRecorder::new());
    let mut registry = HookRegistry::new();
    registry.register(Box::new(CompactionHookRecorderRef(recorder.clone())));
    agent.set_hooks(Some(Arc::new(registry)));

    // Turn 1: drain to completion. After this, the persisted
    // conversation contains [UserText, AssistantText].
    let mut rx = agent.start_turn("seed turn".to_string(), CancellationToken::new());
    while let Some(_event) = rx.recv().await {}
    assert_eq!(
        recorder.pre(),
        0,
        "no compaction is possible on turn 1 because items (1) does not exceed keep (1)",
    );
    assert_eq!(recorder.post(), 0, "no PostCompact without a rewrite");

    // Turn 2: push the third item (the new UserText) — items=3 now
    // exceeds keep=1, so `maybe_compact_conversation` fires. PreCompact
    // must fire before the rewrite, PostCompact must fire after with the
    // before/after token counts in the payload.
    let mut rx = agent.start_turn("second turn".to_string(), CancellationToken::new());
    while let Some(_event) = rx.recv().await {}

    assert_eq!(
        recorder.pre(),
        1,
        "PreCompact must fire exactly once on the turn that compacts",
    );
    assert_eq!(
        recorder.post(),
        1,
        "PostCompact must fire exactly once on the turn that compacts",
    );

    let payload = recorder
        .last_post_payload()
        .expect("PostCompact carries a payload");
    let payload_obj = payload.as_object().expect("payload is an object");
    assert!(
        payload_obj.contains_key("before_tokens"),
        "PostCompact payload missing before_tokens: {payload}",
    );
    assert!(
        payload_obj.contains_key("after_tokens"),
        "PostCompact payload missing after_tokens: {payload}",
    );
    let before = payload_obj["before_tokens"].as_u64().expect("u64");
    let after = payload_obj["after_tokens"].as_u64().expect("u64");
    assert!(
        after <= before,
        "compaction should shrink or hold token count: before={before} after={after}",
    );
}

/// Stub `PreToolUse` handler that denies every call for a target tool
/// name and lets other tools through. Used by
/// `pretooluse_hook_denies_tool_call` to verify deny enforcement
/// without needing a per-test config file.
struct DenyToolByName {
    tool_name: &'static str,
    reason: &'static str,
}

impl squeezy_hooks::HookHandler for DenyToolByName {
    fn handle(&self, ctx: &squeezy_hooks::HookContext) -> squeezy_hooks::HookResult {
        if let squeezy_hooks::HookPayload::PreToolUse { tool_name, .. } = &ctx.payload
            && tool_name == self.tool_name
        {
            return squeezy_hooks::HookResult::deny(self.reason);
        }
        squeezy_hooks::HookResult::allow()
    }
}

#[tokio::test]
async fn pretooluse_hook_denies_tool_call() {
    use squeezy_hooks::HookRegistry;

    let root = temp_workspace("pretooluse_hook_denies_tool_call");
    fs::write(root.join("README.md"), "hello\n").expect("write readme");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
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
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            read: PermissionMode::Allow,
            ..Default::default()
        },
        ..AppConfig::default()
    };
    let mut agent = Agent::new(config, provider);
    let mut registry = HookRegistry::new();
    registry.register(Box::new(DenyToolByName {
        tool_name: "read_file",
        reason: "blocked by org policy",
    }));
    agent.set_hooks(Some(Arc::new(registry)));

    let mut rx = agent.start_turn("read the README".to_string(), CancellationToken::new());
    let mut approvals_seen = 0usize;
    let mut read_result = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ApprovalRequested { decision_tx, .. } => {
                approvals_seen += 1;
                let _ = decision_tx.send(ToolApprovalDecision::Denied);
            }
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "read_1" => {
                read_result = Some(result);
            }
            _ => {}
        }
    }

    assert_eq!(
        approvals_seen, 0,
        "PreToolUse deny must short-circuit before the permission engine asks the user",
    );
    let read_result = read_result.expect("ToolCallCompleted with read_1 must arrive");
    assert_eq!(read_result.status, ToolStatus::Denied);
    let reason = read_result.content["reason"]
        .as_str()
        .expect("denied result carries a reason string");
    assert_eq!(reason, "blocked by org policy");
    assert_eq!(
        read_result.content["permission_denied"],
        json!(true),
        "denied result must mark permission_denied for downstream consumers",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn pretooluse_hook_allow_lets_tool_run() {
    use squeezy_hooks::HookRegistry;

    let root = temp_workspace("pretooluse_hook_allow_lets_tool_run");
    fs::write(root.join("README.md"), "hello\n").expect("write readme");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
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
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            read: PermissionMode::Allow,
            ..Default::default()
        },
        ..AppConfig::default()
    };
    let mut agent = Agent::new(config, provider);
    // Handler denies a *different* tool, so the read_file call must run
    // to completion. This guards against the change short-circuiting
    // unrelated tool calls.
    let mut registry = HookRegistry::new();
    registry.register(Box::new(DenyToolByName {
        tool_name: "shell",
        reason: "shell blocked",
    }));
    agent.set_hooks(Some(Arc::new(registry)));

    let mut rx = agent.start_turn("read the README".to_string(), CancellationToken::new());
    let mut read_result = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolCallCompleted { result, .. } = event
            && result.call_id == "read_1"
        {
            read_result = Some(result);
        }
    }

    let read_result = read_result.expect("ToolCallCompleted with read_1 must arrive");
    assert_eq!(
        read_result.status,
        ToolStatus::Success,
        "deny aimed at another tool must not block read_file: {:?}",
        read_result.content,
    );

    let _ = fs::remove_dir_all(root);
}

/// Counts every `HookEvent` variant the registry observed. Used by
/// the expanded-dispatch tests below to assert each new call site
/// fires the documented variant without coupling to the per-variant
/// typed payload fields (those are exercised in `squeezy-hooks`).
struct EventCounter {
    counts: std::sync::Mutex<BTreeMap<squeezy_hooks::HookEvent, usize>>,
}

impl EventCounter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            counts: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    fn count(&self, event: squeezy_hooks::HookEvent) -> usize {
        self.counts
            .lock()
            .unwrap()
            .get(&event)
            .copied()
            .unwrap_or(0)
    }
}

struct EventCounterRef(Arc<EventCounter>);

impl squeezy_hooks::HookHandler for EventCounterRef {
    fn handle(&self, ctx: &squeezy_hooks::HookContext) -> squeezy_hooks::HookResult {
        *self.0.counts.lock().unwrap().entry(ctx.event).or_insert(0) += 1;
        squeezy_hooks::HookResult::allow()
    }
}

/// End-to-end check that a clean tool-less turn fans out the new
/// session-lifecycle hooks: `Setup` + `SessionStart` fire exactly
/// once on the first turn (after hooks installed via
/// [`Agent::set_hooks`]), `UserPromptSubmit` fires per turn, and
/// `Stop` fires once the turn yields back to the user. Guards
/// against future refactors regressing any of those four call sites
/// silently.
#[tokio::test]
async fn session_lifecycle_hooks_fire_around_clean_turn() {
    use squeezy_hooks::{HookEvent, HookRegistry};

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("hello".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("again".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));

    let mut agent = Agent::new(AppConfig::default(), provider);
    let counter = EventCounter::new();
    let mut registry = HookRegistry::new();
    registry.register(Box::new(EventCounterRef(counter.clone())));
    agent.set_hooks(Some(Arc::new(registry)));

    let mut rx = agent.start_turn("first turn".to_string(), CancellationToken::new());
    while let Some(_event) = rx.recv().await {}

    assert_eq!(
        counter.count(HookEvent::Setup),
        1,
        "Setup must fire exactly once on the first turn after hooks install",
    );
    assert_eq!(
        counter.count(HookEvent::SessionStart),
        1,
        "SessionStart must fire exactly once on the first turn",
    );
    assert_eq!(
        counter.count(HookEvent::UserPromptSubmit),
        1,
        "UserPromptSubmit must fire once per turn",
    );
    assert_eq!(
        counter.count(HookEvent::Stop),
        1,
        "Stop must fire once at the end of a clean turn",
    );

    let mut rx = agent.start_turn("second turn".to_string(), CancellationToken::new());
    while let Some(_event) = rx.recv().await {}

    assert_eq!(
        counter.count(HookEvent::Setup),
        1,
        "Setup must not fire again on subsequent turns",
    );
    assert_eq!(
        counter.count(HookEvent::SessionStart),
        1,
        "SessionStart must not fire again on subsequent turns",
    );
    assert_eq!(
        counter.count(HookEvent::UserPromptSubmit),
        2,
        "UserPromptSubmit must fire per turn",
    );
    assert_eq!(counter.count(HookEvent::Stop), 2, "Stop must fire per turn",);
}

/// Verifies the post-tool dispatch sites: every tool call produces
/// one `PostToolUse` and one `PostTool` event, and a failed tool
/// status additionally fires `PostToolUseFailure` while leaving the
/// "result was appended to the conversation" `PostTool` semantics
/// untouched. The provider drives one happy-path read and a
/// guaranteed failure (path that does not exist) so the same
/// registry observes both shapes inside a single turn.
#[tokio::test]
async fn post_tool_and_failure_hooks_split_success_and_failure_paths() {
    use squeezy_hooks::{HookEvent, HookRegistry};

    let root = temp_workspace("post_tool_and_failure_hooks");
    fs::write(root.join("README.md"), "hi\n").expect("write readme");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "ok_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({ "path": "README.md" }),
            })),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "fail_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({ "path": "does-not-exist.txt" }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
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
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            read: PermissionMode::Allow,
            ..Default::default()
        },
        ..AppConfig::default()
    };
    let mut agent = Agent::new(config, provider);
    let counter = EventCounter::new();
    let mut registry = HookRegistry::new();
    registry.register(Box::new(EventCounterRef(counter.clone())));
    agent.set_hooks(Some(Arc::new(registry)));

    let mut rx = agent.start_turn(
        "read README and a missing file".to_string(),
        CancellationToken::new(),
    );
    while let Some(_event) = rx.recv().await {}

    assert_eq!(
        counter.count(HookEvent::PostToolUse),
        2,
        "PostToolUse must fire for every tool call regardless of status",
    );
    assert_eq!(
        counter.count(HookEvent::PostTool),
        2,
        "PostTool must fire once per FunctionCallOutput appended to the conversation",
    );
    assert_eq!(
        counter.count(HookEvent::PostToolUseFailure),
        1,
        "PostToolUseFailure must fire exactly once, for the failed tool call",
    );

    let _ = fs::remove_dir_all(root);
}

/// `PermissionRequest` must fire on every permission evaluation, and
/// `PermissionDenied` must fire whenever the eventual decision is a
/// deny. We deny via a deny-by-default permission policy so the
/// evaluator short-circuits to `ApprovalDecision::Denied` without
/// going through the user-approval round-trip.
#[tokio::test]
async fn permission_request_and_denied_hooks_fire_on_policy_deny() {
    use squeezy_hooks::{HookEvent, HookRegistry};

    let root = temp_workspace("permission_request_denied_hooks");
    fs::write(root.join("README.md"), "hi\n").expect("write readme");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "read_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({ "path": "README.md" }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
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
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            read: PermissionMode::Deny,
            ..Default::default()
        },
        ..AppConfig::default()
    };
    let mut agent = Agent::new(config, provider);
    let counter = EventCounter::new();
    let mut registry = HookRegistry::new();
    registry.register(Box::new(EventCounterRef(counter.clone())));
    agent.set_hooks(Some(Arc::new(registry)));

    let mut rx = agent.start_turn("read the README".to_string(), CancellationToken::new());
    while let Some(_event) = rx.recv().await {}

    assert!(
        counter.count(HookEvent::PermissionRequest) >= 1,
        "PermissionRequest must fire at least once before the policy evaluation runs",
    );
    assert!(
        counter.count(HookEvent::PermissionDenied) >= 1,
        "PermissionDenied must fire when the policy resolves the request as a deny",
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn compaction_strategy_default_is_extractive() {
    assert_eq!(
        CompactionStrategy::default(),
        CompactionStrategy::Extractive
    );
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
    assert_eq!(err.reason, SubagentRejectionReason::ConcurrencyCap);
    assert_eq!(err.limit, SUBAGENT_MAX_CONCURRENT);
    assert_eq!(err.active, SUBAGENT_MAX_CONCURRENT);
    let message = err.as_message();
    assert!(
        message.contains(&SUBAGENT_MAX_CONCURRENT.to_string()),
        "cap error should mention the limit: {message}"
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
    let names: Vec<_> = core_control_tools(&config, SessionMode::Build)
        .into_iter()
        .map(|tool| tool.spec.name.clone())
        .collect();
    assert!(names.iter().any(|n| n == DELEGATE_TOOL_NAME));
    assert!(names.iter().any(|n| n == EXPLORE_TOOL_NAME));
    assert!(names.iter().any(|n| n == DELEGATE_PLAN_TOOL_NAME));
    assert!(names.iter().any(|n| n == DELEGATE_REVIEW_TOOL_NAME));
    assert!(names.iter().any(|n| n == DELEGATE_CHAIN_TOOL_NAME));
}

#[test]
fn core_control_tools_drops_all_when_subagents_disabled() {
    let config = SubagentConfig {
        enabled: false,
        ..SubagentConfig::default()
    };
    assert!(core_control_tools(&config, SessionMode::Build).is_empty());
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

/// Parent tool advertisement that mixes typical read/search tools with a
/// representative mutating tool for every non-read capability. The mutating
/// names cover the audit's named risks (`write_file`, `shell`, `apply_patch`)
/// plus one per remaining `PermissionCapability` variant so a new capability
/// added later cannot leak in unnoticed.
fn parent_tools_with_mutators() -> Vec<AdvertisedTool> {
    vec![
        // Read/search tools every subagent role allow-list mentions.
        test_advertised_tool("read_file", PermissionCapability::Read),
        test_advertised_tool("read_slice", PermissionCapability::Read),
        test_advertised_tool("grep", PermissionCapability::Search),
        test_advertised_tool("glob", PermissionCapability::Search),
        test_advertised_tool("repo_map", PermissionCapability::Search),
        test_advertised_tool("decl_search", PermissionCapability::Search),
        test_advertised_tool("definition_search", PermissionCapability::Search),
        test_advertised_tool("reference_search", PermissionCapability::Search),
        test_advertised_tool("hierarchy", PermissionCapability::Search),
        test_advertised_tool("symbol_context", PermissionCapability::Search),
        test_advertised_tool("upstream_flow", PermissionCapability::Search),
        test_advertised_tool("downstream_flow", PermissionCapability::Search),
        test_advertised_tool("diff_context", PermissionCapability::Read),
        // Mutating tools the audit calls out by name plus one per remaining
        // non-read capability.
        test_advertised_tool("write_file", PermissionCapability::Edit),
        test_advertised_tool("apply_patch", PermissionCapability::Edit),
        test_advertised_tool("shell", PermissionCapability::Shell),
        test_advertised_tool("webfetch", PermissionCapability::Network),
        test_advertised_tool("mcp__remote__call", PermissionCapability::Mcp),
        test_advertised_tool("git_commit", PermissionCapability::Git),
        test_advertised_tool("cargo_build", PermissionCapability::Compiler),
        test_advertised_tool("rm_rf", PermissionCapability::Destructive),
    ]
}

#[test]
fn explore_subagent_cannot_call_write_file() {
    // F10-typed-subagent-permission-derivation: the explorer
    // role is read-only by construction. `subagent_allowed_tools` filters the
    // parent tool advertisement down to Read|Search capability, so even when
    // the parent advertises `write_file`, `apply_patch`, and `shell`, the
    // explore subagent must never see them.
    let parent_tools = parent_tools_with_mutators();
    let allowed = subagent_allowed_tools(&parent_tools, SubagentKind::Explore);
    let allowed_names: BTreeSet<&str> =
        allowed.iter().map(|tool| tool.spec.name.as_str()).collect();

    assert!(
        !allowed_names.contains("write_file"),
        "explore subagent must not see write_file: {allowed_names:?}"
    );
    assert!(
        !allowed_names.contains("apply_patch"),
        "explore subagent must not see apply_patch: {allowed_names:?}"
    );
    assert!(
        !allowed_names.contains("shell"),
        "explore subagent must not see shell: {allowed_names:?}"
    );
    // Read/search tools the explorer needs must survive the filter.
    assert!(
        allowed_names.contains("read_file"),
        "explore subagent should keep read_file: {allowed_names:?}"
    );
    assert!(
        allowed_names.contains("grep"),
        "explore subagent should keep grep: {allowed_names:?}"
    );
}

#[test]
fn typed_subagents_filter_to_read_search_capability() {
    // The capability filter is the load-bearing safety guarantee. Iterate
    // every non-DocHelp role kind and assert that no tool of a capability
    // outside `{Read, Search}` survives the filter. Captures explorer,
    // planner, and reviewer — extending to any future typed subagent kind
    // requires updating this matrix.
    let parent_tools = parent_tools_with_mutators();
    for kind in [
        SubagentKind::Delegate,
        SubagentKind::Explore,
        SubagentKind::Plan,
        SubagentKind::Review,
    ] {
        let allowed = subagent_allowed_tools(&parent_tools, kind);
        for tool in &allowed {
            assert!(
                matches!(
                    tool.capability,
                    PermissionCapability::Read | PermissionCapability::Search
                ),
                "subagent kind {:?} leaked non-read/search tool {:?} with capability {:?}",
                kind,
                tool.spec.name,
                tool.capability
            );
        }
    }
}

#[test]
fn reviewer_subagent_cannot_call_apply_patch_or_shell() {
    // F10-typed-subagent-permission-derivation: the reviewer
    // role reviews diffs and must not be able to mutate the working tree or
    // run shell commands, even if the parent advertisement contains those
    // tools.
    let parent_tools = parent_tools_with_mutators();
    let allowed = subagent_allowed_tools(&parent_tools, SubagentKind::Review);
    let allowed_names: BTreeSet<&str> =
        allowed.iter().map(|tool| tool.spec.name.as_str()).collect();

    assert!(
        !allowed_names.contains("write_file"),
        "reviewer subagent must not see write_file: {allowed_names:?}"
    );
    assert!(
        !allowed_names.contains("apply_patch"),
        "reviewer subagent must not see apply_patch: {allowed_names:?}"
    );
    assert!(
        !allowed_names.contains("shell"),
        "reviewer subagent must not see shell: {allowed_names:?}"
    );
    assert!(
        allowed_names.contains("diff_context"),
        "reviewer subagent should keep diff_context: {allowed_names:?}"
    );
}

#[test]
fn compaction_strategy_parse_round_trip() {
    for variant in [
        CompactionStrategy::Extractive,
        CompactionStrategy::ModelAssisted,
        CompactionStrategy::LayeredFallback,
    ] {
        assert_eq!(CompactionStrategy::parse(variant.as_str()), Some(variant));
    }
}

/// Provider that scripts the parent's first round (a `delegate` tool call),
/// scripts the subagent's first round (a rejected tool call + cost), and then
/// returns a pending stream for every subsequent stream — so the subagent's
/// second round hangs and the wall-clock timeout fires.
struct SubagentTimeoutProvider {
    calls: Mutex<usize>,
}

impl SubagentTimeoutProvider {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }
}

impl LlmProvider for SubagentTimeoutProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let mut calls = self.calls.lock().expect("calls");
        *calls += 1;
        let n = *calls;
        drop(calls);
        match n {
            1 => {
                let events = vec![
                    Ok(LlmEvent::Started),
                    Ok(LlmEvent::ToolCall(LlmToolCall {
                        call_id: "del_1".to_string(),
                        name: "delegate".to_string(),
                        arguments: json!({"prompt": "hang please"}),
                    })),
                    Ok(LlmEvent::Completed {
                        response_id: Some("resp_parent_1".to_string()),
                        cost: CostSnapshot::default(),
                        stop_reason: None,
                        reasoning_only_stop: false,
                    }),
                ];
                let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
                    Box::pin(stream::iter(events));
                stream
            }
            2 => {
                // Subagent's first round: drive a tool call we will reject so
                // the loop continues past the empty-tool-calls fast path, and
                // ship a non-zero cost so partial metrics are observable when
                // the wall-clock timer fires in the next round.
                let events = vec![
                    Ok(LlmEvent::Started),
                    Ok(LlmEvent::ToolCall(LlmToolCall {
                        call_id: "sub_1".to_string(),
                        name: "definitely_not_a_real_tool".to_string(),
                        arguments: json!({}),
                    })),
                    Ok(LlmEvent::Completed {
                        response_id: Some("resp_sub_1".to_string()),
                        cost: CostSnapshot {
                            input_tokens: Some(42),
                            output_tokens: Some(7),
                            estimated_usd_micros: Some(1_234),
                            ..CostSnapshot::default()
                        },
                        stop_reason: None,
                        reasoning_only_stop: false,
                    }),
                ];
                let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
                    Box::pin(stream::iter(events));
                stream
            }
            _ => Box::pin(stream::pending()),
        }
    }
}

#[tokio::test]
async fn subagent_wall_clock_timeout_terminates_with_partial_metrics() {
    let provider = Arc::new(SubagentTimeoutProvider::new());
    let mut config = AppConfig::default();
    // 1s budget keeps the test fast while staying well above scheduler noise.
    config.subagents.max_runtime_secs = Some(1);
    let agent = Agent::new(config, provider.clone());

    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    let mut rx = agent.start_turn("delegate to the void".to_string(), cancel.clone());
    let mut saw_timeout = false;
    let mut subagent_metrics = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::SubagentFailed {
            agent,
            error,
            metrics,
            ..
        } = event
        {
            assert_eq!(agent, "delegate");
            assert!(
                error.contains("wall-clock") || error.contains("timed_out"),
                "timeout error should be self-describing: {error}"
            );
            subagent_metrics = Some(metrics);
            saw_timeout = true;
            cancel.cancel();
            break;
        }
    }
    let elapsed = started.elapsed();
    // Drain any tail events without blocking the test on the parent's
    // post-cancel teardown.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while rx.recv().await.is_some() {}
    })
    .await;

    assert!(
        saw_timeout,
        "expected SubagentFailed for the timed-out delegate"
    );
    assert!(
        elapsed < Duration::from_millis(2_500),
        "timeout fired in {elapsed:?}, expected <2.5s"
    );
    let metrics = subagent_metrics.expect("metrics captured");
    // Partial cost from round 1 must survive the timeout. If we accidentally
    // returned `TurnMetrics::default()`, every counter below would be zero.
    // `SubagentFailed.metrics` carries the subagent's own TurnMetrics, so
    // the spent tokens land on `provider` (the parent later folds them into
    // its own `subagent_provider` via `merge_subagent_tool_metrics`).
    assert_eq!(
        metrics.provider.input_tokens,
        Some(42),
        "partial provider cost dropped on timeout: {metrics:?}"
    );
    assert!(
        metrics.model_output_bytes > 0,
        "model_output_bytes from the rejected tool result should be preserved: {metrics:?}"
    );
}

/// Scripts a single `delegate` tool call so the parent loop reaches
/// `handle_subagent_call`. The cap rejection short-circuits before the
/// subagent runs, so a second parent round is needed to consume the
/// rejection tool result and finish the turn.
struct OneDelegateProvider {
    calls: Mutex<usize>,
}

impl OneDelegateProvider {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }
}

impl LlmProvider for OneDelegateProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let mut calls = self.calls.lock().expect("calls");
        *calls += 1;
        let n = *calls;
        drop(calls);
        let events = match n {
            1 => vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "del_capped".to_string(),
                    name: "delegate".to_string(),
                    arguments: json!({"prompt": "please help"}),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("parent_tools".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ],
            _ => vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("noted".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("parent_final".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ],
        };
        let stream: Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>> =
            Box::pin(stream::iter(events));
        stream
    }
}

#[tokio::test]
async fn subagent_concurrency_cap_emits_rejected_event() {
    let provider = Arc::new(OneDelegateProvider::new());
    let agent = Agent::new(AppConfig::default(), provider.clone());

    // Saturate the registry from outside the turn so the first delegate
    // attempt hits the cap synchronously. Leases live until the end of
    // the test, mirroring how a real parent would have N in-flight peers.
    let registry = agent.subagent_registry_for_test();
    let cancel = CancellationToken::new();
    let mut leases = Vec::new();
    for slot in 0..SUBAGENT_MAX_CONCURRENT {
        leases.push(
            registry
                .start(
                    roles::SubagentRole::Explorer,
                    cancel.child_token(),
                    SUBAGENT_MAX_CONCURRENT,
                    format!("pre-saturate {slot}"),
                )
                .expect("under-cap start"),
        );
    }

    let mut rx = agent.start_turn("delegate now".to_string(), cancel.clone());
    let mut rejection: Option<(String, SubagentRejectionReason, usize, usize)> = None;
    let mut saw_started = false;
    let mut saw_failed = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::SubagentRejected {
                agent,
                reason,
                limit,
                active,
                ..
            } => {
                rejection = Some((agent, reason, limit, active));
            }
            AgentEvent::SubagentStarted { .. } => saw_started = true,
            AgentEvent::SubagentFailed { .. } => saw_failed = true,
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. } => break,
            _ => {}
        }
    }
    drop(leases);

    let (agent_name, reason, limit, active) =
        rejection.expect("expected a SubagentRejected event when the cap is full");
    assert_eq!(agent_name, "delegate");
    assert_eq!(reason, SubagentRejectionReason::ConcurrencyCap);
    assert_eq!(limit, SUBAGENT_MAX_CONCURRENT);
    assert_eq!(active, SUBAGENT_MAX_CONCURRENT);
    assert!(
        !saw_started,
        "SubagentStarted must not fire when the registry refuses the lease"
    );
    assert!(
        !saw_failed,
        "rejection must use SubagentRejected, not SubagentFailed"
    );
}

#[tokio::test]
async fn compact_with_strategy_falls_back_to_extractive_when_hanging_provider_times_out() {
    use squeezy_core::Redactor;
    use std::sync::Arc;

    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            strategy: CompactionStrategy::ModelAssisted,
            model_assisted_model: Some("test-model".to_string()),
            model_assisted_timeout_secs: 1,
            recent_items: 2,
            min_items: 4,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let provider: Arc<dyn LlmProvider> = Arc::new(HangingProvider::new());
    let redactor = Arc::new(Redactor::default());
    let report = super::compact_conversation_with_strategy(
        &mut conversation,
        &mut state,
        &[],
        None,
        &provider,
        None,
        &redactor,
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .await
    .expect("compaction should fire even when model assist times out");
    assert!(
        report
            .summary
            .contains("Squeezy compacted conversation context"),
        "summary should fall back to extractive output, got: {}",
        report.summary,
    );
}

#[test]
fn compaction_summary_includes_recent_observations() {
    use squeezy_store::{Observation, ObservationKind, SqueezyStore};

    let root = temp_workspace("compaction_observations");
    let store = SqueezyStore::open(&root, None).expect("open store");
    store
        .put_observation(Observation::new(
            ObservationKind::Decision,
            "Always batch redb writes inside a single transaction.",
            "test-suite",
        ))
        .expect("put observation");

    let config = AppConfig::default();
    let summary = super::build_compaction_summary(
        1,
        &ContextCompactionState::default(),
        &[],
        &[],
        Some(&store),
        &config,
    );
    assert!(
        summary.contains("Prior decisions and notes"),
        "summary should mention notes_recall block: {summary}",
    );
    assert!(
        summary.contains("Always batch redb writes"),
        "summary should include the stored decision text: {summary}",
    );
}

#[test]
fn compaction_persists_checkpoint_and_stamps_replacement_id() {
    use squeezy_store::SqueezyStore;

    let root = temp_workspace("compact_checkpoint");
    let store = SqueezyStore::open(&root, None).expect("open store");
    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            recent_items: 2,
            min_items: 4,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut conversation = mid_turn_test_conversation();
    let original_len = conversation.len();
    let mut state = ContextCompactionState::default();
    let report = super::compact_conversation(
        &mut conversation,
        &mut state,
        &[],
        Some(&store),
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .expect("compaction");
    assert!(conversation.len() < original_len);
    let replacement_id = report
        .record
        .replacement_id
        .clone()
        .expect("replacement_id stamped");
    let checkpoint = store
        .get_compaction_checkpoint(&replacement_id)
        .expect("get checkpoint")
        .expect("checkpoint present");
    assert_eq!(checkpoint.items.len(), report.record.dropped_items);
}

#[test]
fn compaction_without_store_leaves_replacement_id_none() {
    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            recent_items: 2,
            min_items: 4,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let report = super::compact_conversation(
        &mut conversation,
        &mut state,
        &[],
        None,
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .expect("compaction");
    assert!(report.record.replacement_id.is_none());
}

#[test]
fn compaction_drops_orphan_function_call_outputs_from_interleaved_parallel_calls() {
    // Parallel tool calls produce `[FC(A), FC(B), FCO(A), FCO(B)]`. If the
    // compaction split lands between the two calls, recent starts with
    // `FC(B)` and snap_compaction_split stops there — leaving `FCO(A)` as
    // an orphan in the recent slice whose declaring `FC(A)` was dropped
    // into `older`. The next provider request would then carry a bare
    // `function_call_output` and OpenAI would 400 the turn.
    let mut conversation = vec![
        LlmInputItem::UserText("seed user message".to_string()),
        LlmInputItem::AssistantText("seed assistant reply".to_string()),
        LlmInputItem::FunctionCall {
            call_id: "call_A".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({"pattern": "foo"}),
        },
        LlmInputItem::FunctionCall {
            call_id: "call_B".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({"pattern": "bar"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_A".to_string(),
            output: "foo result".to_string(),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_B".to_string(),
            output: "bar result".to_string(),
        },
        LlmInputItem::AssistantText("post-tools reply".to_string()),
    ];
    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            recent_items: 4,
            min_items: 1,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut state = ContextCompactionState::default();
    let report = super::compact_conversation(
        &mut conversation,
        &mut state,
        &[],
        None,
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .expect("compaction should run");

    let kept_call_ids: std::collections::BTreeSet<&str> = conversation
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    for item in &conversation {
        if let LlmInputItem::FunctionCallOutput { call_id, .. } = item {
            assert!(
                kept_call_ids.contains(call_id.as_str()),
                "orphan function_call_output survived compaction: call_id={call_id} \
                 conversation={conversation:?}"
            );
        }
    }
    // The summary itself should land at the head.
    assert!(matches!(
        conversation.first(),
        Some(LlmInputItem::UserText(_))
    ));
    assert!(report.record.dropped_items >= 3);
}

#[test]
fn mark_intra_batch_duplicates_stamps_hint_on_second_identical_call() {
    let root = temp_workspace("agent_duplicate_tools");
    let tools = ToolRegistry::new(&root).expect("registry");
    let make_call = |call_id: &str, pattern: &str| ToolCall {
        call_id: call_id.to_string(),
        name: "grep".to_string(),
        arguments: serde_json::json!({"pattern": pattern}),
    };
    let make_result = |call: &ToolCall| ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.name.clone(),
        status: ToolStatus::Success,
        content: serde_json::json!({"matches": []}),
        cost_hint: ToolCostHint::default(),
        receipt: squeezy_tools::ToolReceipt {
            output_sha256: String::new(),
            content_sha256: None,
        },
        spill_model_output: None,
    };
    let calls = vec![
        make_call("g1", "\\bfn make_widget\\b"),
        make_call("g2", "\\bfn make_widget\\b"),
        make_call("g3", "\\bfn make_widget\\b"),
        make_call("g4", "different"),
    ];
    let mut results: Vec<ToolResult> = calls.iter().map(make_result).collect();

    super::mark_intra_batch_duplicates(&calls, &mut results, &tools);

    assert!(
        results[0].content.get("duplicate_of").is_none(),
        "first call must not be marked"
    );
    assert_eq!(
        results[1]
            .content
            .get("duplicate_of")
            .and_then(Value::as_str),
        Some("g1"),
        "second call should be marked as duplicate of first"
    );
    assert_eq!(
        results[2]
            .content
            .get("duplicate_of")
            .and_then(Value::as_str),
        Some("g1"),
        "third call should also be marked as duplicate of first"
    );
    assert!(
        results[3].content.get("duplicate_of").is_none(),
        "different args must not be marked"
    );
    assert!(results[1].content.get("hint").is_some());
}

#[test]
fn redact_llm_input_items_drops_orphan_function_call_outputs_defensively() {
    // Even if a bug elsewhere (or a state loaded from an older squeezy
    // session) leaves an orphan in the conversation, the request build
    // path must scrub it before we hand the input to the provider —
    // otherwise OpenAI 400s the turn and the failure is sticky.
    let redactor = squeezy_core::Redactor::default();
    let input = vec![
        LlmInputItem::UserText("hi".to_string()),
        LlmInputItem::FunctionCall {
            call_id: "call_keep".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({"pattern": "x"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_keep".to_string(),
            output: "ok".to_string(),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_orphan".to_string(),
            output: "lingering output from a dropped call".to_string(),
        },
    ];

    let prepared = super::redact_llm_input_items(input, &redactor);

    let call_ids: std::collections::BTreeSet<&str> = prepared
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    for item in &prepared {
        if let LlmInputItem::FunctionCallOutput { call_id, .. } = item {
            assert!(
                call_ids.contains(call_id.as_str()),
                "orphan output reached provider input: call_id={call_id}"
            );
        }
    }
    assert_eq!(prepared.len(), 3, "only the orphan should be removed");
}

#[test]
fn tool_call_without_output_gets_synthetic_error_repair() {
    // A cancel mid-tool-call or an executor panic can leave a bare
    // `FunctionCall` in the conversation with no answering
    // `FunctionCallOutput`. Anthropic's Messages API rejects the whole
    // turn — *"tool_use blocks must be followed by a tool_result"* —
    // and the failure is sticky until `/clear`. The redact pipeline
    // must inject a synthetic error output so the orphan call is
    // closed before the request reaches the provider.
    let redactor = squeezy_core::Redactor::default();
    let input = vec![
        LlmInputItem::UserText("hi".to_string()),
        LlmInputItem::FunctionCall {
            call_id: "call_orphan".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::json!({}),
        },
        LlmInputItem::UserText("continue".to_string()),
    ];

    let prepared = super::redact_llm_input_items(input, &redactor);

    assert_eq!(prepared.len(), 4, "synthetic output should be inserted");
    assert!(matches!(
        &prepared[1],
        LlmInputItem::FunctionCall { call_id, .. } if call_id == "call_orphan"
    ));
    match &prepared[2] {
        LlmInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "call_orphan");
            assert!(
                output.contains("interrupted"),
                "synthetic output should advertise the repair: {output}"
            );
            assert!(output.contains("is_error"));
        }
        other => panic!("expected synthetic FunctionCallOutput, got {other:?}"),
    }
    assert!(matches!(
        &prepared[3],
        LlmInputItem::UserText(text) if text == "continue"
    ));
}

#[test]
fn mixed_orphan_call_and_orphan_output_both_repaired() {
    // The two repair passes must compose: an orphan output (without a
    // declaring call) is stripped while an orphan call (without an
    // answering output) is closed with a synthetic error in the same
    // pipeline run.
    let redactor = squeezy_core::Redactor::default();
    let input = vec![
        LlmInputItem::UserText("hi".to_string()),
        LlmInputItem::FunctionCall {
            call_id: "call_orphan".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::json!({}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "output_orphan".to_string(),
            output: "lingering output from a dropped call".to_string(),
        },
        LlmInputItem::UserText("continue".to_string()),
    ];

    let prepared = super::redact_llm_input_items(input, &redactor);

    let call_ids: std::collections::BTreeSet<&str> = prepared
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let output_ids: std::collections::BTreeSet<&str> = prepared
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(call_ids, std::collections::BTreeSet::from(["call_orphan"]));
    assert_eq!(
        output_ids,
        std::collections::BTreeSet::from(["call_orphan"]),
        "orphan output should be stripped and orphan call answered"
    );
}

#[tokio::test]
async fn compact_with_strategy_uses_extractive_when_no_model_configured() {
    use squeezy_core::Redactor;
    use std::sync::Arc;

    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            strategy: CompactionStrategy::ModelAssisted,
            model_assisted_model: None,
            recent_items: 2,
            min_items: 4,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(Vec::new()));
    let redactor = Arc::new(Redactor::default());
    let report = super::compact_conversation_with_strategy(
        &mut conversation,
        &mut state,
        &[],
        None,
        &provider,
        None,
        &redactor,
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .await
    .expect("compaction should still produce extractive output");
    assert!(
        report
            .summary
            .contains("Squeezy compacted conversation context")
    );
}

#[tokio::test]
async fn compact_with_strategy_accepts_structured_template_output() {
    // End-to-end: when the model returns a four-slot structured document,
    // `compact_conversation_with_strategy` accepts it verbatim and stamps
    // it into both the returned report and the in-memory state. This is
    // the happy path that F12-pi-iterative-summary-update unlocks; the
    // legacy "rewrite verbatim" prompt would have accepted any string,
    // including ones that silently dropped `## Decisions`.
    use squeezy_core::Redactor;
    use std::sync::Arc;

    let structured = "## Goal\nbuild a parser\n\n\
                      ## Progress\n- wrote lexer\n\n\
                      ## Decisions\n- use tree-sitter\n\n\
                      ## Next\n- wire grammar tests\n";
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(structured.to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("compaction".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));

    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            strategy: CompactionStrategy::ModelAssisted,
            model_assisted_model: Some("test-model".to_string()),
            model_assisted_timeout_secs: 5,
            recent_items: 2,
            min_items: 4,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let provider_trait: Arc<dyn LlmProvider> = provider.clone();
    let redactor = Arc::new(Redactor::default());
    let report = super::compact_conversation_with_strategy(
        &mut conversation,
        &mut state,
        &[],
        None,
        &provider_trait,
        None,
        &redactor,
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .await
    .expect("structured compaction should accept the model output");

    assert_eq!(report.summary.trim(), structured.trim());
    assert_eq!(state.summary.as_deref(), Some(structured.trim()));
    assert_eq!(
        conversation.first().and_then(|item| match item {
            LlmInputItem::UserText(text) => Some(text.as_str()),
            _ => None,
        }),
        Some(structured.trim()),
        "synthetic summary head must carry the structured output"
    );

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        1,
        "model-assisted compaction issues exactly one request"
    );
    let prompt = match requests[0].input.first().expect("input present") {
        LlmInputItem::UserText(text) => text.as_str(),
        other => panic!("expected UserText prompt, got {other:?}"),
    };
    for slot in ["## Goal", "## Progress", "## Decisions", "## Next"] {
        assert!(
            prompt.contains(slot),
            "model prompt must advertise slot header {slot}"
        );
    }
    assert!(
        prompt.contains("<new-conversation>"),
        "model prompt must wrap the extractive output in a `<new-conversation>` block"
    );
}

#[tokio::test]
async fn compact_with_strategy_falls_back_when_model_output_missing_slots() {
    // The validator is the safety net: if the model returns prose without
    // all four required slots, `compact_conversation_with_strategy` keeps
    // the deterministic extractive summary so the file-lineage append pass
    // still has a stable anchor. Any model that ignored the structured
    // template instructions would otherwise quietly degrade the summary
    // chain — the same failure mode F12-pi-iterative-summary-update fixes.
    use squeezy_core::Redactor;
    use std::sync::Arc;

    let unstructured = "Here is a freeform summary that drops decisions and next steps entirely.";
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(unstructured.to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("compaction".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));

    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            strategy: CompactionStrategy::ModelAssisted,
            model_assisted_model: Some("test-model".to_string()),
            model_assisted_timeout_secs: 5,
            recent_items: 2,
            min_items: 4,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let provider_trait: Arc<dyn LlmProvider> = provider.clone();
    let redactor = Arc::new(Redactor::default());
    let report = super::compact_conversation_with_strategy(
        &mut conversation,
        &mut state,
        &[],
        None,
        &provider_trait,
        None,
        &redactor,
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .await
    .expect("compaction should still produce extractive output");

    assert!(
        report
            .summary
            .contains("Squeezy compacted conversation context"),
        "missing-slot output must fall back to the extractive summary, got: {}",
        report.summary,
    );
    assert!(
        !report.summary.contains(unstructured),
        "unstructured model output must not be promoted to the final summary"
    );
}

#[tokio::test]
async fn compact_with_strategy_passes_previous_summary_block_on_iterative_compaction() {
    // The iterative-update contract: on the second compaction the prior
    // summary must reach the model as a *dedicated* `<previous-summary>`
    // block so it can deterministically carry forward slot contents.
    // Embedding it only inline inside the extractive output (the legacy
    // behaviour) is what made the slot chain lose ~60% of content after
    // a handful of generations.
    use squeezy_core::Redactor;
    use std::sync::Arc;

    let structured =
        "## Goal\ngoal text\n\n## Progress\n- p1\n\n## Decisions\n- d1\n\n## Next\n- n1\n";
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(structured.to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("compaction-1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(structured.to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("compaction-2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));

    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            strategy: CompactionStrategy::ModelAssisted,
            model_assisted_model: Some("test-model".to_string()),
            model_assisted_timeout_secs: 5,
            recent_items: 2,
            min_items: 4,
            estimated_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut state = ContextCompactionState::default();
    let provider_trait: Arc<dyn LlmProvider> = provider.clone();
    let redactor = Arc::new(Redactor::default());

    let mut conversation = mid_turn_test_conversation();
    super::compact_conversation_with_strategy(
        &mut conversation,
        &mut state,
        &[],
        None,
        &provider_trait,
        None,
        &redactor,
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .await
    .expect("first compaction");
    assert_eq!(state.summary.as_deref(), Some(structured.trim()));

    let mut conversation = mid_turn_test_conversation();
    super::compact_conversation_with_strategy(
        &mut conversation,
        &mut state,
        &[],
        None,
        &provider_trait,
        None,
        &redactor,
        &config,
        ContextCompactionTrigger::Manual,
        true,
    )
    .await
    .expect("second compaction");

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "each compaction issues exactly one model-assisted request"
    );
    let first_prompt = match requests[0].input.first().expect("input present") {
        LlmInputItem::UserText(text) => text.as_str(),
        other => panic!("expected UserText prompt, got {other:?}"),
    };
    let second_prompt = match requests[1].input.first().expect("input present") {
        LlmInputItem::UserText(text) => text.as_str(),
        other => panic!("expected UserText prompt, got {other:?}"),
    };

    // Match the actual block opening (`<previous-summary>\n`) — the Rules
    // text also references the tag inside backticks, so the bare string
    // would over-match on cold start.
    assert!(
        !first_prompt.contains("<previous-summary>\n"),
        "cold-start compaction must not emit a `<previous-summary>` block; got prompt:\n{first_prompt}"
    );
    assert!(
        second_prompt.contains("<previous-summary>\n"),
        "iterative compaction must surface the prior summary as a `<previous-summary>` block; got prompt:\n{second_prompt}"
    );
    assert!(
        second_prompt.contains("- d1"),
        "iterative compaction must embed the prior summary's `## Decisions` body verbatim; got prompt:\n{second_prompt}"
    );
}

#[test]
fn parse_subagent_structured_tail_extracts_findings_object() {
    let text = "Here is the plan.\n\n{\"findings\": [{\"finding\": \"missing tracing\", \"recommendation\": \"add span\", \"priority\": \"warning\"}], \"summary\": \"add a tracing span\"}";
    let parsed = super::parse_subagent_structured_tail(text).expect("structured tail should parse");
    assert_eq!(parsed["summary"], json!("add a tracing span"));
    assert_eq!(parsed["findings"][0]["finding"], json!("missing tracing"));
    assert_eq!(parsed["findings"][0]["priority"], json!("warning"));
}

#[test]
fn parse_subagent_structured_tail_returns_none_for_plain_text() {
    assert!(
        super::parse_subagent_structured_tail("Just a free-text summary with no JSON in sight.")
            .is_none(),
        "plain text must not be coerced into structured output"
    );
}

#[test]
fn parse_subagent_structured_tail_accepts_bare_json_object() {
    let text = "{\"findings\": [], \"summary\": \"nothing to report\"}";
    let parsed = super::parse_subagent_structured_tail(text).expect("bare JSON object parses");
    assert_eq!(parsed["findings"], json!([]));
    assert_eq!(parsed["summary"], json!("nothing to report"));
}

#[test]
fn parse_subagent_structured_tail_rejects_json_array_only() {
    // The contract requires an object. A bare array tail should not be
    // misclassified as the structured-output payload.
    assert!(super::parse_subagent_structured_tail("[1, 2, 3]").is_none());
}

#[test]
fn plan_subagent_instructions_advertise_json_tail_contract() {
    let request = super::SubagentRequest {
        prompt: "plan something".to_string(),
        scope: None,
        thoroughness: None,
    };
    let plan = super::subagent_instructions(SubagentKind::Plan, &request);
    assert!(
        plan.contains("Output contract") && plan.contains("\"findings\""),
        "plan prompt must teach the JSON tail contract: {plan}"
    );
    let review = super::subagent_instructions(SubagentKind::Review, &request);
    assert!(
        review.contains("Output contract") && review.contains("\"findings\""),
        "review prompt must teach the JSON tail contract: {review}"
    );
    let delegate = super::subagent_instructions(SubagentKind::Delegate, &request);
    assert!(
        !delegate.contains("Output contract"),
        "delegate prompt must not advertise the Plan/Review JSON tail contract: {delegate}"
    );
}

#[tokio::test]
async fn plan_subagent_parses_json_tail_into_structured_output() {
    let provider = Arc::new(MockProvider::new(vec![
        // Parent turn 1: emit a delegate_plan tool call.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "plan_1".to_string(),
                name: DELEGATE_PLAN_TOOL_NAME.to_string(),
                arguments: json!({ "goal": "add tracing to ingest pipeline" }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Plan subagent: return text followed by a JSON tail.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "Plan: instrument ingest with spans.\n\n".to_string(),
            )),
            Ok(LlmEvent::TextDelta(
                "{\"findings\": [{\"finding\": \"missing tracing on ingest\", \"recommendation\": \"add span around process_batch\", \"priority\": \"warning\"}], \"summary\": \"add a tracing span on ingest\"}".to_string(),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("plan_subagent_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Parent turn 2: wrap up.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("plan tracing".to_string(), CancellationToken::new());
    let mut plan_result = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolCallCompleted { result, .. } = event
            && result.tool_name == DELEGATE_PLAN_TOOL_NAME
        {
            plan_result = Some(result);
        }
    }

    let plan_result = plan_result.expect("plan tool result");
    assert_eq!(plan_result.status, ToolStatus::Success);
    let structured = plan_result
        .content
        .get("structured_output")
        .expect("plan subagent must surface structured_output on success");
    assert_eq!(
        structured["findings"][0]["finding"],
        json!("missing tracing on ingest")
    );
    assert_eq!(structured["findings"][0]["priority"], json!("warning"));
    assert_eq!(structured["summary"], json!("add a tracing span on ingest"));
    let summary = plan_result
        .content
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .expect("summary must be present");
    assert!(
        summary.contains("instrument ingest"),
        "raw assistant text must still appear in summary: {summary}"
    );
}

#[tokio::test]
async fn plan_subagent_falls_back_to_summary_when_json_missing() {
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "plan_1".to_string(),
                name: DELEGATE_PLAN_TOOL_NAME.to_string(),
                arguments: json!({ "goal": "describe ingest" }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Plan subagent: emits plain prose with no JSON tail.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "Plan: refactor ingest module to extract span helpers.".to_string(),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("plan_subagent_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("parent_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("plan tracing".to_string(), CancellationToken::new());
    let mut plan_result = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolCallCompleted { result, .. } = event
            && result.tool_name == DELEGATE_PLAN_TOOL_NAME
        {
            plan_result = Some(result);
        }
    }

    let plan_result = plan_result.expect("plan tool result");
    assert_eq!(plan_result.status, ToolStatus::Success);
    assert!(
        plan_result.content.get("structured_output").is_none(),
        "free-text plan subagent must not produce structured_output"
    );
    let summary = plan_result
        .content
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .expect("summary must be present");
    assert!(
        summary.contains("refactor ingest module"),
        "raw assistant text must still appear in summary: {summary}"
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

#[test]
fn delegate_chain_threads_previous_step_summary_into_next_step_prompt() {
    // F10: the chain helper must replace every literal `{previous}` token
    // in step N+1's prompt with step N's summary verbatim. Step 0 sees an
    // empty `previous`, so the leading step's template stays
    // byte-identical apart from the placeholder being erased.
    //
    // Substitution is exercised through `chain_substitute_previous`
    // directly so the test does not need a live LLM or subagent registry
    // — the chain helper IS the contract documented in the
    // `delegate_chain` tool description, and a regression here would
    // silently send unrendered `{previous}` text to the model.
    let step_a = "Summarise the auth module";
    let step_b_template = "Now critique the summary: {previous}. Flag any missing risks.";

    let step_a_rendered = chain_substitute_previous(step_a, "");
    assert_eq!(
        step_a_rendered, step_a,
        "first step has no prior summary; template must pass through unchanged"
    );

    let step_a_summary =
        "Auth uses JWT with HS256, refresh tokens stored in redis, 14d TTL.".to_string();
    let step_b_rendered = chain_substitute_previous(step_b_template, &step_a_summary);
    assert_eq!(
        step_b_rendered,
        format!(
            "Now critique the summary: {summary}. Flag any missing risks.",
            summary = step_a_summary
        ),
        "{{previous}} must be replaced verbatim with step A's summary so the chain threads output of A → input of B"
    );
    assert!(
        !step_b_rendered.contains(DELEGATE_CHAIN_PREVIOUS_PLACEHOLDER),
        "rendered chain prompt must not still contain the literal placeholder"
    );

    // Also confirm the parser shape so the dispatcher can build the
    // chained delegate calls without a separate JSON contract round-trip.
    let chain_call = ToolCall {
        call_id: "chain_1".to_string(),
        name: DELEGATE_CHAIN_TOOL_NAME.to_string(),
        arguments: json!({
            "steps": [
                { "prompt": step_a },
                { "prompt": step_b_template },
            ]
        }),
    };
    let steps = parse_delegate_chain_steps(&chain_call).expect("chain args valid");
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].prompt, step_a);
    assert_eq!(steps[1].prompt, step_b_template);
    assert!(steps[0].scope.is_none());
    assert!(steps[1].scope.is_none());

    // Missing prompt on a step must surface as an actionable error
    // before any subagent lease is taken.
    let bad = ToolCall {
        call_id: "chain_bad".to_string(),
        name: DELEGATE_CHAIN_TOOL_NAME.to_string(),
        arguments: json!({ "steps": [{ "prompt": "" }] }),
    };
    let err = parse_delegate_chain_steps(&bad).expect_err("empty prompt must error");
    assert!(
        err.contains("prompt"),
        "error must mention the missing prompt field: {err}"
    );
}

#[test]
fn replace_config_swaps_immediately() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(AppConfig::default(), provider);
    assert_eq!(
        agent.config_snapshot().tui.response_verbosity,
        squeezy_core::ResponseVerbosity::Normal
    );
    let mut next = agent.config_snapshot();
    next.tui.response_verbosity = squeezy_core::ResponseVerbosity::Concise;
    agent.replace_config(next);
    assert_eq!(
        agent.config_snapshot().tui.response_verbosity,
        squeezy_core::ResponseVerbosity::Concise
    );
    assert!(agent.pending_config_swap().is_none());
}

#[test]
fn arm_then_drain_applies_swap_with_optional_provider() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(AppConfig::default(), provider);
    let original_model = agent.config_snapshot().model.clone();

    let mut next = agent.config_snapshot();
    next.model = "claude-opus-4-7".to_string();
    agent.arm_config_swap(PendingConfigSwap {
        config: next,
        provider: None,
        display_note: Some("model swap".to_string()),
    });
    // Pre-drain: still the old config.
    assert_eq!(agent.config_snapshot().model, original_model);
    assert!(agent.pending_config_swap().is_some());

    let drained = agent.drain_pending_swap().expect("swap was armed");
    assert_eq!(drained.display_note.as_deref(), Some("model swap"));
    assert_eq!(agent.config_snapshot().model, "claude-opus-4-7");
    assert!(agent.pending_config_swap().is_none());
}

#[tokio::test]
async fn drained_swap_makes_next_request_carry_new_model_id() {
    // Build a provider that records every request it sees and answers with
    // a single Completed event so the turn loop terminates promptly.
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("ok".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_swap".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let mut agent = Agent::new(AppConfig::default(), provider.clone());
    let original_model = agent.config_snapshot().model.clone();

    let mut next = agent.config_snapshot();
    next.model = "claude-haiku-4-5-20251001".to_string();
    agent.arm_config_swap(PendingConfigSwap {
        config: next,
        provider: None,
        display_note: None,
    });
    // Drain the swap (this is what the TUI does at the top of each new
    // user turn) then run a real start_turn against the MockProvider.
    let _drained = agent.drain_pending_swap().expect("swap was armed");

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    while rx.recv().await.is_some() {}

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "expected one provider request");
    assert_eq!(
        requests[0].model.as_ref(),
        "claude-haiku-4-5-20251001",
        "swapped model id should reach the wire (was {})",
        original_model
    );
}

#[tokio::test]
async fn plan_mode_request_user_input_pauses_turn_and_resumes_with_choice() {
    use super::{REQUEST_USER_INPUT_TOOL_NAME, RequestUserInputResponse};

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "ask_1".to_string(),
                name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
                arguments: json!({
                    "question": "Which approach?",
                    "choices": [
                        {"label": "A", "value": "approach-a"},
                        {"label": "B", "value": "approach-b"}
                    ]
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let config = AppConfig {
        session_mode: SessionMode::Plan,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("plan it".to_string(), CancellationToken::new());
    let mut completed = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::RequestUserInputRequested {
                request,
                response_tx,
                ..
            } => {
                assert_eq!(request.question, "Which approach?");
                assert_eq!(request.choices.len(), 2);
                let _ = response_tx.send(RequestUserInputResponse::choice("approach-b"));
            }
            AgentEvent::Completed { .. } => {
                completed = true;
                break;
            }
            AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    assert!(completed, "turn must complete after answer is provided");
    let requests = provider.requests();
    assert!(
        requests.len() >= 2,
        "expected a follow-up round once the user answered; got {} request(s)",
        requests.len()
    );
}

#[tokio::test]
async fn plan_mode_request_user_input_rejects_choice_value_not_in_offered_set() {
    use super::{REQUEST_USER_INPUT_TOOL_NAME, RequestUserInputResponse};

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "ask_1".to_string(),
                name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
                arguments: json!({
                    "question": "Which approach?",
                    "choices": [
                        {"label": "Simplify", "value": "simplify"},
                        {"label": "Split",    "value": "split"},
                        {"label": "Perf",     "value": "perf"}
                    ]
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let config = AppConfig {
        session_mode: SessionMode::Plan,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("plan it".to_string(), CancellationToken::new());
    let mut saw_validation_error = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::RequestUserInputRequested { response_tx, .. } => {
                // Driver/UI responds with a choice_value that is not in the
                // offered choices — emulates the OpenAI wave2-06 trace.
                let _ = response_tx.send(RequestUserInputResponse::choice("small"));
            }
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "ask_1" => {
                assert_eq!(result.status, squeezy_tools::ToolStatus::Error);
                let content = serde_json::to_string(&result.content).unwrap();
                assert!(
                    content.contains("choice_value not in offered choices"),
                    "expected typed validation error in payload: {content}",
                );
                saw_validation_error = true;
            }
            AgentEvent::Completed { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    assert!(
        saw_validation_error,
        "agent must surface a typed ToolStatus::Error when choice_value is out of bounds"
    );
}

#[tokio::test]
async fn plan_mode_request_user_input_rejects_freeform_when_disallowed() {
    use super::{REQUEST_USER_INPUT_TOOL_NAME, RequestUserInputResponse};

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "ask_1".to_string(),
                name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
                // allow_freeform omitted → defaults to false (matches the
                // Anthropic wave2-06 bumped trace).
                arguments: json!({
                    "question": "What is the primary goal of this refactor?",
                    "choices": [
                        {"label": "Speed",    "value": "speed"},
                        {"label": "Clarity",  "value": "clarity"}
                    ]
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let config = AppConfig {
        session_mode: SessionMode::Plan,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("plan it".to_string(), CancellationToken::new());
    let mut saw_validation_error = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::RequestUserInputRequested { response_tx, .. } => {
                // Driver/UI replies with a freeform string despite the
                // request not allowing freeform answers.
                let _ = response_tx.send(RequestUserInputResponse::freeform(
                    "Yes - focus on the eval driver",
                ));
            }
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "ask_1" => {
                assert_eq!(result.status, squeezy_tools::ToolStatus::Error);
                let content = serde_json::to_string(&result.content).unwrap();
                assert!(
                    content.contains("freeform not allowed for this question"),
                    "expected typed validation error in payload: {content}",
                );
                saw_validation_error = true;
            }
            AgentEvent::Completed { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    assert!(
        saw_validation_error,
        "agent must surface a typed ToolStatus::Error when freeform is disabled"
    );
}

#[tokio::test]
async fn build_mode_refuses_request_user_input_call() {
    use super::REQUEST_USER_INPUT_TOOL_NAME;

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "ask_1".to_string(),
                name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
                arguments: json!({ "question": "ok?" }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("noted".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let config = AppConfig {
        session_mode: SessionMode::Build,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("just do it".to_string(), CancellationToken::new());
    let mut saw_request_user_input = false;
    let mut saw_refusal_result = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::RequestUserInputRequested { .. } => saw_request_user_input = true,
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "ask_1" => {
                assert_eq!(result.status, squeezy_tools::ToolStatus::Denied);
                let content = serde_json::to_string(&result.content).unwrap();
                assert!(
                    content.contains("Plan mode"),
                    "refusal payload should explain the mode gating: {content}",
                );
                saw_refusal_result = true;
            }
            AgentEvent::Completed { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    assert!(
        !saw_request_user_input,
        "Build mode must not surface a RequestUserInputRequested event"
    );
    assert!(
        saw_refusal_result,
        "expected a ToolCallCompleted with the mode-refusal payload"
    );
}

#[tokio::test]
async fn plan_mode_instructions_are_appended_to_request() {
    use super::plan_mode::PLAN_MODE_INSTRUCTIONS;

    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_plan".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let config = AppConfig {
        session_mode: SessionMode::Plan,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("draft a refactor".to_string(), CancellationToken::new());
    while rx.recv().await.is_some() {}

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "expected exactly one provider request");
    let instructions = &requests[0].instructions;
    assert!(
        instructions.contains(PLAN_MODE_INSTRUCTIONS),
        "Plan-mode instructions missing from request: {instructions}"
    );
}

#[tokio::test]
async fn build_mode_instructions_omit_plan_overlay() {
    use super::plan_mode::PLAN_MODE_INSTRUCTIONS;

    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_build".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let config = AppConfig {
        session_mode: SessionMode::Build,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn("ship a fix".to_string(), CancellationToken::new());
    while rx.recv().await.is_some() {}

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "expected exactly one provider request");
    let instructions = &requests[0].instructions;
    assert!(
        !instructions.contains(PLAN_MODE_INSTRUCTIONS),
        "Build-mode request must not include Plan overlay: {instructions}"
    );
}

#[test]
fn progress_snapshot_fires_at_stride_multiples() {
    let mut broker = CostBroker::new(&AppConfig::default());
    // Simulate 7 completed tool calls (4 success, 2 errors, 1 denial) and
    // a running provider cost so the snapshot carries non-zero numbers.
    broker.metrics.tool_successes = 4;
    broker.metrics.tool_errors = 2;
    broker.metrics.tool_denials = 1;
    broker.metrics.provider.input_tokens = Some(12_500);
    broker.metrics.provider.estimated_usd_micros = Some(2_500);

    assert!(broker.progress_snapshot_if_due(3).is_none(), "7 % 3 ≠ 0");
    broker.metrics.tool_successes = 5; // total now 8
    assert!(broker.progress_snapshot_if_due(3).is_none(), "8 % 3 ≠ 0");
    broker.metrics.tool_successes = 6; // total now 9
    let snap = broker
        .progress_snapshot_if_due(3)
        .expect("stride boundary at 9 tool calls");
    assert_eq!(snap.tool_count, 9);
    assert_eq!(snap.input_tokens, 12_500);
    assert_eq!(snap.micro_usd, 2_500);
}

#[test]
fn progress_snapshot_returns_none_before_any_calls() {
    let broker = CostBroker::new(&AppConfig::default());
    assert!(broker.progress_snapshot_if_due(3).is_none());
}

/// Repro for bd ticket squeezy-xt2o: with a $0.01 cap and a fresh broker the
/// pre-flight gate must refuse to dispatch a turn whose projected input/output
/// pricing already exceeds the cap, so the broker trips *before* the over-cap
/// spend is billed. claude-haiku-4-5-20251001 prices output at $5/Mtok, so a
/// 4096-output-token reply is $0.02048 by itself — comfortably over the cap.
#[test]
fn cost_cap_fires_pre_flight_on_first_turn_when_projection_exceeds_cap() {
    let config = AppConfig {
        max_session_cost_usd_micros: Some(10_000),
        max_output_tokens: Some(4_096),
        ..AppConfig::default()
    };
    let broker = CostBroker::new(&config);
    // Pre-flight: nothing has been billed yet, so the post-hoc check is
    // silent — only the projection should catch the overrun.
    assert!(
        broker.session_cap_reached().is_none(),
        "session_cap_reached must not fire before any provider cost lands"
    );
    let status = broker
        .projected_session_cap_overrun(
            "anthropic",
            "claude-haiku-4-5-20251001",
            1_000, // projected input tokens
            4_096, // projected output tokens
        )
        .expect("projected spend ($0.0205 input + output) must exceed $0.01 cap");
    assert_eq!(status.cap_usd_micros, 10_000);
    assert!(
        status.spent_usd_micros >= status.cap_usd_micros,
        "projected total ({} micros) must be at or above cap ({} micros)",
        status.spent_usd_micros,
        status.cap_usd_micros
    );
    // The "spent" reported on the cap-reached event is the *projection*, not
    // the actual recorded spend, so the operator sees the would-have-been
    // total they were saved from.
    assert!(status.percent >= 100, "percent should reflect overrun");
}

/// Drives the broker through a scripted spent sequence: after a cheap first
/// round lands, the next round's projection must trip the cap pre-flight even
/// though the post-hoc check is still under cap.
#[test]
fn cost_cap_fires_pre_flight_after_partial_spend_under_cap() {
    let config = AppConfig {
        max_session_cost_usd_micros: Some(10_000),
        max_output_tokens: Some(4_096),
        ..AppConfig::default()
    };
    let mut broker = CostBroker::new(&config);
    // Round 1 landed at $0.006 spent — still well under the $0.01 cap, so
    // post-hoc check passes.
    broker.seed_session(6_000, squeezy_llm::TokenCalibration::default());
    assert!(
        broker.session_cap_reached().is_none(),
        "post-hoc check must not trip at 60% of cap"
    );
    // Pre-flight projection for the next round: ~$0.0205 worth of output
    // tokens at haiku pricing. 6_000 + 20_480 = 26_480 micros — over cap.
    let status = broker
        .projected_session_cap_overrun("anthropic", "claude-haiku-4-5-20251001", 512, 4_096)
        .expect("pre-flight projection must trip at 6_000 spent + projected round");
    assert!(
        status.spent_usd_micros >= 10_000,
        "projected total must be >= cap; got {}",
        status.spent_usd_micros
    );
}

/// Once spent has crossed the cap, the post-hoc check still fires as the
/// safety-net path, so resuming a session whose prior turn somehow exceeded
/// the cap (e.g. a recorded provider cost that came in higher than the
/// pre-flight projection) is still gated before the next round dispatches.
#[test]
fn cost_cap_post_hoc_check_still_fires_when_spent_exceeds_cap() {
    let config = AppConfig {
        max_session_cost_usd_micros: Some(10_000),
        ..AppConfig::default()
    };
    let mut broker = CostBroker::new(&config);
    broker.seed_session(12_457, squeezy_llm::TokenCalibration::default());
    let status = broker
        .session_cap_reached()
        .expect("post-hoc check must fire when spent >= cap");
    assert_eq!(status.spent_usd_micros, 12_457);
    assert_eq!(status.cap_usd_micros, 10_000);
    assert_eq!(status.percent, 124);
}

/// Cap unset → both gates are silent; the broker must not synthesize a cap
/// from defaults when the operator explicitly disabled it.
#[test]
fn cost_cap_pre_flight_silent_when_no_cap_configured() {
    let config = AppConfig {
        max_session_cost_usd_micros: None,
        ..AppConfig::default()
    };
    let broker = CostBroker::new(&config);
    assert!(broker.session_cap_reached().is_none());
    assert!(
        broker
            .projected_session_cap_overrun(
                "anthropic",
                "claude-haiku-4-5-20251001",
                1_000_000,
                1_000_000
            )
            .is_none(),
        "no cap configured → projection must not trip"
    );
}

/// Provider/model pair the registry can't price → projection returns `None`
/// so the agent falls through to the post-hoc check rather than incorrectly
/// gating dispatch on an unknown rate.
#[test]
fn cost_cap_pre_flight_silent_when_model_has_no_pricing() {
    let config = AppConfig {
        max_session_cost_usd_micros: Some(10_000),
        ..AppConfig::default()
    };
    let broker = CostBroker::new(&config);
    assert!(
        broker
            .projected_session_cap_overrun("ollama", "made-up-local-model", 1_000, 4_096)
            .is_none(),
        "unpriced model → projection must abstain"
    );
}

fn shell_fallback_result(
    backend: &str,
    fallback_count: u64,
    first_in_session: bool,
) -> squeezy_tools::ToolResult {
    squeezy_tools::ToolResult {
        call_id: "shell-call".to_string(),
        tool_name: "shell".to_string(),
        status: ToolStatus::Success,
        content: json!({
            "sandbox": {
                "backend": "none",
                "mode": "best_effort",
                "best_effort_fallback": {
                    "backend": backend,
                    "fallback_count": fallback_count,
                    "first_in_session": first_in_session,
                }
            }
        }),
        cost_hint: squeezy_tools::ToolCostHint::default(),
        receipt: squeezy_tools::ToolReceipt {
            output_sha256: "0".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
    }
}

#[tokio::test]
async fn shell_sandbox_fallback_warns_tui_exactly_once_per_session() {
    // F3-4: the TUI must learn about the sandbox degradation on the
    // first fallback and never again in the same session. The tool
    // layer's one-shot latch drives `first_in_session`; the agent
    // routes that signal into AgentEvent::ShellSandboxBestEffortFallback.
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(8);

    let first = shell_fallback_result("macos-sandbox-exec", 1, true);
    let second = shell_fallback_result("macos-sandbox-exec", 2, false);
    let third = shell_fallback_result("macos-sandbox-exec", 3, false);

    maybe_emit_shell_sandbox_fallback_warning(&tx, TurnId::new(7), &first).await;
    maybe_emit_shell_sandbox_fallback_warning(&tx, TurnId::new(8), &second).await;
    maybe_emit_shell_sandbox_fallback_warning(&tx, TurnId::new(9), &third).await;

    drop(tx);

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }

    assert_eq!(
        events.len(),
        1,
        "exactly one TUI warning must be emitted per session"
    );
    let AgentEvent::ShellSandboxBestEffortFallback {
        turn_id,
        backend,
        fallback_count,
    } = &events[0]
    else {
        panic!("expected AgentEvent::ShellSandboxBestEffortFallback");
    };
    assert_eq!(
        turn_id.get(),
        7,
        "warning must carry the originating turn id"
    );
    assert_eq!(backend, "macos-sandbox-exec");
    assert_eq!(*fallback_count, 1);
}

#[tokio::test]
async fn shell_sandbox_fallback_ignores_clean_shell_results_and_non_shell_tools() {
    // Defence in depth: clean shell completions and non-shell tools
    // must NOT trip the warning; the agent helper inspects the result
    // payload and only routes on the embedded fallback descriptor.
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(4);

    let clean_shell = squeezy_tools::ToolResult {
        call_id: "call".to_string(),
        tool_name: "shell".to_string(),
        status: ToolStatus::Success,
        content: json!({
            "sandbox": {
                "backend": "macos-sandbox-exec",
                "mode": "required",
            }
        }),
        cost_hint: squeezy_tools::ToolCostHint::default(),
        receipt: squeezy_tools::ToolReceipt {
            output_sha256: "0".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
    };
    let read_file = squeezy_tools::ToolResult {
        call_id: "call".to_string(),
        tool_name: "read_file".to_string(),
        status: ToolStatus::Success,
        content: json!({"path": "foo.rs"}),
        cost_hint: squeezy_tools::ToolCostHint::default(),
        receipt: squeezy_tools::ToolReceipt {
            output_sha256: "0".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
    };

    maybe_emit_shell_sandbox_fallback_warning(&tx, TurnId::new(1), &clean_shell).await;
    maybe_emit_shell_sandbox_fallback_warning(&tx, TurnId::new(2), &read_file).await;
    drop(tx);

    assert!(
        rx.recv().await.is_none(),
        "no AgentEvent must fire for clean or non-shell results"
    );
}

#[tokio::test]
async fn shell_sandbox_fallback_counter_emits_per_call() {
    // The `approval.best_effort.fallback{tool=shell}` counter ticks on
    // EVERY fallback, even after the TUI warning has already fired.
    // We drive `emit_tool_telemetry` directly with synthesized results
    // so the test does not depend on the live shell sandbox.
    let temp = std::env::temp_dir().join(format!(
        "squeezy-best-effort-fallback-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&temp).expect("temp dir");
    let install_id_path = temp.join("install_id");
    let config = AppConfig {
        telemetry: squeezy_telemetry::telemetry_config(true, "https://telemetry.example/v1/batch"),
        ..AppConfig::default()
    };
    let telemetry = TelemetryClient::from_config_with_install_path(&config, &install_id_path);
    assert!(telemetry.enabled(), "telemetry must be live for this test");

    let call = ToolCall {
        call_id: "shell-call".to_string(),
        name: "shell".to_string(),
        arguments: json!({"command": "true"}),
    };

    // First fallback: first_in_session=true.
    emit_tool_telemetry(
        &config,
        &telemetry,
        TurnId::new(1),
        1,
        &call,
        &shell_fallback_result("macos-sandbox-exec", 1, true),
        Duration::from_millis(5),
    );
    // Second fallback: counter must still tick even though the TUI
    // one-shot latch has flipped.
    emit_tool_telemetry(
        &config,
        &telemetry,
        TurnId::new(1),
        2,
        &call,
        &shell_fallback_result("macos-sandbox-exec", 2, false),
        Duration::from_millis(7),
    );
    // Clean shell call must NOT add a fallback counter event.
    let clean = squeezy_tools::ToolResult {
        call_id: "clean".to_string(),
        tool_name: "shell".to_string(),
        status: ToolStatus::Success,
        content: json!({
            "sandbox": {
                "backend": "macos-sandbox-exec",
                "mode": "required",
            }
        }),
        cost_hint: squeezy_tools::ToolCostHint::default(),
        receipt: squeezy_tools::ToolReceipt {
            output_sha256: "0".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
    };
    emit_tool_telemetry(
        &config,
        &telemetry,
        TurnId::new(1),
        3,
        &call,
        &clean,
        Duration::from_millis(2),
    );

    // `spawn` is fire-and-forget; let queued tasks land in the queue.
    // Each `emit_tool_telemetry` schedules background tasks for both
    // the tool-completed event and (optionally) the fallback counter,
    // so we yield twice per call and then poll the queue snapshot
    // until both invariants hold or we run out of patience.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut snapshot = Vec::new();
    while std::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
        snapshot = telemetry.pending_events_snapshot().await;
        let tool_completed = snapshot
            .iter()
            .filter(|event| event.event == squeezy_telemetry::TelemetryEventName::ToolCompleted)
            .count();
        let fallback = snapshot
            .iter()
            .filter(|event| {
                event.event == squeezy_telemetry::TelemetryEventName::ShellSandboxBestEffortFallback
            })
            .count();
        if tool_completed >= 3 && fallback >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut fallback_events = 0;
    let mut tool_completed_events = 0;
    for event in &snapshot {
        match event.event {
            squeezy_telemetry::TelemetryEventName::ShellSandboxBestEffortFallback => {
                fallback_events += 1;
                assert_eq!(
                    event.properties.sandbox_backend.as_deref(),
                    Some("macos-sandbox-exec")
                );
            }
            squeezy_telemetry::TelemetryEventName::ToolCompleted => {
                tool_completed_events += 1;
            }
            _ => {}
        }
    }
    assert_eq!(
        fallback_events, 2,
        "counter must tick on every fallback (got {fallback_events})"
    );
    assert_eq!(
        tool_completed_events, 3,
        "tool_completed must still fire for every call regardless of fallback"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn effective_tool_choice_downgrades_required_after_round_zero() {
    assert_eq!(
        effective_tool_choice(Some("required"), 0),
        Some("required".to_string()),
        "round 0 keeps 'required' to force the first tool call"
    );
    assert_eq!(
        effective_tool_choice(Some("required"), 1),
        Some("auto".to_string()),
        "round 1+ downgrades so the model can end the turn naturally"
    );
    assert_eq!(
        effective_tool_choice(Some("required"), 47),
        Some("auto".to_string())
    );
}

#[test]
fn effective_tool_choice_passes_through_other_values_unchanged() {
    for round in [0_usize, 1, 5] {
        assert_eq!(
            effective_tool_choice(Some("auto"), round),
            Some("auto".to_string())
        );
        assert_eq!(
            effective_tool_choice(Some("none"), round),
            Some("none".to_string())
        );
        assert_eq!(effective_tool_choice(None, round), None);
    }
}

#[tokio::test]
async fn max_tokens_stop_reason_emits_failed_with_recovery_hint() {
    // Provider stream completes cleanly but signals truncation via
    // `StopReason::MaxTokens`. Before this change Anthropic raised an
    // opaque `ProviderStream("Anthropic response stopped after
    // max_tokens")` here; now the agent surfaces an `AgentEvent::Failed`
    // with a descriptive error so the TUI can suggest /compact or
    // raising `max_output_tokens`.
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("partial".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_trunc".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: Some(StopReason::MaxTokens),
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider);
    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut failed_error: Option<String> = None;
    let mut saw_success = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Failed { error, .. } => failed_error = Some(error.to_string()),
            AgentEvent::Completed { .. } => saw_success = true,
            _ => {}
        }
    }
    let err = failed_error.expect("AgentEvent::Failed must fire for MaxTokens");
    assert!(
        err.contains("max_tokens"),
        "error message should mention max_tokens, got: {err}"
    );
    assert!(
        !saw_success,
        "MaxTokens must not produce a successful Completed event"
    );
}

#[tokio::test]
async fn refusal_stop_reason_emits_failed_with_safety_hint() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_refusal".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: Some(StopReason::Refusal),
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider);
    let mut rx = agent.start_turn("forbidden".to_string(), CancellationToken::new());
    let mut failed_error: Option<String> = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Failed { error, .. } = event {
            failed_error = Some(error.to_string());
        }
    }
    let err = failed_error.expect("AgentEvent::Failed must fire for Refusal");
    assert!(
        err.contains("refused"),
        "error message should mention refusal, got: {err}"
    );
}

#[tokio::test]
async fn end_turn_stop_reason_completes_successfully() {
    // Regression guard for the audit's `end_turn_with_empty_content_is_success`
    // case: a clean `EndTurn` with text content must still take the
    // success path even now that explicit branches exist on stop_reason.
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("done".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_ok".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: Some(StopReason::EndTurn),
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider);
    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut saw_success = false;
    let mut saw_failure = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Completed { .. } => saw_success = true,
            AgentEvent::Failed { .. } => saw_failure = true,
            _ => {}
        }
    }
    assert!(saw_success, "EndTurn must produce a successful Completed");
    assert!(!saw_failure, "EndTurn must not produce a Failed event");
}

fn make_subagent_execution(
    supporting_receipts: Vec<serde_json::Value>,
    files_touched: Vec<String>,
    transcript: Vec<serde_json::Value>,
    provider: CostSnapshot,
) -> SubagentExecution {
    SubagentExecution {
        status: ToolStatus::Success,
        summary: "ok".to_string(),
        status_label: "completed",
        error: None,
        metrics: TurnMetrics {
            provider,
            ..TurnMetrics::default()
        },
        supporting_receipts,
        model: "test-model".to_string(),
        structured_output: None,
        files_touched,
        transcript,
    }
}

#[test]
fn subagent_tool_call_path_extracts_known_tool_args() {
    assert_eq!(
        subagent_tool_call_path(&ToolCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "src/lib.rs"}),
        }),
        Some("src/lib.rs".to_string())
    );
    assert_eq!(
        subagent_tool_call_path(&ToolCall {
            call_id: "c2".to_string(),
            name: "apply_patch".to_string(),
            arguments: json!({"patches": [{"path": "a.rs"}, {"path": "b.rs"}]}),
        }),
        Some("a.rs".to_string())
    );
    assert_eq!(
        subagent_tool_call_path(&ToolCall {
            call_id: "c3".to_string(),
            name: "shell".to_string(),
            arguments: json!({"command": "ls"}),
        }),
        None
    );
}

#[test]
fn collect_files_touched_dedupes_and_drops_denied_and_pathless() {
    let receipts = vec![
        json!({"tool": "read_file", "status": "success", "path": "src/lib.rs"}),
        json!({"tool": "read_file", "status": "success", "path": "src/lib.rs"}),
        json!({"tool": "read_file", "status": "success", "path": "src/main.rs"}),
        json!({"tool": "read_file", "status": "denied", "path": "secret.env"}),
        json!({"tool": "shell", "status": "success"}),
        json!({"tool": "grep", "status": "success"}),
    ];
    assert_eq!(
        collect_files_touched(&receipts),
        vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
    );
}

#[test]
fn subagent_result_contains_files_touched() {
    let call = ToolCall {
        call_id: "del_1".to_string(),
        name: "delegate".to_string(),
        arguments: json!({"prompt": "investigate"}),
    };
    let supporting_receipts = vec![
        json!({"tool": "read_file", "status": "success", "path": "crates/a.rs"}),
        json!({"tool": "read_file", "status": "success", "path": "crates/b.rs"}),
    ];
    let execution = make_subagent_execution(
        supporting_receipts,
        vec!["crates/a.rs".to_string(), "crates/b.rs".to_string()],
        Vec::new(),
        CostSnapshot::default(),
    );
    let result = subagent_control_result(&call, SubagentKind::Delegate, execution);
    let files = result
        .content
        .get("files_touched")
        .expect("files_touched key");
    let files = files.as_array().expect("files_touched is an array");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0], "crates/a.rs");
    assert_eq!(files[1], "crates/b.rs");
}

#[test]
fn subagent_result_includes_cache_breakdown() {
    let call = ToolCall {
        call_id: "del_2".to_string(),
        name: "delegate".to_string(),
        arguments: json!({"prompt": "with cache"}),
    };
    let provider = CostSnapshot {
        input_tokens: Some(1_000),
        output_tokens: Some(120),
        cached_input_tokens: Some(640),
        cache_write_input_tokens: Some(360),
        ..CostSnapshot::default()
    };
    let execution = make_subagent_execution(Vec::new(), Vec::new(), Vec::new(), provider);
    let result = subagent_control_result(&call, SubagentKind::Delegate, execution);
    let cache = result.content.get("cache").expect("cache key");
    assert_eq!(cache.get("input_tokens"), Some(&json!(1_000)));
    assert_eq!(cache.get("output_tokens"), Some(&json!(120)));
    assert_eq!(cache.get("cached_input_tokens"), Some(&json!(640)));
    assert_eq!(cache.get("cache_write_input_tokens"), Some(&json!(360)));
}

#[test]
fn subagent_result_omits_transcript_by_default() {
    let call = ToolCall {
        call_id: "del_3".to_string(),
        name: "delegate".to_string(),
        arguments: json!({"prompt": "no transcript"}),
    };
    let execution =
        make_subagent_execution(Vec::new(), Vec::new(), Vec::new(), CostSnapshot::default());
    let result = subagent_control_result(&call, SubagentKind::Delegate, execution);
    assert!(
        result.content.get("transcript").is_none(),
        "transcript leaked into default result: {:?}",
        result.content
    );
}

#[test]
fn subagent_result_includes_transcript_when_debug_enabled() {
    let call = ToolCall {
        call_id: "del_4".to_string(),
        name: "delegate".to_string(),
        arguments: json!({"prompt": "with transcript"}),
    };
    let transcript = vec![
        json!({"role": "user", "text": "investigate"}),
        json!({"role": "assistant", "text": "looking..."}),
    ];
    let execution = make_subagent_execution(
        Vec::new(),
        Vec::new(),
        transcript.clone(),
        CostSnapshot::default(),
    );
    let result = subagent_control_result(&call, SubagentKind::Delegate, execution);
    let recorded = result
        .content
        .get("transcript")
        .expect("transcript key when populated");
    let recorded = recorded.as_array().expect("transcript is an array");
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].get("role"), Some(&json!("user")));
    assert_eq!(recorded[1].get("role"), Some(&json!("assistant")));
}

#[test]
fn subagent_transcript_serializes_conversation_items() {
    use squeezy_llm::LlmInputItem;
    let conversation = vec![
        LlmInputItem::UserText("hello".to_string()),
        LlmInputItem::FunctionCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "x"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "c1".to_string(),
            output: "ok".to_string(),
        },
    ];
    let transcript = subagent_transcript(&conversation);
    assert_eq!(transcript.len(), 3);
    assert_eq!(transcript[0].get("role"), Some(&json!("user")));
    assert_eq!(transcript[0].get("text"), Some(&json!("hello")));
    assert_eq!(transcript[1].get("role"), Some(&json!("tool_call")));
    assert_eq!(transcript[1].get("name"), Some(&json!("read_file")));
    assert_eq!(transcript[2].get("role"), Some(&json!("tool_result")));
}

#[test]
fn subagent_config_include_transcript_defaults_false() {
    let config = SubagentConfig::default();
    assert!(!config.include_transcript);
}

#[test]
fn assistant_text_has_unresolved_intent_detects_let_me_scan() {
    assert!(assistant_text_has_unresolved_intent(
        "Let me scan the codebase to find a good candidate.",
    ));
}

#[test]
fn assistant_text_has_unresolved_intent_detects_ill_with_action() {
    assert!(assistant_text_has_unresolved_intent(
        "I'll read src/lib.rs and then we'll see what to do.",
    ));
}

#[test]
fn assistant_text_has_unresolved_intent_skips_chitchat() {
    assert!(!assistant_text_has_unresolved_intent(
        "I'm doing well, thanks for asking. What can I help you with?",
    ));
}

#[test]
fn assistant_text_has_unresolved_intent_skips_final_answer() {
    // Intent phrase present but the model is signaling end of work.
    assert!(!assistant_text_has_unresolved_intent(
        "Let me summarize. In summary: the bug is in lib.rs.",
    ));
}

#[test]
fn assistant_text_has_unresolved_intent_skips_proposed_plan_block() {
    // Plan-mode legitimate finish_reason=stop.
    let text = "I'll start with the planner.\n<proposed_plan>\n## Context\nfoo\n</proposed_plan>";
    assert!(!assistant_text_has_unresolved_intent(text));
}

#[test]
fn assistant_text_has_unresolved_intent_skips_empty() {
    assert!(!assistant_text_has_unresolved_intent(""));
    assert!(!assistant_text_has_unresolved_intent("   \n\n"));
}

#[test]
fn assistant_text_has_unresolved_intent_skips_intent_without_action_verb() {
    // Phrase like "let me think" without a tool verb shouldn't fire.
    assert!(!assistant_text_has_unresolved_intent(
        "Let me think about this. The answer depends on what you mean by X.",
    ));
}

// F17-dispatch-command-completeness: each typed `DispatchCommand`
// variant lands in `Agent::dispatch_command` with a deterministic
// outcome. Variants whose effect lives in the TUI return `TuiOnly`;
// agent-side variants return the structured outcome the caller
// (eval / RPC) needs.

fn mock_agent_for_dispatch() -> Agent {
    let provider = Arc::new(MockProvider::new(Vec::new()));
    Agent::new(AppConfig::default(), provider)
}

#[tokio::test]
async fn dispatch_command_mode_switches() {
    let agent = mock_agent_for_dispatch();
    let outcome = agent
        .dispatch_command(DispatchCommand::Plan { prompt: None })
        .await;
    assert!(matches!(
        outcome,
        DispatchOutcome::ModeChanged { ref mode, changed: true, .. } if mode == "plan"
    ));
    // Repeating the call is a no-op: changed=false.
    let outcome = agent
        .dispatch_command(DispatchCommand::Plan { prompt: None })
        .await;
    assert!(matches!(
        outcome,
        DispatchOutcome::ModeChanged { ref mode, changed: false, .. } if mode == "plan"
    ));
    let outcome = agent
        .dispatch_command(DispatchCommand::Build { prompt: None })
        .await;
    assert!(matches!(
        outcome,
        DispatchOutcome::ModeChanged { ref mode, changed: true, .. } if mode == "build"
    ));
}

#[tokio::test]
async fn dispatch_command_plan_with_prompt_surfaces_prompt_in_outcome() {
    // squeezy-9n9w (audit B3): the agent dispatch path used to discard
    // the trailing prompt on `/plan <prompt>` / `/build <prompt>`, so
    // non-TUI callers (RPC, squeezy-eval) silently lost the user's
    // intent. The outcome now carries the prompt through.
    let agent = mock_agent_for_dispatch();
    let outcome = agent
        .dispatch_command(DispatchCommand::Plan {
            prompt: Some("analyze the changes since main".into()),
        })
        .await;
    let DispatchOutcome::ModeChanged {
        ref mode,
        changed: true,
        ref prompt,
    } = outcome
    else {
        panic!("expected ModeChanged, got {outcome:?}");
    };
    assert_eq!(mode, "plan");
    assert_eq!(prompt.as_deref(), Some("analyze the changes since main"));
}

#[tokio::test]
async fn dispatch_command_cost_and_context() {
    let agent = mock_agent_for_dispatch();
    let cost = agent.dispatch_command(DispatchCommand::Cost).await;
    assert!(matches!(cost, DispatchOutcome::CostSnapshot { .. }));
    let ctx = agent.dispatch_command(DispatchCommand::Context).await;
    assert!(matches!(ctx, DispatchOutcome::ContextSnapshot { .. }));
}

#[tokio::test]
async fn dispatch_command_jobs_permissions_reviewer_snapshots_are_empty_by_default() {
    let agent = mock_agent_for_dispatch();
    let jobs = agent.dispatch_command(DispatchCommand::Tasks).await;
    assert!(matches!(jobs, DispatchOutcome::JobsList { count: 0 }));
    let perms = agent.dispatch_command(DispatchCommand::Permissions).await;
    assert!(matches!(
        perms,
        DispatchOutcome::PermissionsList { count: 0 }
    ));
    let reviewer = agent.dispatch_command(DispatchCommand::Reviewer).await;
    assert!(matches!(
        reviewer,
        DispatchOutcome::ReviewerSnapshot { count: 0 }
    ));
}

#[tokio::test]
async fn dispatch_command_task_lookup_and_cancel_for_missing_id() {
    let agent = mock_agent_for_dispatch();
    let detail = agent
        .dispatch_command(DispatchCommand::Task {
            id: "99".to_string(),
        })
        .await;
    assert!(matches!(
        detail,
        DispatchOutcome::TaskDetail { ref id, found: false } if id == "99"
    ));
    let cancel = agent
        .dispatch_command(DispatchCommand::TaskCancel {
            id: "99".to_string(),
        })
        .await;
    assert!(matches!(
        cancel,
        DispatchOutcome::TaskCancel { ref id, cancelled: false } if id == "99"
    ));
}

#[tokio::test]
async fn dispatch_command_attachments_default_to_empty() {
    let agent = mock_agent_for_dispatch();
    let attachments = agent.dispatch_command(DispatchCommand::Attachments).await;
    assert!(matches!(
        attachments,
        DispatchOutcome::AttachmentsList { count: 0 }
    ));
    let pins = agent.dispatch_command(DispatchCommand::Pins).await;
    assert!(matches!(pins, DispatchOutcome::PinsList { count: 0 }));
}

#[tokio::test]
async fn dispatch_command_unpin_missing_returns_error() {
    let agent = mock_agent_for_dispatch();
    let outcome = agent
        .dispatch_command(DispatchCommand::Unpin {
            id: "pin-missing".to_string(),
        })
        .await;
    assert!(matches!(outcome, DispatchOutcome::Error { ref command, .. } if command == "/unpin"));
}

#[tokio::test]
async fn dispatch_command_attach_path_propagates_error() {
    let agent = mock_agent_for_dispatch();
    let outcome = agent
        .dispatch_command(DispatchCommand::Attach {
            path: "/path/that/does/not/exist".to_string(),
        })
        .await;
    assert!(matches!(outcome, DispatchOutcome::Error { ref command, .. } if command == "/attach"));
}

#[tokio::test]
async fn dispatch_command_session_lookup_for_missing_id() {
    let agent = mock_agent_for_dispatch();
    let outcome = agent
        .dispatch_command(DispatchCommand::Session {
            id: "missing".to_string(),
        })
        .await;
    assert!(matches!(
        outcome,
        DispatchOutcome::SessionDetail { ref session_id, exists: false } if session_id == "missing"
    ));
}

#[tokio::test]
async fn dispatch_command_tui_only_for_renderer_owned_commands() {
    // `/diff` and `/undo` deliberately omitted: both now resolve to
    // typed `DispatchOutcome::DiffSnapshot` / `CheckpointUndo`
    // outcomes so non-TUI drivers (eval / RPC) can audit them. See
    // `dispatch_command_diff_returns_typed_snapshot` and
    // `dispatch_command_undo_returns_typed_result` below.
    let agent = mock_agent_for_dispatch();
    let cases: &[(DispatchCommand, &str)] = &[
        (DispatchCommand::Keymap, "keymap"),
        (DispatchCommand::Statusline, "statusline"),
        (DispatchCommand::Help { topic: None }, "help"),
        (DispatchCommand::Config { section: None }, "options"),
        (DispatchCommand::Model, "model"),
        (
            DispatchCommand::Plans {
                args: String::new(),
            },
            "plans",
        ),
        (DispatchCommand::Copy { target: None }, "copy"),
        (DispatchCommand::Collapse { category: None }, "collapse"),
        (DispatchCommand::Expand { category: None }, "expand"),
        (
            DispatchCommand::Feedback {
                args: String::new(),
            },
            "feedback",
        ),
        (
            DispatchCommand::Report {
                args: String::new(),
            },
            "report",
        ),
        (DispatchCommand::Effort { value: None }, "effort"),
        (DispatchCommand::Verbosity { value: None }, "verbosity"),
        (
            DispatchCommand::ToolVerbosity { value: None },
            "tool-verbosity",
        ),
        (
            DispatchCommand::Theme {
                theme: "dark".to_string(),
            },
            "theme",
        ),
        (DispatchCommand::Fork, "fork"),
        (
            DispatchCommand::Resume {
                id: "sess".to_string(),
            },
            "resume",
        ),
        (DispatchCommand::Checkpoints, "checkpoints"),
        (
            DispatchCommand::Checkpoint {
                id: "ck".to_string(),
            },
            "checkpoint",
        ),
        (
            DispatchCommand::RevertTurn {
                group_id: "t".to_string(),
            },
            "revert-turn",
        ),
        (
            DispatchCommand::SessionExportHtml {
                id: "s".to_string(),
                path: None,
            },
            "session-export-html",
        ),
        (
            DispatchCommand::SessionCleanup {
                args: String::new(),
            },
            "session-cleanup",
        ),
        (DispatchCommand::Pin { target: None }, "pin"),
    ];
    for (cmd, expected_kind) in cases {
        let outcome = agent.dispatch_command(cmd.clone()).await;
        match outcome {
            DispatchOutcome::TuiOnly { command } => {
                assert_eq!(command, *expected_kind, "TuiOnly kind mismatch for {cmd:?}");
            }
            other => panic!("expected TuiOnly for {cmd:?}, got {other:?}"),
        }
    }
}

/// `/diff` returns a typed `DispatchOutcome::DiffSnapshot` payload
/// so headless drivers (eval / RPC) can audit the same diff the TUI
/// renders into a card. The mock agent runs against
/// `AppConfig::default()`, so `vcs_kind` is `"git"` when the test
/// host is a git checkout and `"none"` otherwise; the variant shape
/// is the same either way.
#[tokio::test]
async fn dispatch_command_diff_returns_typed_snapshot() {
    let agent = mock_agent_for_dispatch();
    let outcome = agent.dispatch_command(DispatchCommand::Diff).await;
    match outcome {
        DispatchOutcome::DiffSnapshot {
            vcs_kind,
            files_changed,
            additions,
            deletions,
            untracked_files,
            snapshot,
        } => {
            // `vcs_kind` is the only serializable summary the eval
            // driver records up front; it must be one of the two
            // tags the `VcsKind` enum serializes to.
            assert!(
                vcs_kind == "git" || vcs_kind == "none",
                "vcs_kind must be git|none, got {vcs_kind}"
            );
            // The lifted summary counters must mirror the boxed
            // snapshot exactly — the eval driver formats them
            // without unboxing the full `DiffSnapshot`.
            assert_eq!(files_changed, snapshot.summary.files_changed);
            assert_eq!(additions, snapshot.summary.additions);
            assert_eq!(deletions, snapshot.summary.deletions);
            assert_eq!(untracked_files, snapshot.summary.untracked_files);
        }
        other => panic!("expected DiffSnapshot for /diff, got {other:?}"),
    }
}

/// `/undo` returns a typed `DispatchOutcome::CheckpointUndo`
/// payload, not `TuiOnly`. The default mock agent has no
/// `CheckpointStore` wired up (checkpoints disabled), so `result`
/// is `None` and the driver records a structured "nothing to undo
/// — checkpoints disabled" signal. When checkpoints are enabled but
/// the journal is empty, `result` is `Some(_)` with
/// `applied=false, skipped=true`. Both shapes are valid eval
/// evidence — the test asserts the typed variant only.
#[tokio::test]
async fn dispatch_command_undo_returns_typed_result() {
    let agent = mock_agent_for_dispatch();
    let outcome = agent.dispatch_command(DispatchCommand::Undo).await;
    match outcome {
        DispatchOutcome::CheckpointUndo {
            applied,
            skipped,
            checkpoint_ids: _,
            result,
        } => {
            // No rollback can have applied: the test runs against
            // a fresh registry whose checkpoint journal is either
            // disabled (`result = None`) or empty
            // (`result = Some(_), skipped = true`). Either way
            // `applied` must be false.
            assert!(!applied, "no rollback should have applied");
            match result {
                None => assert!(
                    skipped,
                    "disabled-checkpoint path must surface skipped=true"
                ),
                Some(rollback) => {
                    assert!(
                        rollback.skipped && !rollback.applied,
                        "empty-journal rollback must report skipped=true, applied=false"
                    );
                    assert!(
                        rollback.checkpoint_ids.is_empty(),
                        "empty-journal rollback must report no checkpoint ids"
                    );
                }
            }
        }
        other => panic!("expected CheckpointUndo for /undo, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_command_raw_routes_through_parser() {
    let agent = mock_agent_for_dispatch();
    let plan = agent.dispatch_command_raw("/plan").await;
    assert!(matches!(
        plan,
        DispatchOutcome::ModeChanged { ref mode, changed: true, .. } if mode == "plan"
    ));
    let unknown = agent.dispatch_command_raw("/no-such-command").await;
    assert!(matches!(
        unknown,
        DispatchOutcome::Unsupported { ref command } if command == "/no-such-command"
    ));
    let attach = agent.dispatch_command_raw("/attach").await;
    assert!(matches!(
        attach,
        DispatchOutcome::Error { ref command, .. } if command == "/attach"
    ));
}

fn completed_turn_response(text: &str, response_id: &str) -> Vec<Result<LlmEvent>> {
    vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(text.to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some(response_id.to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]
}

#[tokio::test]
async fn next_turn_dispatches_through_start_turn() {
    // `next_turn` is the typed entry point for "start a fresh user
    // turn". It must run the full LLM-turn loop and surface the same
    // event stream as `start_turn`, with the supplied input reaching
    // the provider as a `UserText` item.
    let provider = Arc::new(MockProvider::new(vec![completed_turn_response(
        "ok",
        "resp_next",
    )]));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.next_turn("kick off a new turn".to_string(), CancellationToken::new());
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
        Some(("ok".to_string(), Some("resp_next".to_string()))),
        "next_turn should drive the LLM loop to completion just like start_turn"
    );
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        1,
        "next_turn should issue exactly one provider request"
    );
    assert!(
        requests[0].input.iter().any(|item| matches!(
            item,
            LlmInputItem::UserText(text) if text == "kick off a new turn"
        )),
        "next_turn input should reach the provider as a UserText item, got {:?}",
        requests[0].input
    );
}

#[tokio::test]
async fn follow_up_appends_user_text_without_running_a_turn() {
    // `follow_up` is the typed entry point for "extend the current
    // turn with another user message". It must push the text onto the
    // conversation queue (the same path as `queue_user_message`) and
    // it must NOT spawn a new turn — the provider should see zero
    // requests until a turn is started.
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    agent
        .follow_up("more context for the running turn".to_string())
        .await;

    let conversation = agent.conversation_state.lock().await.conversation.clone();
    assert_eq!(
        conversation.len(),
        1,
        "follow_up should push exactly one item onto the conversation queue"
    );
    assert!(
        matches!(
            &conversation[0],
            LlmInputItem::UserText(text) if text == "more context for the running turn"
        ),
        "follow_up should dispatch through the conversation-queue path as a UserText item, got {:?}",
        conversation[0]
    );
    assert!(
        provider.requests().is_empty(),
        "follow_up must not start a new turn"
    );
}

#[tokio::test]
async fn steer_aliases_next_turn_until_interrupt_semantics_land() {
    // `steer` is the typed entry point for "interrupt the running
    // turn with new input". The agent has no mid-turn-interrupt
    // primitive yet, so `steer` is documented as an alias for
    // `next_turn`: it must drive a fresh turn to completion and
    // surface the input to the provider exactly like `next_turn`
    // does, so call sites can adopt the typed name today and pick up
    // real interrupt semantics for free when they land.
    let provider = Arc::new(MockProvider::new(vec![completed_turn_response(
        "steered",
        "resp_steer",
    )]));
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut rx = agent.steer("change direction".to_string(), CancellationToken::new());
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
        Some(("steered".to_string(), Some("resp_steer".to_string()))),
        "steer should currently behave like next_turn and run a turn to completion"
    );
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        1,
        "steer should issue exactly one provider request via the next_turn path"
    );
    assert!(
        requests[0].input.iter().any(|item| matches!(
            item,
            LlmInputItem::UserText(text) if text == "change direction"
        )),
        "steer input should reach the provider as a UserText item, got {:?}",
        requests[0].input
    );
}

// F08-session-lifecycle-events-cancelable: `Agent::switch_session`
// consults the typed `AgentHookBus` before swapping the active
// session, and a `Decision::Deny` from any registered handler aborts
// the swap before `resume_current` runs. The two tests below pin both
// halves of that contract — allow proceeds to `resume_current`, which
// then fails with the synthetic id; deny short-circuits with the hook
// message and leaves the in-process session id untouched.

struct AllowSwitchHook;

impl squeezy_hooks::AgentHook for AllowSwitchHook {
    fn before_session_switch<'a>(
        &'a self,
        _target_id: &'a str,
    ) -> squeezy_hooks::HookFuture<'a, squeezy_hooks::Decision> {
        Box::pin(async { squeezy_hooks::Decision::Allow })
    }
}

struct DenySwitchHook {
    reason: &'static str,
}

impl squeezy_hooks::AgentHook for DenySwitchHook {
    fn before_session_switch<'a>(
        &'a self,
        _target_id: &'a str,
    ) -> squeezy_hooks::HookFuture<'a, squeezy_hooks::Decision> {
        let message = self.reason.to_string();
        Box::pin(async move { squeezy_hooks::Decision::Deny { message } })
    }
}

#[tokio::test]
async fn switch_session_allow_hook_proceeds_to_resume_current() {
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let mut agent = Agent::new(AppConfig::default(), provider);
    let mut bus = squeezy_hooks::AgentHookBus::new();
    bus.register(Box::new(AllowSwitchHook));
    agent.set_agent_hook_bus(Some(Arc::new(bus)));

    // The synthetic id has no on-disk session, so `resume_current`
    // will reject the swap once it actually runs. The exact disk-level
    // failure mode varies by environment (io::ErrorKind::NotFound, an
    // explicit "is not resumable" message, etc.), so the assertion
    // anchors on the *negative* shape that uniquely identifies the
    // hook deny path. If allow ever leaks the deny message we'd see
    // "denied by hook" instead of a resume-current error.
    let result = agent.switch_session("nonexistent-session-id-allow").await;
    let err = result.expect_err("nonexistent synthetic session must fail to resume");
    let msg = err.to_string();
    assert!(
        !msg.contains("denied by hook"),
        "allow hook must never short-circuit to a deny error: {msg}",
    );
    assert!(
        !msg.is_empty(),
        "resume_current must surface its disk-level failure, got empty error",
    );
}

#[tokio::test]
async fn switch_session_deny_hook_aborts_before_resume_current() {
    let provider = Arc::new(MockProvider::new(Vec::new()));
    let mut agent = Agent::new(AppConfig::default(), provider);
    let initial_session_id = agent.session_id();

    let mut bus = squeezy_hooks::AgentHookBus::new();
    bus.register(Box::new(DenySwitchHook {
        reason: "unsaved work",
    }));
    agent.set_agent_hook_bus(Some(Arc::new(bus)));

    let result = agent.switch_session("any-target-session-id").await;
    let err = result.expect_err("denying hook must abort the switch");
    let msg = err.to_string();
    assert!(
        msg.contains("denied by hook") && msg.contains("unsaved work"),
        "expected deny error from hook, got {msg}",
    );
    assert!(
        !msg.contains("not resumable"),
        "deny must short-circuit before resume_current touches disk: {msg}",
    );
    assert_eq!(
        agent.session_id(),
        initial_session_id,
        "deny must leave the in-process session id untouched",
    );
}
