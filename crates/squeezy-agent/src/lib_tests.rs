use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_core::Stream;
use futures_util::stream;
use serde_json::json;
use squeezy_core::{
    AppConfig, CompactionStrategy, ContextAttachmentKind, ContextCompactionConfig,
    ContextCompactionState, ContextCompactionTrigger, CostSnapshot, PermissionAction,
    PermissionCapability, PermissionMode, PermissionPolicy, PermissionRequest, PermissionRisk,
    PermissionRuleSource, ResponseVerbosity, Result, SessionLogConfig, SessionMode,
    ShellSandboxMode, SkillsConfig, SubagentConfig, TaskStateStatus,
};
use squeezy_llm::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, LlmToolSpec,
    StopReason,
};
use squeezy_tools::{ToolCall, ToolCostHint, ToolReceipt, ToolStatus, sha256_hex};
use tracing_subscriber::fmt::MakeWriter;

use super::*;

/// Agent turn + replay tests nest deep async state machines; debug builds on
/// Windows' smaller default thread stacks can overflow without a larger pool.
/// See `docs/internal/TEST_STACK_POSTURE.md` for the project posture and when
/// to use this 8 MiB helper vs. the 32 MiB `run_high_stack_test` in
/// `crates/squeezy-agent/tests/tool_loop.rs`.
fn run_high_stack_async_test(future: impl std::future::Future<Output = ()> + Send + 'static) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("build high-stack test runtime");
    runtime.block_on(async move {
        tokio::spawn(future)
            .await
            .expect("high-stack test task should not panic");
    });
}

struct MockProvider {
    name: &'static str,
    responses: Mutex<VecDeque<Vec<Result<LlmEvent>>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl MockProvider {
    fn new(responses: Vec<Vec<Result<LlmEvent>>>) -> Self {
        Self::named("mock", responses)
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

impl LlmProvider for MockProvider {
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

struct SteerInterruptProvider {
    requests: Mutex<Vec<LlmRequest>>,
}

impl SteerInterruptProvider {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl LlmProvider for SteerInterruptProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let mut requests = self.requests.lock().expect("requests");
        requests.push(request);
        let call_count = requests.len();
        drop(requests);

        if call_count == 1 {
            Box::pin(stream::pending())
        } else {
            let events = vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("replacement done".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_steered".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ];
            Box::pin(stream::iter(events))
        }
    }
}

struct DelayedFirstCancelProvider {
    requests: Mutex<Vec<LlmRequest>>,
    first_response_released: (Mutex<bool>, Condvar),
}

impl DelayedFirstCancelProvider {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            first_response_released: (Mutex::new(false), Condvar::new()),
        }
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().expect("requests").clone()
    }

    fn release_first_response(&self) {
        let (lock, cv) = &self.first_response_released;
        let mut released = lock.lock().expect("release lock");
        *released = true;
        cv.notify_all();
    }
}

impl LlmProvider for DelayedFirstCancelProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream_response(&self, request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let mut requests = self.requests.lock().expect("requests");
        requests.push(request);
        let call_count = requests.len();
        drop(requests);

        if call_count == 1 {
            let (lock, cv) = &self.first_response_released;
            let mut released = lock.lock().expect("release lock");
            while !*released {
                released = cv.wait(released).expect("release wait");
            }
            Box::pin(stream::pending())
        } else {
            let text = if call_count == 2 {
                "replacement done"
            } else {
                "after done"
            };
            let response_id = if call_count == 2 {
                "resp_replacement"
            } else {
                "resp_after"
            };
            Box::pin(stream::iter(completed_turn_response(text, response_id)))
        }
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
    let saw_cancelled = tokio::time::timeout(Duration::from_secs(1), async {
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
async fn terminal_stream_error_preserves_partial_assistant_text_in_conversation_state() {
    // Terminal provider-stream error (retries-exhausted idle timeout): the
    // model streams a few text deltas, then the stream yields a terminal
    // `ProviderStream` error instead of `Completed`/`Cancelled`. The turn
    // ends as `AgentEvent::Failed`, but — exactly like the cancel paths —
    // the partial assistant text already streamed to the TUI must be
    // preserved in `conversation_state.conversation`/`transcript` instead
    // of being silently dropped, so resume keeps what the model produced.
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("partial ".to_string())),
        Ok(LlmEvent::TextDelta("answer".to_string())),
        Err(SqueezyError::ProviderStream(
            "stream idle timeout".to_string(),
        )),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("answer me".to_string(), CancellationToken::new());
    let mut saw_failed = false;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::Failed { error, .. } = event {
            saw_failed = error.to_string().contains("stream idle timeout");
        }
    }
    assert!(
        saw_failed,
        "terminal stream error must still surface AgentEvent::Failed"
    );

    let state = agent.conversation_state.lock().await;
    let partial = "partial answer";

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
        "partial assistant text streamed before a terminal stream error must be pushed onto \
         `conversation_state.conversation`, not discarded like a hard failure"
    );

    let assistant = state
        .transcript
        .iter()
        .find(|item| item.role == squeezy_core::Role::Assistant)
        .expect("transcript must record the partial assistant turn");
    assert_eq!(assistant.content, partial);
}

#[test]
fn approval_context_excerpt_prefers_complete_bounded_text() {
    let excerpt = approval_context_excerpt(
        "I need to validate the Rust workspace before reporting back. Next I will run tests and inspect the result.",
    )
    .expect("excerpt");

    assert_eq!(
        excerpt,
        "I need to validate the Rust workspace before reporting back. Next I will run tests and inspect the result."
    );
    assert!(!excerpt.contains("..."));
    assert!(!excerpt.contains('…'));
}

#[test]
fn approval_context_excerpt_omits_unbounded_fragments() {
    let long_fragment = "word ".repeat(APPROVAL_CONTEXT_CAP + 20);

    assert_eq!(approval_context_excerpt(&long_fragment), None);
}

#[test]
fn approval_context_excerpt_uses_complete_boundary_before_cap() {
    let first_sentence = "I need to validate the Rust workspace before reporting back.";
    let long_tail = " Next I will keep explaining the same approval rationale".repeat(20);
    let input = format!("{first_sentence}{long_tail}");

    assert!(input.chars().count() > APPROVAL_CONTEXT_CAP);
    assert_eq!(
        approval_context_excerpt(&input).as_deref(),
        Some(first_sentence)
    );
}

#[test]
fn approval_context_from_request_uses_explicit_description() {
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "description".to_string(),
        "I need to validate the Rust workspace before reporting back. Then I will summarize the result."
            .to_string(),
    );
    let request = PermissionRequest {
        call_id: "call_1".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Shell,
        target: "cargo:*".to_string(),
        risk: PermissionRisk::Medium,
        summary: "run shell command".to_string(),
        metadata,
        suggested_rules: Vec::new(),
    };
    let redactor = Redactor::new(&Default::default()).expect("redactor");
    let context = approval_context_from_request(&request, &redactor).expect("approval context");

    assert_eq!(
        context,
        "I need to validate the Rust workspace before reporting back. Then I will summarize the result."
    );
    assert!(!context.contains("..."));
    assert!(!context.contains('…'));
}

#[tokio::test]
async fn approval_request_omits_unrelated_transcript_context() {
    let root = temp_workspace("agent_approval_unrelated_context");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::ToolCall(LlmToolCall {
            call_id: "call_find".to_string(),
            name: "shell".to_string(),
            arguments: json!({
                "command": "find . -name pom.xml",
            }),
        })),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_find".to_string()),
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
    {
        let mut state = agent.conversation_state.lock().await;
        state.transcript.push(TranscriptItem::assistant(
            "Hi! I'm Squeezy. How can I help?",
        ));
    }

    let mut rx = agent.start_turn("inspect project".to_string(), CancellationToken::new());
    let mut approval_context = Some("not captured".to_string());
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ApprovalRequested {
            request,
            decision_tx,
            ..
        } = event
        {
            approval_context = request.context;
            let _ = decision_tx.send(ToolApprovalDecision::Denied);
        }
    }

    assert_eq!(approval_context, None);
    let _ = fs::remove_dir_all(root);
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

#[test]
fn routing_session_disabled_persists_resume_snapshot() {
    let root = temp_workspace("routing-disabled-resume");
    let config = AppConfig {
        workspace_root: root,
        session_logs: SessionLogConfig {
            log_dir: Some(PathBuf::from(".squeezy/sessions")),
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let agent = Agent::new(config.clone(), Arc::new(MockProvider::new(Vec::new())));
    let session_id = agent.session_id().expect("session id");

    agent.set_routing_session_disabled(true);

    let store = squeezy_store::SessionStore::open(&config);
    let resume = store
        .open_session(session_id)
        .read_resume_state()
        .expect("resume state");
    assert!(resume.routing_session_disabled);
}

#[tokio::test]
async fn routing_session_disabled_persists_after_busy_state_lock() {
    let root = temp_workspace("routing-disabled-busy-resume");
    let config = AppConfig {
        workspace_root: root,
        session_logs: SessionLogConfig {
            log_dir: Some(PathBuf::from(".squeezy/sessions")),
            ..SessionLogConfig::default()
        },
        ..AppConfig::default()
    };
    let agent = Agent::new(config.clone(), Arc::new(MockProvider::new(Vec::new())));
    let session_id = agent.session_id().expect("session id");

    let guard = agent.conversation_state.lock().await;
    agent.set_routing_session_disabled(true);
    assert!(!guard.routing_session_disabled());
    drop(guard);

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if agent
                .conversation_state
                .lock()
                .await
                .routing_session_disabled()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background resume update");

    let store = squeezy_store::SessionStore::open(&config);
    let resume = store
        .open_session(session_id)
        .read_resume_state()
        .expect("resume state");
    assert!(resume.routing_session_disabled);
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
        context_compaction: ContextCompactionConfig {
            repo_doc_max_bytes: 0,
            user_memory_max_bytes: 0,
            ..ContextCompactionConfig::default()
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
async fn transient_turn_image_items_do_not_persist_as_context_attachments() {
    let provider = Arc::new(MockProvider::new(vec![vec![Ok(LlmEvent::Completed {
        response_id: Some("resp_1".to_string()),
        cost: CostSnapshot::default(),
        stop_reason: None,
        reasoning_only_stop: false,
    })]]));
    let agent = Agent::new(AppConfig::default(), provider.clone());
    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    png.extend_from_slice(b"inline-turn-image");

    let mut rx = agent.start_turn_with_display_input(
        "describe [Image shot.png]".to_string(),
        "describe [Image shot.png]".to_string(),
        vec![LlmInputItem::Image {
            media_type: "image/png".to_string(),
            bytes: Arc::from(png.clone().into_boxed_slice()),
        }],
        CancellationToken::new(),
        ResponseVerbosity::Normal,
    );
    let mut displayed_user = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::UserMessage { message, .. } = event {
            displayed_user = Some(message.content);
        }
    }

    assert_eq!(displayed_user.as_deref(), Some("describe [Image shot.png]"));
    assert!(agent.context_attachments_snapshot().await.is_empty());
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .input
            .iter()
            .any(|item| matches!(item, LlmInputItem::Image { .. })),
        "request should include transient image input"
    );
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
async fn refusal_event_surfaces_text_to_assistant_stream_and_message() {
    // OpenAI Responses streams safety-refusal prose on a dedicated
    // `LlmEvent::Refusal` channel. The main turn loop must route that text
    // through the assistant stream + `AssistantDelta` + completed message,
    // not drop it (F1) — otherwise the user only sees the generic
    // `StopReason::Refusal` failure with no explanation.
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::Refusal {
            content: "I can't help with that request.".to_string(),
        }),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_refusal".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut deltas: Vec<String> = Vec::new();
    let mut completed_message = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::AssistantDelta { delta, .. } => deltas.push(delta),
            AgentEvent::Completed { message, .. } => {
                completed_message = Some(message.content);
            }
            _ => {}
        }
    }

    let combined = deltas.join("");
    assert!(
        combined.contains("I can't help with that request."),
        "refusal text must reach the live assistant stream"
    );
    let message = completed_message.expect("completed");
    assert!(
        message.contains("I can't help with that request."),
        "refusal text must land in the persisted assistant message"
    );
    // Live deltas and the stored message agree, same invariant as ordinary
    // text deltas.
    assert_eq!(combined, message);
}

#[test]
fn document_resume_item_preserves_descriptive_placeholder() {
    // `ResumeItem` has no `Document` variant, so a checkpoint round-trip
    // can't carry the bytes. The catch-all used to flatten a fully-defined
    // document into an *empty* `UserText` (silent data loss, F2). Confirm
    // the explicit arm now emits a descriptive, non-empty placeholder that
    // names the attachment and its media type.
    let doc = LlmInputItem::document("application/pdf", "spec.pdf", vec![1u8, 2, 3]);
    let resume = llm_input_to_resume_item(doc);
    match resume {
        ResumeItem::UserText { text } => {
            assert!(!text.is_empty(), "document placeholder must not be empty");
            assert!(text.contains("spec.pdf"), "placeholder names the document");
            assert!(
                text.contains("application/pdf"),
                "placeholder names the media type"
            );
        }
        other => panic!("expected UserText placeholder, got {other:?}"),
    }
}

#[test]
fn content_parts_text_is_redacted_in_redact_input_item() {
    // No producer populates `content_parts` yet, but `redact_input_item`
    // must redact each text part defensively so a future structured
    // tool-result path can't slip a secret past the redactor (F5). Image
    // parts pass through untouched.
    let redactor = Redactor::default();
    let item = LlmInputItem::FunctionCallOutput {
        call_id: "call_1".to_string(),
        output: "ok".to_string(),
        content_parts: Some(vec![
            squeezy_llm::ToolResultPart::Text {
                text: "token sk-abcdefghijklmnopqrstuvwxyz".to_string(),
            },
            squeezy_llm::ToolResultPart::Image {
                media_type: "image/png".to_string(),
                bytes: Arc::from(vec![0u8, 1, 2].into_boxed_slice()),
            },
        ]),
        is_error: false,
    };
    let redacted = redact_input_item(item, &redactor);
    match redacted {
        LlmInputItem::FunctionCallOutput { content_parts, .. } => {
            let parts = content_parts.expect("content_parts retained");
            match &parts[0] {
                squeezy_llm::ToolResultPart::Text { text } => {
                    assert!(
                        !text.contains("sk-abcdefghijklmnopqrstuvwxyz"),
                        "secret in a text content part must be redacted"
                    );
                    assert!(text.contains("<redacted:"));
                }
                other => panic!("expected text part, got {other:?}"),
            }
            match &parts[1] {
                squeezy_llm::ToolResultPart::Image { bytes, .. } => {
                    assert_eq!(bytes.as_ref(), &[0u8, 1, 2], "image bytes pass through");
                }
                other => panic!("expected image part, got {other:?}"),
            }
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
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
fn subagent_request_carries_per_role_reasoning_effort() {
    use squeezy_core::ReasoningEffort;

    // The parent's global effort is deliberately *not* the per-role value so
    // the test proves the override fires rather than passing the inherited
    // setting straight through. `medium` is also distinct from both High and
    // Low so neither role's assertion can pass by accident.
    let mut config = AppConfig {
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string(),
        reasoning_effort: Some(ReasoningEffort::Medium),
        ..Default::default()
    };

    // Helper mirrors the real `run_subagent` wiring: apply the per-role
    // override to the inherited effort, then resolve the request field the
    // same way `run_subagent_rounds` builds the `LlmRequest`.
    let mut effective = |kind: SubagentKind, provider: &str| {
        config.reasoning_effort =
            subagent_role_reasoning_effort(kind, Some(ReasoningEffort::Medium));
        request_reasoning_effort(&config, provider)
    };

    // Reasoning-capable provider: each catalog role pins its own tier.
    assert_eq!(
        effective(SubagentKind::Plan, "openai"),
        Some(ReasoningEffort::High),
        "Planner subagent must reason hard"
    );
    assert_eq!(
        effective(SubagentKind::Explore, "openai"),
        Some(ReasoningEffort::Low),
        "Explorer subagent must stay cheap"
    );
    assert_eq!(
        effective(SubagentKind::Review, "openai"),
        Some(ReasoningEffort::Low),
        "Reviewer subagent must stay cheap"
    );

    // Kinds without a catalog role keep the parent's inherited global effort.
    assert_eq!(
        effective(SubagentKind::Delegate, "openai"),
        Some(ReasoningEffort::Medium),
        "Delegate keeps the inherited global effort"
    );

    // Non-reasoning provider: the role override sets the config field but the
    // downstream capability gate still drops it — behavior is unchanged from
    // the global path, for every role.
    for kind in [
        SubagentKind::Plan,
        SubagentKind::Explore,
        SubagentKind::Review,
        SubagentKind::Delegate,
    ] {
        assert_eq!(
            effective(kind, "anthropic"),
            None,
            "{} on a non-reasoning provider must not force reasoning_effort",
            kind.as_str()
        );
    }
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

#[test]
fn batch_tool_calls_hint_is_off_by_default_and_appends_when_enabled() {
    let base = "system rules";

    // Default (disabled): the system prompt is byte-for-byte unchanged so
    // the cache prefix is never disturbed unless the operator opts in.
    assert_eq!(instructions_with_batch_hint(base, false), base);

    // Enabled: the nudge is appended in a deterministic position and the
    // base prompt remains a verbatim prefix (cache-stable per session).
    let hinted = instructions_with_batch_hint(base, true);
    assert!(hinted.starts_with(base), "{hinted}");
    assert!(
        hinted.contains(BATCH_TOOL_CALLS_HINT),
        "enabled hint must contain the nudge text: {hinted}"
    );
    // The nudge steers only read-only lookups and explicitly keeps edits
    // sequential — the load-bearing safety property for G3.
    assert!(
        BATCH_TOOL_CALLS_HINT.contains("read-only") && BATCH_TOOL_CALLS_HINT.contains("sequential"),
        "nudge must scope to read-only and preserve edit ordering"
    );
}

#[tokio::test]
async fn parallel_tool_calls_config_flows_into_dispatched_request() {
    // G3 end-to-end: the operator's `[model].parallel_tool_calls` choice and
    // the batching nudge must reach the wire request the agent dispatches.
    // The default config leaves both untouched (None / no nudge); an opted-in
    // config carries `Some(true)` and appends the hint to the system prompt.
    for (parallel, hint) in [(None, false), (Some(true), true), (Some(false), false)] {
        let provider = Arc::new(MockProvider::new(vec![vec![Ok(LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        })]]));
        let config = AppConfig {
            parallel_tool_calls: parallel,
            batch_tool_calls_hint: hint,
            temperature: Some(0.2),
            top_p: Some(0.75),
            seed: Some(99),
            stop: vec!["END".to_string()],
            frequency_penalty: Some(0.1),
            presence_penalty: Some(-0.1),
            ..Default::default()
        };
        let agent = Agent::new(config, provider.clone());

        let mut rx = agent.start_turn("hello".to_string(), CancellationToken::new());
        while rx.recv().await.is_some() {}

        let requests = provider.requests();
        assert_eq!(requests.len(), 1, "exactly one request per turn");
        assert_eq!(
            requests[0].parallel_tool_calls, parallel,
            "request must carry the configured parallel_tool_calls={parallel:?}"
        );
        assert_eq!(requests[0].temperature, Some(0.2));
        assert_eq!(requests[0].top_p, Some(0.75));
        assert_eq!(requests[0].seed, Some(99));
        assert_eq!(requests[0].stop, vec!["END".to_string()]);
        assert_eq!(requests[0].frequency_penalty, Some(0.1));
        assert_eq!(requests[0].presence_penalty, Some(-0.1));
        let carries_hint = requests[0].instructions.contains(BATCH_TOOL_CALLS_HINT);
        assert_eq!(
            carries_hint, hint,
            "system prompt nudge presence must track batch_tool_calls_hint={hint}"
        );
    }
}

#[test]
fn tool_loop_executes_fallback_tool_and_returns_observation() {
    run_high_stack_async_test(async {
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
    });
}

#[test]
fn promised_action_retry_preserves_prior_visible_answer_in_transcript() {
    run_high_stack_async_test(async {
        let root = temp_workspace("agent_promised_action_retry_transcript");
        fs::write(root.join("sample.rs"), "fn marker() {}\n").expect("write sample");
        // A genuine stall: a substantive verdict followed by a trailing,
        // undelivered intent. The final clause is the unresolved action, so the
        // sharpened detector fires the retry — exercising the preservation path.
        let substantive_answer = "## Bug-by-Bug Verdict\nBug 1 confirmed. Bug 2 retracted.\n\nNow let me re-run each scenario directly to double-check the compacted summary.";
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "call_1".to_string(),
                    name: "grep".to_string(),
                    arguments: json!({"pattern": "marker", "include": ["*.rs"]}),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_1".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::ToolUse),
                    reasoning_only_stop: false,
                }),
            ],
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta(substantive_answer.to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_2".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::EndTurn),
                    reasoning_only_stop: false,
                }),
            ],
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta(
                    "The previous output is the complete answer. All scenarios were re-tested."
                        .to_string(),
                )),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_3".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::EndTurn),
                    reasoning_only_stop: false,
                }),
            ],
        ]));
        let config = AppConfig {
            workspace_root: root.clone(),
            session_logs: SessionLogConfig {
                log_dir: Some(PathBuf::from(".squeezy/sessions")),
                ..SessionLogConfig::default()
            },
            context_compaction: ContextCompactionConfig {
                repo_doc_max_bytes: 0,
                user_memory_max_bytes: 0,
                ..ContextCompactionConfig::default()
            },
            ..AppConfig::default()
        };
        let agent = Agent::new(config.clone(), provider.clone());

        let mut rx = agent.start_turn("revisit your reports".to_string(), CancellationToken::new());
        let mut completed = None;
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Completed { message, .. } = event {
                completed = Some(message.content);
            }
        }

        let completed = completed.expect("turn should complete");
        assert_eq!(
            completed, substantive_answer,
            "a substantive answer that triggered the one-shot retry must remain the visible turn output"
        );
        assert_eq!(
            provider.requests().len(),
            3,
            "the test must exercise the promised-action retry round"
        );

        let state = agent.conversation_state.lock().await;
        let assistant = state
            .transcript
            .iter()
            .find(|item| item.role == squeezy_core::Role::Assistant)
            .expect("transcript must persist the completed assistant turn");
        assert_eq!(
            assistant.content, substantive_answer,
            "resume/transcript state must not replace the answer with the retry acknowledgement"
        );

        let session_id = agent.session_id().expect("session id");
        let record = agent.show_session(&session_id).expect("session record");
        let retry_event = record
            .events
            .iter()
            .find(|event| event.kind == "assistant_retry")
            .expect("session events must record the retry decision");
        assert_eq!(retry_event.payload["branch"], "promised_action");
        assert_eq!(
            retry_event.payload["preserved_visible_chars"],
            json!(substantive_answer.chars().count()),
            "retry event must expose how much visible text was preserved"
        );

        let replay = record.replay.expect("replay tape");
        let retry_completion = replay
            .events
            .iter()
            .find(|event| {
                event.kind == SessionReplayEventKind::ModelCompleted
                    && event.payload["retry"]["branch"] == "promised_action"
            })
            .expect("replay model_completed must carry retry metadata");
        assert_eq!(retry_completion.payload["stop_reason"]["kind"], "end_turn");
        assert_eq!(
            retry_completion.payload["retry"]["preserved_visible_chars"],
            json!(substantive_answer.chars().count()),
        );

        let report = Agent::replay_tape(
            config,
            session_id,
            replay,
            "mock",
            AppConfig::default().model,
            SessionMode::default(),
        )
        .await
        .expect("retry session replay should consume the full tape");
        assert!(
            report.final_answer.contains(substantive_answer),
            "replay must retain the substantive answer, got {:?}",
            report.final_answer,
        );

        let _ = fs::remove_dir_all(root);
    });
}

#[test]
fn promised_action_retry_preserves_prior_visible_answer_on_terminal_failure() {
    run_high_stack_async_test(async {
        let root = temp_workspace("agent_promised_action_retry_terminal_failure");
        fs::write(root.join("sample.rs"), "fn marker() {}\n").expect("write sample");
        let substantive_answer = "The report was already complete.\n\nNow let me inspect the result directly to confirm.";
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "call_1".to_string(),
                    name: "grep".to_string(),
                    arguments: json!({"pattern": "marker", "include": ["*.rs"]}),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_1".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::ToolUse),
                    reasoning_only_stop: false,
                }),
            ],
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta(substantive_answer.to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_2".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::EndTurn),
                    reasoning_only_stop: false,
                }),
            ],
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_3".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::MaxTokens),
                    reasoning_only_stop: false,
                }),
            ],
        ]));
        let config = AppConfig {
            workspace_root: root.clone(),
            session_logs: SessionLogConfig {
                log_dir: Some(PathBuf::from(".squeezy/sessions")),
                ..SessionLogConfig::default()
            },
            ..AppConfig::default()
        };
        let agent = Agent::new(config, provider);

        let mut rx = agent.start_turn("revisit your reports".to_string(), CancellationToken::new());
        let mut failed = None;
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Failed { error, .. } = event {
                failed = Some(error.to_string());
            }
        }

        assert!(
            failed
                .as_deref()
                .is_some_and(|error| error.contains("max_tokens")),
            "turn should surface max_tokens failure, got {failed:?}",
        );
        let state = agent.conversation_state.lock().await;
        let assistant = state
            .transcript
            .iter()
            .find(|item| item.role == squeezy_core::Role::Assistant)
            .expect("failed turn transcript must preserve visible assistant text");
        assert_eq!(
            assistant.content, substantive_answer,
            "terminal failure after retry must not drop the answer the user already saw",
        );

        let session_id = agent.session_id().expect("session id");
        let record = agent.show_session(&session_id).expect("session record");
        let resume_state = record
            .resume_state
            .expect("terminal failure should write durable resume state");
        let durable_assistant = resume_state
            .transcript
            .iter()
            .find(|item| item.role == squeezy_core::Role::Assistant)
            .expect("resume_state transcript must preserve visible assistant text");
        assert_eq!(
            durable_assistant.content, substantive_answer,
            "terminal failure must persist preserved text to resume_state.json",
        );

        let _ = fs::remove_dir_all(root);
    });
}

#[test]
fn promised_action_retry_preserves_prior_visible_answer_on_soft_completion() {
    run_high_stack_async_test(async {
        let root = temp_workspace("agent_promised_action_retry_soft_completion");
        let bad_args = json!({
            INVALID_TOOL_ARGUMENTS_KEY: true,
            INVALID_TOOL_ARGUMENTS_ERROR_KEY: "EOF while parsing a string at line 1 column 59",
            INVALID_TOOL_ARGUMENTS_RAW_KEY: "{\"query\":\"getFoo",
        });
        let substantive_answer =
            "The useful answer is already here.\n\nNow let me inspect the failed lookup directly.";
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "call_bad_1".to_string(),
                    name: "definition_search".to_string(),
                    arguments: bad_args.clone(),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_1".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::ToolUse),
                    reasoning_only_stop: false,
                }),
            ],
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta(substantive_answer.to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_2".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::EndTurn),
                    reasoning_only_stop: false,
                }),
            ],
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "call_bad_2".to_string(),
                    name: "definition_search".to_string(),
                    arguments: bad_args,
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_3".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::ToolUse),
                    reasoning_only_stop: false,
                }),
            ],
        ]));
        let agent = Agent::new(
            AppConfig {
                workspace_root: root.clone(),
                context_compaction: ContextCompactionConfig {
                    repo_doc_max_bytes: 0,
                    user_memory_max_bytes: 0,
                    ..ContextCompactionConfig::default()
                },
                ..AppConfig::default()
            },
            provider,
        );

        let mut rx = agent.start_turn("find getFoo".to_string(), CancellationToken::new());
        let mut completed = None;
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Completed { message, .. } = event {
                completed = Some(message.content);
            }
        }

        let completed = completed.expect("turn should soft-complete");
        assert!(
            completed.starts_with(substantive_answer),
            "soft completion must preserve the answer the user already saw, got {completed:?}",
        );
        assert!(
            completed.contains("stopped early: repeated definition_search failure"),
            "soft completion should explain the loop guard, got {completed:?}",
        );
        let state = agent.conversation_state.lock().await;
        let assistant = state
            .transcript
            .iter()
            .find(|item| item.role == squeezy_core::Role::Assistant)
            .expect("soft-completed transcript must include assistant");
        assert!(
            assistant.content.starts_with(substantive_answer),
            "soft completion transcript must preserve retried visible text",
        );

        let _ = fs::remove_dir_all(root);
    });
}

#[tokio::test]
async fn server_model_echo_drives_cost_estimation() {
    let usage = CostSnapshot {
        input_tokens: Some(1_000_000),
        output_tokens: Some(0),
        reasoning_output_tokens: None,
        cached_input_tokens: None,
        cache_write_input_tokens: None,
        estimated_usd_micros: None,
    };
    assert_eq!(
        squeezy_llm::estimate_cost("openai", "gpt-5.4-nano", &usage),
        Some(200_000),
        "fixture should price to the server model's known OpenAI rate"
    );
    let provider = Arc::new(MockProvider::named(
        "openai",
        vec![vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ServerModel("gpt-5.4-nano".to_string())),
            Ok(LlmEvent::TextDelta("priced by server model".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_server_model".to_string()),
                cost: usage,
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ]],
    ));
    let config = AppConfig {
        model: "gpt-5.5".to_string(),
        routing: squeezy_core::RoutingConfig {
            enabled: false,
            ..AppConfig::default().routing
        },
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider);
    assert_eq!(agent.provider.name(), "openai");

    let mut rx = agent.start_turn("hi".to_string(), CancellationToken::new());
    let mut completed_cost = None;
    let mut failed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Completed { cost, .. } => {
                completed_cost = Some(cost);
            }
            AgentEvent::Failed { error, .. } => {
                failed = Some(error.to_string());
            }
            _ => {}
        }
    }

    assert!(failed.is_none(), "turn should complete, got: {failed:?}");
    let completed_cost = completed_cost.expect("turn should emit AgentEvent::Completed");
    assert_eq!(completed_cost.input_tokens, Some(1_000_000));
    assert_eq!(
        completed_cost.estimated_usd_micros,
        Some(200_000),
        "OpenAI gpt-5.4-nano input pricing should be used instead of requested gpt-5.5 pricing"
    );
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
            ai_reviewer: squeezy_core::AiReviewerConfig {
                enabled: false,
                ..Default::default()
            },
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
            ai_reviewer: squeezy_core::AiReviewerConfig {
                enabled: false,
                ..Default::default()
            },
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
async fn ai_reviewer_escalation_denial_reaches_user_prompt() {
    let root = temp_workspace("agent_ai_reviewer_escalation_denial");
    let doomed = root.join("created.txt");
    fs::write(&doomed, "keep\n").expect("write fixture");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "rm_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "rm -rf created.txt",
                    "description": "destructive shell"
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
            Ok(LlmEvent::TextDelta(
                r#"{"action":"deny","reason":"destructive capability requests are never auto-approved per policy; must escalate to human"}"#.to_string(),
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
            destructive: PermissionMode::Ask,
            ..Default::default()
        },
        ..Default::default()
    };
    config.permissions.ai_reviewer.enabled = true;
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("delete fixture".to_string(), CancellationToken::new());
    let mut approvals_seen = 0usize;
    let mut shell_result = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ApprovalRequested { decision_tx, .. } => {
                approvals_seen += 1;
                decision_tx
                    .send(ToolApprovalDecision::Denied)
                    .expect("send decision");
            }
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "rm_1" => {
                shell_result = Some(result);
            }
            _ => {}
        }
    }

    assert_eq!(approvals_seen, 1);
    assert!(doomed.exists(), "human denial must keep fixture intact");
    let shell_result = shell_result.expect("shell result");
    assert_eq!(shell_result.status, ToolStatus::Denied);
    assert!(
        shell_result.content["error"]
            .as_str()
            .is_some_and(|error| error.contains("user denied tool call")),
        "{:?}",
        shell_result.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_mode_forced_shell_ask_routes_through_ai_reviewer() {
    let root = temp_workspace("agent_plan_mode_ai_reviewer_shell");
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "shell_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf hi",
                    "description": "ambiguous non-mutating shell probe"
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
            Ok(LlmEvent::TextDelta(
                r#"{"action":"allow","reason":"non-mutating probe"}"#.to_string(),
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
        session_mode: SessionMode::Plan,
        permissions: PermissionPolicy {
            shell: PermissionMode::Ask,
            shell_sandbox: squeezy_core::ShellSandboxConfig {
                mode: ShellSandboxMode::Off,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    config.permissions.ai_reviewer.enabled = true;
    config.permissions.ai_reviewer.allow_capabilities = vec![PermissionCapability::Shell];
    let agent = Agent::new(config, provider.clone());

    let mut rx = agent.start_turn(
        "plan with shell probe".to_string(),
        CancellationToken::new(),
    );
    let mut approvals_seen = 0usize;
    let mut shell_result = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ApprovalRequested { decision_tx, .. } => {
                approvals_seen += 1;
                let _ = decision_tx.send(ToolApprovalDecision::Denied);
            }
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "shell_1" => {
                shell_result = Some(result);
            }
            _ => {}
        }
    }

    assert_eq!(approvals_seen, 0, "auto-review should handle Plan Mode Ask");
    assert_eq!(
        shell_result.expect("shell result").status,
        ToolStatus::Success
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        matches!(&requests[1].input[0], LlmInputItem::UserText(text) if text.contains("Approval policy") && text.contains("\"tool_name\":\"shell\"")),
        "reviewer prompt should carry the shell permission request: {:?}",
        requests[1].input
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
            ai_reviewer: squeezy_core::AiReviewerConfig {
                enabled: false,
                ..Default::default()
            },
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
async fn fork_mode_skills_render_in_fork_block_not_active_block() {
    let root = temp_workspace("agent_skill_fork_partition");
    let inline_dir = root.join(".agents/skills/inline-skill");
    fs::create_dir_all(&inline_dir).expect("mkdir inline");
    fs::write(
        inline_dir.join("SKILL.md"),
        "---\nname: inline-skill\ndescription: \"inline desc\"\ntriggers:\n  - inline phrase\n---\n# Inline Body\n",
    )
    .expect("write inline skill");
    let fork_dir = root.join(".agents/skills/fork-skill");
    fs::create_dir_all(&fork_dir).expect("mkdir fork");
    fs::write(
        fork_dir.join("SKILL.md"),
        "---\nname: fork-skill\ndescription: \"fork desc\"\ncontext: fork\ntriggers:\n  - fork phrase\n---\n# Fork Body\n",
    )
    .expect("write fork skill");

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
        "trigger inline phrase and fork phrase together".to_string(),
        CancellationToken::new(),
    );
    while rx.recv().await.is_some() {}

    let request = provider.requests().pop().expect("captured llm request");
    let instructions = &request.instructions;
    assert!(
        instructions.contains("<active_skills>"),
        "inline-mode skill must still render under <active_skills>: {instructions}"
    );
    assert!(
        instructions.contains("inline-skill"),
        "inline skill missing from instructions: {instructions}"
    );
    assert!(
        instructions.contains("<fork_skills>"),
        "fork-mode skill must render in a separate <fork_skills> block: {instructions}"
    );
    assert!(
        instructions.contains("context_mode=\"fork\""),
        "fork block must tag the skill with context_mode=\"fork\": {instructions}"
    );
    let active_segment = instructions
        .split("<active_skills>")
        .nth(1)
        .and_then(|tail| tail.split("</active_skills>").next())
        .unwrap_or("");
    assert!(
        !active_segment.contains("fork-skill"),
        "fork-mode skill leaked into <active_skills>: {active_segment}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn skill_manifest_missing_tool_deps_emit_warning_block() {
    let root = temp_workspace("agent_skill_tool_deps");
    let skill_dir = root.join(".agents/skills/needs-things");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: needs-things\ndescription: \"depends on absent tools\"\ntriggers:\n  - needs things\n---\n# body\n",
    )
    .expect("write skill md");
    fs::write(
        skill_dir.join("skill.toml"),
        "tool_deps = [\"mcp:nonexistent\", \"definitely_not_a_tool\"]\n",
    )
    .expect("write manifest");

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
        "needs things now please".to_string(),
        CancellationToken::new(),
    );
    while rx.recv().await.is_some() {}

    let request = provider.requests().pop().expect("captured request");
    assert!(
        request.instructions.contains("<skill_warnings>"),
        "missing skill_warnings block: {}",
        request.instructions
    );
    assert!(
        request.instructions.contains("needs-things"),
        "warning block must name the skill: {}",
        request.instructions
    );
    assert!(
        request.instructions.contains("mcp:nonexistent")
            && request.instructions.contains("definitely_not_a_tool"),
        "warning block must list each missing dep: {}",
        request.instructions
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn replace_config_rediscovers_skill_catalog() {
    let root = temp_workspace("agent_skill_reload");
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
    let initial_config = config_with_skill_dirs(&root);
    let mut agent = Agent::new(initial_config.clone(), provider.clone());

    // Drop a brand-new skill onto disk after the agent has been
    // built. Without `replace_config` rebuilding the catalog the next
    // turn would not see this file because `SkillCatalog::discover`
    // had already run.
    let skill_dir = root.join(".agents/skills/late-skill");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: late-skill\ndescription: \"added after init\"\ntriggers:\n  - late phrase\n---\n# Late\n",
    )
    .expect("write late skill");

    // Mutate the skills config (a no-op `[[skills.config]]` rule is
    // enough to change the value so `replace_config` rebuilds) and
    // hand it back to the agent the same way the TUI reload path
    // would.
    let mut next_config = initial_config;
    next_config
        .skills
        .config
        .push(squeezy_core::SkillConfigEntry {
            name: Some("late-skill".to_string()),
            path: None,
            enabled: true,
        });
    agent.replace_config(next_config);

    let mut rx = agent.start_turn("trigger late phrase".to_string(), CancellationToken::new());
    while rx.recv().await.is_some() {}

    let request = provider.requests().pop().expect("captured request");
    assert!(
        request.instructions.contains("late-skill"),
        "reloaded catalog must surface late-skill: {}",
        request.instructions
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn agent_skill_hooks_default_to_disabled() {
    let root = temp_workspace("agent_skill_hooks_off");
    let skill_dir = root.join(".agents/skills/validator");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: validator\ndescription: \"d\"\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"true\"\n---\n# validator\n",
    )
    .expect("write skill");

    let provider = Arc::new(MockProvider::new(Vec::new()));
    let config = config_with_skill_dirs(&root);
    assert!(
        !config.skills.hooks_enabled,
        "default config must keep skill hooks dormant"
    );
    let agent = Agent::new(config, provider);
    assert!(
        agent.hooks().is_none(),
        "skill hooks must stay off until [skills] hooks_enabled = true"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn agent_skill_hooks_register_when_enabled() {
    let root = temp_workspace("agent_skill_hooks_on");
    let skill_dir = root.join(".agents/skills/validator");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: validator\ndescription: \"d\"\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"true\"\n---\n# validator\n",
    )
    .expect("write skill");

    let provider = Arc::new(MockProvider::new(Vec::new()));
    let mut config = config_with_skill_dirs(&root);
    config.skills.hooks_enabled = true;
    let agent = Agent::new(config, provider);

    let registry = agent.hooks().expect("hooks registry installed");
    assert_eq!(registry.len(), 1, "one declared hook should be registered");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn replace_config_clears_hooks_when_hooks_enabled_toggled_off() {
    let root = temp_workspace("agent_skill_hooks_toggle_off");
    let skill_dir = root.join(".agents/skills/validator");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: validator\ndescription: \"d\"\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"true\"\n---\n# validator\n",
    )
    .expect("write skill");

    let provider = Arc::new(MockProvider::new(Vec::new()));
    let mut config = config_with_skill_dirs(&root);
    config.skills.hooks_enabled = true;
    let mut agent = Agent::new(config.clone(), provider);

    assert!(
        agent.hooks().is_some(),
        "hooks must be installed when hooks_enabled=true"
    );

    // Simulate hot-reload that disables the gate.
    let mut next = config;
    next.skills.hooks_enabled = false;
    // Trigger the skills_changed path by tweaking another skills field.
    next.skills.inline = true;
    agent.replace_config(next);

    assert!(
        agent.hooks().is_none(),
        "hooks must be cleared when hooks_enabled flipped to false via replace_config"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn replace_config_rebuilds_hooks_when_hooks_remain_enabled() {
    let root = temp_workspace("agent_skill_hooks_rebuild");
    let skill_dir = root.join(".agents/skills/validator");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: validator\ndescription: \"d\"\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"true\"\n---\n# validator\n",
    )
    .expect("write skill");

    let provider = Arc::new(MockProvider::new(Vec::new()));
    let mut config = config_with_skill_dirs(&root);
    config.skills.hooks_enabled = true;
    let mut agent = Agent::new(config.clone(), provider);

    let old_hook_count = agent.hooks().map(|r| r.len()).unwrap_or(0);
    assert_eq!(old_hook_count, 1);

    // Disable the skill via a config rule while hooks_enabled stays true.
    let mut next = config;
    next.skills.config.push(squeezy_core::SkillConfigEntry {
        name: Some("validator".to_string()),
        path: None,
        enabled: false,
    });
    agent.replace_config(next);

    // After the skill is disabled the hook should vanish.
    assert!(
        agent.hooks().is_none(),
        "disabling the skill via [[skills.config]] must clear its hook handlers"
    );

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
        request.instructions.contains("doc-help subagent"),
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
    // Unknown topics (no curated match) get the full corpus so DocHelp has
    // maximum coverage.  Both a providers-related doc and a sessions-related doc
    // must be present; neither is topic-specific for "quantum billing rules".
    assert!(
        user_prompt.contains("PATH: docs/external/AGENT_APPROACH.md"),
        "unknown-topic corpus must include AGENT_APPROACH.md: {user_prompt:?}"
    );
    assert!(
        user_prompt.contains("PATH: docs/external/PROVIDERS.md"),
        "unknown-topic corpus must include PROVIDERS.md (full corpus, not scoped): {user_prompt:?}"
    );
    assert!(
        user_prompt.contains("PATH: docs/external/SESSIONS.md"),
        "unknown-topic corpus must include SESSIONS.md (full corpus, not scoped): {user_prompt:?}"
    );

    let completed = completed.expect("help turn should complete");
    assert!(completed.contains("quantum-billing"), "{completed}");
    assert!(!completed.contains("No local help coverage"), "{completed}");
}

// NOTE: doc_help_subagent_scopes_corpus_to_matching_topic was removed because
// `/help <known-topic>` is always handled by the curated layer (Answered), so
// DocHelp never fires for known topics — the assertion `requests.len() == 1`
// was always false.  Corpus scoping is tested as a pure unit test in
// crates/squeezy-skills/src/help_tests.rs (relevant_docs_for_input_scopes_corpus).

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
fn non_success_tool_statuses_are_model_errors() {
    assert!(!tool_status_is_model_error(ToolStatus::Success));
    assert!(tool_status_is_model_error(ToolStatus::Error));
    assert!(tool_status_is_model_error(ToolStatus::Denied));
    assert!(tool_status_is_model_error(ToolStatus::Stale));
    assert!(tool_status_is_model_error(ToolStatus::Cancelled));
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

    let (call_a, result_a) = make_shell("shell-1", "cargo check -p sample-arch-graph");
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

    let (call_c, result_c) = make_shell("shell-3", "cargo check -p sample-arch-graph");
    let reason = guard
        .observe_round(&[call_c], &[result_c])
        .expect("genuine repeat of the same shell command should still stop");
    assert!(reason.contains("repeated shell failure"), "{reason}");
}

#[test]
fn tool_loop_guard_allows_webfetch_http_error_recovery_round() {
    let make_webfetch = |call_id: &str| {
        let call = ToolCall {
            call_id: call_id.to_string(),
            name: "webfetch".to_string(),
            arguments: json!({"url": "https://example.com/missing"}),
        };
        let mut result = control_tool_result(
            &call,
            ToolStatus::Error,
            json!({"error": "webfetch returned HTTP status 404"}),
        );
        result.tool_name = "webfetch".to_string();
        (call, result)
    };

    let (call_a, result_a) = make_webfetch("web-1");
    let (call_b, result_b) = make_webfetch("web-2");
    let (call_c, result_c) = make_webfetch("web-3");
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
        "a repeated external docs 404 should return to the model once so it can pivot"
    );
    let reason = guard
        .observe_round(&[call_c], &[result_c])
        .expect("third identical webfetch HTTP failure should still stop");
    assert!(reason.contains("repeated webfetch failure"), "{reason}");
}

#[test]
fn tool_loop_guard_still_stops_repeated_invalid_webfetch_arguments() {
    let make_webfetch = |call_id: &str| {
        let call = ToolCall {
            call_id: call_id.to_string(),
            name: "webfetch".to_string(),
            arguments: json!({}),
        };
        let mut result = control_tool_result(
            &call,
            ToolStatus::Error,
            json!({"error": "invalid tool arguments: missing field `url`"}),
        );
        result.tool_name = "webfetch".to_string();
        (call, result)
    };

    let (call_a, result_a) = make_webfetch("web-1");
    let (call_b, result_b) = make_webfetch("web-2");
    let mut guard = ToolLoopGuard::default();

    assert!(
        guard
            .observe_round(std::slice::from_ref(&call_a), &[result_a])
            .is_none()
    );
    let reason = guard
        .observe_round(&[call_b], &[result_b])
        .expect("repeated invalid arguments should keep the ordinary fail-fast threshold");
    assert!(reason.contains("repeated webfetch failure"), "{reason}");
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
    // "won't guess" was replaced with "No local help coverage" in the unsupported() message.
    assert!(completed.contains("No local help coverage"), "{completed}");
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
async fn unknown_explicit_skill_warns_and_continues_turn() {
    let root = temp_workspace("agent_skill_unknown_explicit");
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
        "/skill rust-nva inspect main".to_string(),
        CancellationToken::new(),
    );
    let mut saw_warning = false;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::SkillActivationWarning { name, message, .. } = event {
            saw_warning = true;
            assert_eq!(name, "rust-nva");
            assert!(message.contains("skill not found"), "{message}");
        }
    }

    assert!(saw_warning, "unknown explicit skill must surface a warning");
    let request = provider.requests().pop().expect("request");
    assert!(!request.instructions.contains("<active_skills>"));
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
                "command": "node build.js --release",
                "description": "run build script",
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
async fn destructive_pre_classifier_keeps_default_human_prompt() {
    let root = temp_workspace("agent_destructive_preclassifier_prompt");
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::ToolCall(LlmToolCall {
            call_id: "call_rm".to_string(),
            name: "shell".to_string(),
            arguments: json!({
                "command": "rm -rf /tmp/work",
                "description": "remove temp work"
            }),
        })),
        Ok(LlmEvent::Completed {
            response_id: Some("resp_rm".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            ..Default::default()
        },
        provider,
    );

    let mut rx = agent.start_turn("clean temp".to_string(), CancellationToken::new());
    let mut approval_seen = false;
    let mut denied_result = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ApprovalRequested {
                request,
                decision_tx,
                ..
            } => {
                approval_seen = true;
                assert_eq!(request.permission.metadata["command"], "rm -rf /tmp/work");
                assert_eq!(request.reason, "default destructive permission is ask");
                let _ = decision_tx.send(ToolApprovalDecision::Denied);
            }
            AgentEvent::ToolCallCompleted { result, .. } if result.call_id == "call_rm" => {
                denied_result = Some(result);
            }
            _ => {}
        }
    }

    assert!(approval_seen, "destructive shell must prompt before denial");
    let denied_result = denied_result.expect("shell result");
    assert_eq!(denied_result.status, ToolStatus::Denied);

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

/// The shell-classifier schema (M13) must mirror what
/// `extract_json_action` deserializes: the two permitted `action` values
/// (`ask`/`deny` — never `allow`) plus a `reason` string. Every
/// schema-valid document must parse back into the same non-`Allow` verdict.
#[test]
fn shell_classifier_output_schema_mirrors_parse_target() {
    let schema = super::shell_classifier_output_schema();
    assert!(schema.strict, "shell classifier schema must be strict");

    let action_enum = schema.schema["properties"]["action"]["enum"]
        .as_array()
        .expect("action carries an enum");
    let values: Vec<&str> = action_enum.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(
        values,
        vec![
            PermissionAction::Ask.as_str(),
            PermissionAction::Deny.as_str()
        ],
        "action enum excludes allow by design"
    );
    assert_eq!(
        schema.schema["properties"]["reason"]["type"],
        json!("string")
    );
    assert_eq!(schema.schema["required"], json!(["action", "reason"]));
    assert_eq!(schema.schema["additionalProperties"], json!(false));

    // Round-trip: every enum value parses back into the matching verdict.
    let deny = parse_classifier_verdict(r#"{"action":"deny","reason":"x"}"#);
    assert_eq!(deny.action, PermissionAction::Deny);
    let ask = parse_classifier_verdict(r#"{"action":"ask","reason":"x"}"#);
    assert_eq!(ask.action, PermissionAction::Ask);
}

/// Regression: when `shell_classifier` is enabled and a shell command
/// produces an `Ask` verdict, the out-of-band classifier LLM call's
/// billable cost must be folded into persisted session accounting
/// (`state.cost`, `state.metrics.provider`, and `state.metrics.model_ledger`)
/// — mirroring the AI-reviewer fold. Without this assertion, a future
/// refactor that drops the `merge_cost`/`record` lines in
/// `permission_decision_for_request` would silently regress to the
/// pre-PR behaviour where classifier spend went uncounted.
#[tokio::test]
async fn shell_classifier_cost_persists_to_session_accounting() {
    let root = temp_workspace("agent_shell_classifier_cost_fold");
    // Round 1: model emits an *ambiguous* non-mutating shell command
    // (`printf hi` — same probe the AI-reviewer test uses). With
    // `permissions.shell = Ask` and `shell_classifier = true`, the
    // verdict path runs the classifier LLM call before reaching the
    // user-approval prompt; the classifier returns `deny` so the tool
    // call is denied without us having to wire up an approval responder.
    // The command must (a) map to `PermissionCapability::Shell` and
    // (b) pre-classify as `AskAi` — picking a read-only command like
    // `ls -la` would auto-allow before the classifier ever runs.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_printf".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf hi",
                    "description": "ambiguous non-mutating shell probe",
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_round_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Classifier round: provider streams a deny verdict + non-zero
        // cost. This is what we expect to see folded into session
        // accounting.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                r#"{"action":"deny","reason":"too risky"}"#.to_string(),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_classifier".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(72),
                    output_tokens: Some(18),
                    estimated_usd_micros: Some(45_300),
                    ..CostSnapshot::default()
                },
                stop_reason: Some(StopReason::EndTurn),
                reasoning_only_stop: false,
            }),
        ],
        // Round 2: model receives the denied tool result and finishes.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("understood".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_round_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::EndTurn),
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            shell: PermissionMode::Ask,
            shell_classifier: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider);
    let mut rx = agent.start_turn("probe shell".to_string(), CancellationToken::new());
    while let Some(event) = rx.recv().await {
        if matches!(
            event,
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. }
        ) {
            break;
        }
    }
    let snapshot = agent.session_accounting_snapshot().await;
    // Round 1 + round 2 of the parent stream both carried zeroed
    // `CostSnapshot`s, so the only spend in `state.cost` after the turn
    // is the classifier round's 45_300 µUSD / 72 input / 18 output.
    assert_eq!(
        snapshot.cost.estimated_usd_micros,
        Some(45_300),
        "classifier-round cost must be folded into session cost",
    );
    assert_eq!(
        snapshot.cost.input_tokens,
        Some(72),
        "classifier-round input tokens must be folded into session cost",
    );
    assert_eq!(snapshot.cost.output_tokens, Some(18));
    assert_eq!(
        snapshot.metrics.provider.estimated_usd_micros,
        Some(45_300),
        "classifier-round cost must be folded into provider metrics",
    );

    let _ = fs::remove_dir_all(root);
}

/// Regression: the shell classifier returns `None` on `LlmEvent::Cancelled`,
/// which is treated as "no classification reached" by the caller — the
/// verdict therefore stays at the upstream `Ask`, and the permission path
/// falls back to the user-approval prompt rather than auto-resolving.
/// This locks in the cancellation contract documented on `ClassifierResult`.
#[tokio::test]
async fn shell_classifier_cancellation_falls_back_to_ask_verdict() {
    let root = temp_workspace("agent_shell_classifier_cancel");
    // `printf hi` so pre_classify_shell falls through to `AskAi` and the
    // classifier actually runs; a read-only command would auto-allow
    // before the classifier ever sees it.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "call_printf".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf hi",
                    "description": "ambiguous non-mutating shell probe",
                }),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_round_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        // Classifier round: mid-stream cancellation. Any partial cost is
        // dropped (bounded gap documented on `ClassifierResult`).
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("{\"action\":".to_string())),
            Ok(LlmEvent::Cancelled),
        ],
    ]));
    let config = AppConfig {
        workspace_root: root.clone(),
        permissions: PermissionPolicy {
            shell: PermissionMode::Ask,
            shell_classifier: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let agent = Agent::new(config, provider);
    let mut rx = agent.start_turn("probe shell".to_string(), CancellationToken::new());
    let mut approval_seen = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ApprovalRequested { decision_tx, .. } => {
                approval_seen = true;
                let _ = decision_tx.send(ToolApprovalDecision::Denied);
            }
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. } => break,
            _ => {}
        }
    }
    assert!(
        approval_seen,
        "classifier cancellation must fall back to the upstream Ask verdict, \
         which fires an ApprovalRequested event",
    );
    let snapshot = agent.session_accounting_snapshot().await;
    assert_eq!(
        snapshot.cost.estimated_usd_micros, None,
        "cancelled classifier must not write a partial cost row",
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn plan_mode_denies_repo_mutations_before_policy() {
    for capability in [
        PermissionCapability::Edit,
        PermissionCapability::Destructive,
    ] {
        let request = permission_request_for_capability(capability);
        let verdict = mode_permission_verdict(SessionMode::Plan, &request, None)
            .expect("plan mode should deny repo mutation capability");
        assert_eq!(verdict.action, PermissionAction::Deny);
        assert_eq!(verdict.matched_rule, None);
        assert!(verdict.reason.contains(capability.as_str()));
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
fn plan_mode_keeps_discovery_capabilities_on_normal_policy_path() {
    for capability in [
        PermissionCapability::Read,
        PermissionCapability::Search,
        PermissionCapability::Network,
        PermissionCapability::Mcp,
    ] {
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
    for request in [
        shell_permission_request(
            "rg --json pattern",
            PermissionCapability::Shell,
            PermissionRisk::Medium,
        ),
        shell_permission_request(
            "git status --short",
            PermissionCapability::Git,
            PermissionRisk::Low,
        ),
        shell_permission_request(
            "cargo test -p squeezy-agent",
            PermissionCapability::Compiler,
            PermissionRisk::Medium,
        ),
    ] {
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
fn plan_mode_shell_requests_must_be_proven_read_only() {
    for (command, capability) in [
        (
            "rg -l 'fn main' --type rust 2>/dev/null",
            PermissionCapability::Shell,
        ),
        (
            "find . -name \"*.java\" -not -path \"*/target/*\" | head -60",
            PermissionCapability::Shell,
        ),
        (
            "rg -l 'fn main' 2>/dev/null | python3 -c \"import sys; [print(l.strip()) for l in sys.stdin]\" 2>/dev/null || true",
            PermissionCapability::Shell,
        ),
        ("cargo fmt --check", PermissionCapability::Compiler),
        (
            "cargo test -p squeezy-agent",
            PermissionCapability::Compiler,
        ),
        ("git status --short", PermissionCapability::Git),
        ("git diff -- crates", PermissionCapability::Git),
    ] {
        let request = shell_permission_request(command, capability, PermissionRisk::Medium);
        assert_eq!(
            mode_permission_verdict(SessionMode::Plan, &request, None),
            None,
            "{command} should stay on the normal policy path"
        );
    }

    for (command, capability) in [
        ("cargo fmt", PermissionCapability::Compiler),
        ("cargo clippy --fix", PermissionCapability::Compiler),
        (
            "git diff --output=/private/tmp/sqz-pr364-diff-output-check origin/main...HEAD",
            PermissionCapability::Git,
        ),
        ("git checkout -b x", PermissionCapability::Git),
        ("git branch x", PermissionCapability::Git),
        ("echo $(touch created.txt)", PermissionCapability::Shell),
    ] {
        let request = shell_permission_request(command, capability, PermissionRisk::High);
        let verdict = mode_permission_verdict(SessionMode::Plan, &request, None)
            .expect("mutating shell command should be denied in plan mode");
        assert_eq!(verdict.action, PermissionAction::Deny);
        assert!(
            verdict.reason.contains("refuses mutating shell command"),
            "{command} denial reason should name shell mutation: {}",
            verdict.reason
        );
    }
}

#[test]
fn plan_mode_asks_for_ambiguous_shell_instead_of_denying() {
    let request = shell_permission_request(
        "node script.js",
        PermissionCapability::Shell,
        PermissionRisk::High,
    );
    let verdict = mode_permission_verdict(SessionMode::Plan, &request, None)
        .expect("ambiguous shell should require approval in plan mode");
    assert_eq!(verdict.action, PermissionAction::Ask);
    assert!(
        verdict.reason.contains("requires approval"),
        "approval reason should describe plan-mode shell uncertainty: {}",
        verdict.reason
    );
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
fn mode_state_snapshot_reports_live_mode_and_routing_overrides() {
    let agent = Agent::new(
        AppConfig {
            session_mode: SessionMode::Plan,
            ..Default::default()
        },
        Arc::new(MockProvider::new(Vec::new())),
    );

    assert_eq!(agent.mode_state_snapshot().session_mode, SessionMode::Plan);

    assert!(agent.set_session_mode(SessionMode::Build, "test"));
    agent.request_routing_force_cheap();
    agent.set_routing_session_disabled(true);

    let snapshot = agent.mode_state_snapshot();
    assert_eq!(snapshot.session_mode, SessionMode::Build);
    assert!(snapshot.routing_session_disabled);
    assert!(snapshot.pending_force_cheap);
    assert!(!snapshot.pending_force_parent);
    assert_eq!(snapshot.sticky_turns_remaining, 0);

    agent.request_routing_force_parent();
    let snapshot = agent.mode_state_snapshot();
    assert!(!snapshot.pending_force_cheap);
    assert!(snapshot.pending_force_parent);
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
            "shell",
            "symbol_context",
            "upstream_flow",
            "verify",
            "webfetch",
            "websearch",
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
        excluded: Vec::new(),
        ..squeezy_core::ToolSchemaConfig::default()
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
            destructive: PermissionMode::Deny,
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

fn shell_permission_request(
    command: &str,
    capability: PermissionCapability,
    risk: PermissionRisk,
) -> PermissionRequest {
    let mut metadata = BTreeMap::new();
    metadata.insert("command".to_string(), command.to_string());
    PermissionRequest {
        call_id: "shell_call".to_string(),
        tool_name: "shell".to_string(),
        capability,
        target: "shell:*".to_string(),
        risk,
        summary: command.to_string(),
        metadata,
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
            arguments: json!({"command": "cargo check --bin sample-arch-graph"}),
        },
        ToolStatus::Error,
        json!({
            "command": "cargo check --bin sample-arch-graph",
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
fn ingest_agents_md_handles_multibyte_header_at_byte_cap() {
    // Workspace under a directory whose name contains a multibyte char, so the
    // per-file header (which embeds the full path) holds a `é` (2 bytes). When
    // the byte budget runs out partway through that header it must not slice
    // mid-character.
    let base = temp_workspace("ingest_agents_md_multibyte");
    let root = base.join("josé");
    fs::create_dir_all(root.join(".git")).expect("create .git marker");
    fs::write(root.join("AGENTS.md"), "root rule").expect("write root AGENTS.md");
    let nested = root.join("crates").join("béta");
    fs::create_dir_all(&nested).expect("create nested dir");
    fs::write(nested.join("AGENTS.md"), "nested rule").expect("write nested AGENTS.md");

    // Reproduce the header exactly as the function builds it for the nested
    // file, then pick a `max_bytes` that lands inside one of its multibyte
    // chars. The nested file is reached only after the root file consumes some
    // budget, so size the cap to leave `remaining` inside the nested header.
    let canonical = fs::canonicalize(&nested).expect("canonicalize");
    let header = format!("--- {} ---\n", canonical.join("AGENTS.md").display());
    let mid = header
        .char_indices()
        .find(|(_, c)| c.len_utf8() > 1)
        .map(|(i, _)| i + 1)
        .expect("multibyte char in header path");
    assert!(!header.is_char_boundary(mid), "chose a mid-char index");

    // Root header + root body + separator, then `mid` bytes into the nested
    // header — without the fix the nested header slice panics here.
    let root_header = format!(
        "--- {} ---\n",
        fs::canonicalize(&root)
            .expect("canonicalize root")
            .join("AGENTS.md")
            .display()
    );
    // The "\n\n" separator is appended without decrementing `remaining`, so the
    // budget reaching the nested header is exactly `mid`.
    let max_bytes = root_header.len() + "root rule".len() + mid;

    let combined = super::ingest_agents_md(&nested, max_bytes).expect("ingest");
    assert!(combined.contains("root rule"));
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

#[tokio::test]
async fn subagent_lifecycle_events_preserve_full_prompt_and_summary() {
    let prompt_tail = "PROMPT_SENTINEL_FULL_CONTEXT";
    let summary_tail = "SUMMARY_SENTINEL_FULL_CONTEXT";
    let long_prompt = format!(
        "Modernize src/cli/client.rs. Review the code for outdated patterns, deprecated APIs, \
         or Rust idioms that could be improved. Suggest and implement modernization improvements \
         such as using newer Rust features, simplifying error handling, and preserving behavior. \
         Repeat context: {} {prompt_tail}",
        "let-else try-operator iterator-cleanup ".repeat(12)
    );
    let long_summary = format!(
        "Completed the modernization review while preserving behavior. Findings and rationale: {} \
         {summary_tail}",
        "updated patterns validated tests retained context ".repeat(14)
    );
    assert!(
        long_prompt.chars().count() > 240,
        "prompt must exceed the old event preview cap"
    );
    assert!(
        long_summary.chars().count() > 320,
        "summary must exceed the old event preview cap"
    );

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "del_full_lifecycle".to_string(),
                name: "delegate".to_string(),
                arguments: json!({"prompt": long_prompt}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_parent_full_lifecycle_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(long_summary)),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_sub_full_lifecycle_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("noted".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_parent_full_lifecycle_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(AppConfig::default(), provider);
    let mut rx = agent.start_turn(
        "delegate long modernization review".to_string(),
        CancellationToken::new(),
    );
    let mut started_prompt = None;
    let mut completed_summary = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::SubagentStarted { prompt, .. } => {
                started_prompt = Some(prompt);
            }
            AgentEvent::SubagentCompleted { summary, .. } => {
                completed_summary = Some(summary);
            }
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. } => break,
            _ => {}
        }
    }

    let started_prompt = started_prompt.expect("SubagentStarted prompt");
    assert!(
        started_prompt.contains(prompt_tail),
        "subagent prompt event was truncated before the TUI could show it: {started_prompt}"
    );
    assert!(
        !started_prompt.contains("[truncated]"),
        "subagent prompt event should carry full local UI text: {started_prompt}"
    );
    let completed_summary = completed_summary.expect("SubagentCompleted summary");
    assert!(
        completed_summary.contains(summary_tail),
        "subagent summary event was truncated before the TUI could show it: {completed_summary}"
    );
    assert!(
        !completed_summary.contains("[truncated]"),
        "subagent summary event should carry full local UI text: {completed_summary}"
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

#[test]
fn total_tokens_from_cost_sums_present_fields() {
    // `reasoning_output_tokens` is the subset of `output_tokens` that was
    // reasoning, so the total is input + output (reasoning is already
    // inside output and must not be double-counted).
    let cost = CostSnapshot {
        input_tokens: Some(1_000),
        output_tokens: Some(2_000),
        reasoning_output_tokens: Some(500),
        ..CostSnapshot::default()
    };
    assert_eq!(super::total_tokens_from_cost(&cost), Some(3_000));
    // reasoning_output_tokens is a subset of output_tokens, so the
    // total is input + output only (not input + output + reasoning).
    assert_eq!(super::total_tokens_from_cost(&cost), Some(3_000));
}

#[test]
fn total_tokens_from_cost_excludes_reasoning_subset() {
    // OpenAI-family usage: output_tokens is the inclusive generated
    // total and reasoning_output_tokens (2_500) is a subset of it, so
    // the model actually consumed input + output = 4_000 — not 6_500.
    let cost = CostSnapshot {
        input_tokens: Some(1_000),
        output_tokens: Some(3_000),
        reasoning_output_tokens: Some(2_500),
        ..CostSnapshot::default()
    };
    assert_eq!(super::total_tokens_from_cost(&cost), Some(4_000));
}

#[test]
fn total_tokens_from_cost_returns_none_when_no_fields() {
    let cost = CostSnapshot::default();
    assert!(super::total_tokens_from_cost(&cost).is_none());
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
            // Tiny window ⇒ the summarize threshold floors at 1 token, so any
            // non-trivial conversation crosses it and post-turn summarize fires.
            model_context_window: Some(1),
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

#[test]
fn explore_model_alias_resolves_before_subagent_dispatch() {
    let mut config = AppConfig::default();
    config.subagents.explore_model = Some("haiku".to_string());

    let model = subagent_model_for_kind("anthropic", &config, SubagentKind::Explore);

    assert_eq!(model, "claude-haiku-4-5-20251001");
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
async fn subagent_max_concurrent_override_is_honored_by_executor() {
    const OVERRIDE_CAP: usize = 6;
    assert_ne!(OVERRIDE_CAP, SUBAGENT_MAX_CONCURRENT);

    let provider = Arc::new(OneDelegateProvider::new());
    let config = AppConfig {
        subagents: SubagentConfig {
            max_concurrent: OVERRIDE_CAP,
            ..SubagentConfig::default()
        },
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider.clone());

    let registry = agent.subagent_registry_for_test();
    let cancel = CancellationToken::new();
    let mut leases = Vec::new();
    for slot in 0..OVERRIDE_CAP {
        leases.push(
            registry
                .start(
                    roles::SubagentRole::Explorer,
                    cancel.child_token(),
                    OVERRIDE_CAP,
                    format!("pre-saturate {slot}"),
                )
                .expect("under-cap start"),
        );
    }

    let mut rx = agent.start_turn("delegate now".to_string(), cancel.clone());
    let mut rejection: Option<(SubagentRejectionReason, usize, usize)> = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::SubagentRejected {
                reason,
                limit,
                active,
                ..
            } => {
                rejection = Some((reason, limit, active));
            }
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. } => break,
            _ => {}
        }
    }
    drop(leases);

    let (reason, limit, active) =
        rejection.expect("expected a SubagentRejected event when the cap is full");
    assert_eq!(reason, SubagentRejectionReason::ConcurrencyCap);
    assert_eq!(limit, OVERRIDE_CAP);
    assert_eq!(active, OVERRIDE_CAP);
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
            fallback_window_tokens: 0,
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
        0,
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

#[tokio::test]
async fn manual_compact_does_not_hold_conversation_lock_across_model_assisted_call() {
    // `compact_context_manual` runs the (possibly slow) model-assisted
    // provider round-trip. It must release the `conversation_state`
    // mutex before that await so concurrent snapshot readers — the TUI's
    // per-frame context/cost reads — keep making progress. A
    // `HangingProvider` keeps the model-assisted request in flight for
    // the full timeout; if the guard were held across the await, the
    // snapshot read below would block for that entire window instead of
    // returning at once.
    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            strategy: CompactionStrategy::ModelAssisted,
            model_assisted_model: Some("test-model".to_string()),
            // Long enough that holding the lock would block the snapshot
            // far beyond the assertion timeout below.
            model_assisted_timeout_secs: 30,
            recent_items: 2,
            min_items: 4,
            fallback_window_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let provider = Arc::new(HangingProvider::new());
    let agent = Arc::new(Agent::new(config, provider.clone()));
    agent.conversation_state.lock().await.conversation = mid_turn_test_conversation();

    let compact_agent = agent.clone();
    let compact = tokio::spawn(async move { compact_agent.compact_context_manual().await });

    // Wait until the model-assisted provider request is actually in
    // flight (and thus the hanging await is pending).
    tokio::time::timeout(Duration::from_secs(5), async {
        while provider.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("model-assisted compaction request should be issued");

    // The lock must be free while compaction is in flight: a concurrent
    // snapshot read returns promptly instead of queueing behind the held
    // guard for the 30s timeout window.
    tokio::time::timeout(
        Duration::from_millis(500),
        agent.context_estimate_snapshot(),
    )
    .await
    .expect("snapshot read must not block on the in-flight model-assisted compaction");

    compact.abort();
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
            fallback_window_tokens: 0,
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
        None,
        &config,
        ContextCompactionTrigger::Manual,
        true,
        0,
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
fn compaction_checkpoint_stores_session_id() {
    use squeezy_store::SqueezyStore;

    let root = temp_workspace("compact_checkpoint_sid");
    let store = SqueezyStore::open(&root, None).expect("open store");
    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            recent_items: 2,
            min_items: 4,
            fallback_window_tokens: 0,
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
        Some(&store),
        Some("test-session-abc"),
        &config,
        ContextCompactionTrigger::Manual,
        true,
        0,
    )
    .expect("compaction");
    let replacement_id = report
        .record
        .replacement_id
        .clone()
        .expect("replacement_id stamped");
    let checkpoint = store
        .get_compaction_checkpoint(&replacement_id)
        .expect("get checkpoint")
        .expect("checkpoint present");
    assert_eq!(
        checkpoint.session_id, "test-session-abc",
        "checkpoint session_id must match the passed session id"
    );
}

#[test]
fn compaction_without_store_leaves_replacement_id_none() {
    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            recent_items: 2,
            min_items: 4,
            fallback_window_tokens: 0,
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
        None,
        &config,
        ContextCompactionTrigger::Manual,
        true,
        0,
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
            content_parts: None,
            is_error: false,
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_B".to_string(),
            output: "bar result".to_string(),
            content_parts: None,
            is_error: false,
        },
        LlmInputItem::AssistantText("post-tools reply".to_string()),
    ];
    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            recent_items: 4,
            min_items: 1,
            fallback_window_tokens: 0,
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
        None,
        &config,
        ContextCompactionTrigger::Manual,
        true,
        0,
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
        web_call_stats: None,
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
            content_parts: None,
            is_error: false,
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_orphan".to_string(),
            output: "lingering output from a dropped call".to_string(),
            content_parts: None,
            is_error: false,
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
        LlmInputItem::FunctionCallOutput {
            call_id, output, ..
        } => {
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
            content_parts: None,
            is_error: false,
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
            fallback_window_tokens: 0,
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
        0,
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
            fallback_window_tokens: 0,
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
        0,
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
            fallback_window_tokens: 0,
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
        0,
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
async fn maybe_compact_conversation_honors_model_assisted_strategy() {
    // Regression for #235: the post-turn auto-compaction path must honor a
    // configured `ModelAssisted` strategy. Before the fix it called the
    // extractive summarizer directly and ignored `strategy`.
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
            fallback_window_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let mut conversation = mid_turn_test_conversation();
    let mut state = ContextCompactionState::default();
    let provider_trait: Arc<dyn LlmProvider> = provider.clone();
    let redactor = Redactor::default();
    let report = super::maybe_compact_conversation(
        &mut conversation,
        &mut state,
        &[],
        None,
        &provider_trait,
        None,
        &redactor,
        &config,
        ContextCompactionTrigger::Auto,
        0,
    )
    .await
    .expect("post-turn auto-compaction should fire");

    assert_eq!(
        report.summary.trim(),
        structured.trim(),
        "post-turn summary head must be the model-assisted output, not the extractive blob"
    );
    assert!(
        !report
            .summary
            .contains("Squeezy compacted conversation context"),
        "model-assisted auto output must replace the extractive summary"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "the configured strategy must issue exactly one model-assisted request"
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
            fallback_window_tokens: 0,
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
        0,
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
        0,
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
fn skill_subagent_uses_system_override_as_instructions() {
    let request = super::SubagentRequest {
        prompt: "explain how this skill applies to the user's task".to_string(),
        scope: None,
        thoroughness: None,
        system_override: Some("# Skill body\nFollow these steps exactly.".to_string()),
    };
    let instructions = super::subagent_instructions(SubagentKind::Skill, &request);
    assert!(
        instructions.contains("# Skill body"),
        "skill subagent must run the supplied body as system instructions: {instructions}"
    );
}

#[test]
fn skill_subagent_falls_back_when_system_override_missing() {
    let request = super::SubagentRequest {
        prompt: "do the thing".to_string(),
        scope: None,
        thoroughness: None,
        system_override: None,
    };
    let instructions = super::subagent_instructions(SubagentKind::Skill, &request);
    assert!(
        instructions.to_lowercase().contains("fork-mode skill"),
        "missing fallback prompt for skill subagent without override: {instructions}"
    );
}

#[test]
fn plan_subagent_instructions_advertise_json_tail_contract() {
    let request = super::SubagentRequest {
        prompt: "plan something".to_string(),
        scope: None,
        thoroughness: None,
        system_override: None,
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

#[test]
fn model_switch_re_derives_context_window() {
    // openai gpt-5.5 has a 400K registered window; anthropic claude-opus-4-7
    // has 200K. A runtime switch must re-derive the new model's window so
    // mid-turn thresholds stop computing against the old one (finding #1).
    let openai: Arc<dyn LlmProvider> = Arc::new(MockProvider::named("openai", vec![]));
    let config = AppConfig {
        model: "gpt-5.5".to_string(),
        ..AppConfig::default()
    };
    let mut agent = Agent::new(config, openai);
    assert_eq!(
        agent
            .config_snapshot()
            .context_compaction
            .model_context_window,
        Some(400_000)
    );

    let anthropic: Arc<dyn LlmProvider> = Arc::new(MockProvider::named("anthropic", vec![]));
    agent.replace_provider(anthropic, "claude-opus-4-7".to_string());
    assert_eq!(
        agent
            .config_snapshot()
            .context_compaction
            .model_context_window,
        Some(200_000)
    );
}

#[test]
fn explicit_context_window_survives_model_switch() {
    // An explicit override (squeezy.toml / SQUEEZY_CONTEXT_MODEL_CONTEXT_WINDOW)
    // must win over registry derivation across a model switch (finding #1).
    let openai: Arc<dyn LlmProvider> = Arc::new(MockProvider::named("openai", vec![]));
    let mut config = AppConfig {
        model: "gpt-5.5".to_string(),
        ..AppConfig::default()
    };
    config.context_compaction.model_context_window = Some(123_456);
    let mut agent = Agent::new(config, openai);
    assert_eq!(
        agent
            .config_snapshot()
            .context_compaction
            .model_context_window,
        Some(123_456)
    );

    let anthropic: Arc<dyn LlmProvider> = Arc::new(MockProvider::named("anthropic", vec![]));
    agent.replace_provider(anthropic, "claude-opus-4-7".to_string());
    assert_eq!(
        agent
            .config_snapshot()
            .context_compaction
            .model_context_window,
        Some(123_456),
        "explicit window override must not be clobbered by re-derivation"
    );
}

/// Pin the hot-reload bridge between `PendingConfigSwap` and the MCP
/// registry: when a settings-watcher reload changes the provider AND
/// `[mcp.servers]` on the same beat, the swap path used to bypass
/// `replace_config` and leave the registry stale until restart.
/// `drain_pending_swap` now invokes the shared reload hook so the
/// observable `mcp_servers()` snapshot reflects the new map by the
/// time the next turn begins.
#[tokio::test]
async fn drain_pending_swap_picks_up_mcp_servers_drift() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(AppConfig::default(), provider);
    assert!(
        agent.mcp_servers().is_empty(),
        "fresh agent starts with no MCP servers"
    );

    let mut next = agent.config_snapshot();
    next.mcp_servers.insert(
        "docs".to_string(),
        squeezy_core::McpServerConfig {
            enabled: true,
            transport: squeezy_core::McpTransport::Http,
            command: None,
            args: Vec::new(),
            url: Some("https://docs.example/mcp".to_string()),
            timeout_ms: None,
            discovery_timeout_ms: None,
            tool_call_timeout_ms: None,
            enabled_tools: None,
            disabled_tools: Vec::new(),
            env: BTreeMap::new(),
            permissions: squeezy_core::McpPermissionConfig::default(),
            bearer_token_env_var: None,
            http_headers: BTreeMap::new(),
            env_http_headers: BTreeMap::new(),
            cwd: None,
        },
    );

    agent.arm_config_swap(PendingConfigSwap {
        config: next,
        // No provider entry → exercises the NextPrompt swap path
        // exactly the way a provider-changing reload would; the
        // provider field would arrive populated, but the MCP-side
        // assertion is identical either way.
        provider: None,
        display_note: None,
    });
    let _drained = agent.drain_pending_swap().expect("swap armed");

    // Yield until the background `replace_mcp_servers` task has run
    // and updated the registry. We poll for up to a few hundred
    // milliseconds; the assertion fails loudly if the reload never
    // happened (the previous bug shape).
    for _ in 0..40 {
        if agent.mcp_servers().contains_key("docs") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        agent.mcp_servers().contains_key("docs"),
        "drain_pending_swap must propagate [mcp.servers] drift to the registry"
    );
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
async fn plan_mode_request_user_input_empty_choices_enables_freeform() {
    use super::{REQUEST_USER_INPUT_TOOL_NAME, RequestUserInputResponse};

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "ask_1".to_string(),
                name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
                // No choices and allow_freeform omitted: the schema advertises
                // this as a free-form question, so the request must arrive with
                // allow_freeform forced on rather than as an unanswerable modal.
                arguments: json!({
                    "question": "What should the new module be called?"
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
    let mut saw_freeform_request = false;
    let mut completed = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::RequestUserInputRequested {
                request,
                response_tx,
                ..
            } => {
                assert!(
                    request.choices.is_empty(),
                    "request carried choices it was not given"
                );
                assert!(
                    request.allow_freeform,
                    "an empty-choices question must enable freeform input"
                );
                saw_freeform_request = true;
                let _ = response_tx.send(RequestUserInputResponse::freeform("widgets"));
            }
            AgentEvent::Completed { .. } => {
                completed = true;
                break;
            }
            AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    assert!(
        saw_freeform_request,
        "agent must emit a request_user_input event for the empty-choices question"
    );
    assert!(
        completed,
        "turn must complete once the free-form answer is provided"
    );
}

#[tokio::test]
async fn build_mode_keeps_proposed_plan_tag_in_transcript() {
    // Outside Plan mode the structured Plan card is not in play, so a literal
    // <proposed_plan> tag is ordinary prose and must survive in the persisted
    // transcript verbatim rather than being silently stripped.
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(
            "before <proposed_plan>\nstep 1\n</proposed_plan> after".to_string(),
        )),
        Ok(LlmEvent::Completed {
            response_id: None,
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    // AppConfig::default() runs in Build mode.
    let agent = Agent::new(AppConfig::default(), provider);

    let mut rx = agent.start_turn("explain it".to_string(), CancellationToken::new());
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    assert_eq!(
        completed.as_deref(),
        Some("before <proposed_plan>\nstep 1\n</proposed_plan> after"),
        "Build-mode transcript must keep the proposed_plan tag verbatim"
    );
}

#[tokio::test]
async fn plan_mode_strips_proposed_plan_tag_from_transcript() {
    // In Plan mode the Plan card owns proposed-plan rendering, so the block is
    // stripped from the persisted transcript (surrounding narration survives).
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta(
            "before <proposed_plan>\nstep 1\n</proposed_plan> after".to_string(),
        )),
        Ok(LlmEvent::Completed {
            response_id: None,
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let config = AppConfig {
        session_mode: SessionMode::Plan,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider);

    let mut rx = agent.start_turn("plan it".to_string(), CancellationToken::new());
    let mut completed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            AgentEvent::Failed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    let content = completed.expect("turn must complete");
    assert!(
        !content.contains("<proposed_plan>") && !content.contains("step 1"),
        "Plan-mode transcript must strip the proposed_plan block: {content:?}"
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

/// Regression for #259: when the routing LLM-judge's billed spend is the call
/// that pushes the session across `cost_warn_percent`, the one-shot
/// `CostWarning` must still surface. The judge fold records the judge cost into
/// the same broker the main turn uses; dropping the warning status it returns
/// would latch `warn_emitted` and silently suppress the user-facing notice.
#[tokio::test]
async fn routing_judge_spend_crossing_warn_threshold_still_surfaces_cost_warning() {
    // First popped response = the judge call; its `Completed` cost crosses the
    // warn threshold (9_000 >= 80% of 10_000) but stays under the cap. Second
    // popped response = the main turn, billed nothing more.
    let provider = Arc::new(MockProvider::new(vec![
        vec![Ok(LlmEvent::Completed {
            response_id: Some("judge".to_string()),
            cost: CostSnapshot {
                estimated_usd_micros: Some(9_000),
                input_tokens: Some(100),
                output_tokens: Some(10),
                ..CostSnapshot::default()
            },
            stop_reason: None,
            reasoning_only_stop: false,
        })],
        vec![Ok(LlmEvent::Completed {
            response_id: Some("main".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        })],
    ]));
    let config = AppConfig {
        model: "parent-model".to_string(),
        // Distinct cheap model so the classifier doesn't short-circuit, and
        // the judge actually dispatches.
        small_fast_model: Some("cheap-model".to_string()),
        routing: squeezy_core::RoutingConfig {
            enabled: true,
            llm_judge: true,
            ..AppConfig::default().routing
        },
        max_session_cost_usd_micros: Some(10_000),
        cost_warn_percent: 80,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider);

    // A non-slam-dunk, non-deictic prompt so `classify_turn` falls through to
    // the LLM judge rather than the heuristic.
    let mut rx = agent.start_turn(
        "Investigate why the build pipeline keeps failing intermittently".to_string(),
        CancellationToken::new(),
    );
    let mut warnings = Vec::new();
    while let Some(event) = rx.recv().await {
        if let AgentEvent::CostWarning { status, .. } = event {
            warnings.push(status);
        }
    }

    assert_eq!(
        warnings.len(),
        1,
        "judge-driven threshold crossing must surface exactly one CostWarning"
    );
    assert_eq!(warnings[0].cap_usd_micros, 10_000);
    assert!(
        warnings[0].spent_usd_micros >= 8_000,
        "warning must report spend at/above the 80% threshold; got {}",
        warnings[0].spent_usd_micros
    );
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

/// A conversation whose `estimate_context` token estimate is large enough to
/// trip a small pre-flight round-input ceiling. Distinct user/assistant text so
/// the bytes are real (not collapsed by SHA-dedup).
fn round_gate_seed_conversation() -> Vec<LlmInputItem> {
    let mut items = Vec::new();
    for n in 0..40 {
        items.push(LlmInputItem::UserText(format!(
            "user message {n}: a long line of context with enough bytes that the \
             token estimate climbs steadily across the whole conversation history",
        )));
        items.push(LlmInputItem::AssistantText(format!(
            "assistant reply {n}: an equally long acknowledgement so the running \
             estimate stays well above any small per-round input-token ceiling",
        )));
    }
    items
}

/// G5 pre-flight round-input gate: the estimate the gate compares against the
/// ceiling must be exactly `estimate_context(...).estimated_tokens` — the gate
/// reuses the existing estimator rather than carrying its own token model.
#[test]
fn round_input_gate_estimate_matches_estimate_context() {
    let conversation = round_gate_seed_conversation();
    let estimated = super::estimate_context(&conversation).estimated_tokens;
    assert!(estimated > 0, "seed conversation must estimate as nonzero");

    // A ceiling one token under the estimate must gate, and the reported
    // `estimated_input_tokens` must equal the estimator's number verbatim.
    let status = super::cost_broker::round_input_gate_status(
        Some(estimated - 1),
        estimated,
        "mock",
        "mock-model",
        1_024,
    )
    .expect("an estimate one over the ceiling must gate");
    assert_eq!(
        status.estimated_input_tokens, estimated,
        "the gate must carry the estimate_context token count unchanged"
    );

    // A ceiling exactly at the estimate must not gate (inclusive limit).
    assert!(
        super::cost_broker::round_input_gate_status(
            Some(estimated),
            estimated,
            "mock",
            "mock-model",
            1_024,
        )
        .is_none(),
        "an estimate exactly at the ceiling must not gate"
    );
}

/// When `max_round_input_tokens` is set and the assembled round exceeds it, the
/// agent must compact *first* (emitting `ContextCompacted`) and then proceed to
/// dispatch the round — not gate the turn — because the forced compaction
/// brings the conversation back under the ceiling.
#[tokio::test]
async fn round_input_gate_compacts_then_proceeds_when_over_limit() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("done".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let seed = round_gate_seed_conversation();
    // Disable the routine auto/mid-turn compaction paths so the *only* thing
    // that can shrink this conversation is the pre-flight gate's own forced
    // compaction — the gate calls `compact_conversation_with_strategy`
    // directly, which runs regardless of `enabled`.
    let compaction_config = ContextCompactionConfig {
        enabled: false,
        enabled_mid_turn: false,
        // Aggressive: keep only a couple of recent items so the forced
        // compaction collapses the long history into a short summary.
        recent_items: 2,
        // Default Extractive strategy needs no model, so a store-less,
        // model-less agent can still compact.
        ..ContextCompactionConfig::default()
    };
    // Deterministically measure the floor the forced compaction lands at by
    // running the same forced compaction on a clone that mirrors the turn's
    // assembled conversation (seed + the new user item). The ceiling is then
    // placed strictly between that floor and the seed estimate, so the gate is
    // guaranteed to fire AND the forced compaction is guaranteed to clear it.
    let mut probe_conversation = seed.clone();
    probe_conversation.push(LlmInputItem::UserText("continue".to_string()));
    let seed_estimate = super::estimate_context(&probe_conversation).estimated_tokens;
    let probe_config = AppConfig {
        context_compaction: compaction_config.clone(),
        ..AppConfig::default()
    };
    let mut probe_state = ContextCompactionState::default();
    super::compact_conversation(
        &mut probe_conversation,
        &mut probe_state,
        &[],
        None,
        None,
        &probe_config,
        ContextCompactionTrigger::Auto,
        true,
        0,
    )
    .expect("forced compaction must produce a report on the seed conversation");
    let compacted_floor = super::estimate_context(&probe_conversation).estimated_tokens;
    assert!(
        compacted_floor < seed_estimate,
        "compaction must shrink the seed ({compacted_floor} !< {seed_estimate})"
    );
    let ceiling = compacted_floor + (seed_estimate - compacted_floor) / 2;
    let config = AppConfig {
        max_round_input_tokens: Some(ceiling),
        context_compaction: compaction_config,
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider.clone());
    agent.conversation_state.lock().await.conversation = seed;

    let mut rx = agent.start_turn("continue".to_string(), CancellationToken::new());
    let mut compacted = None;
    let mut completed = None;
    let mut failed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ContextCompacted { report, .. } => compacted = Some(report),
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            AgentEvent::Failed { error, .. } => failed = Some(error.to_string()),
            _ => {}
        }
    }

    assert!(
        failed.is_none(),
        "the gate must compact and proceed, not fail the turn; got: {failed:?}"
    );
    let report = compacted.expect("the round-input gate must force a compaction");
    assert!(
        report.record.after.estimated_tokens < report.record.before.estimated_tokens,
        "forced compaction must shrink the conversation: {} -> {}",
        report.record.before.estimated_tokens,
        report.record.after.estimated_tokens,
    );
    assert!(
        report.record.after.estimated_tokens <= ceiling,
        "post-compaction estimate ({}) must land at/under the ceiling ({ceiling})",
        report.record.after.estimated_tokens,
    );
    assert_eq!(
        completed.as_deref(),
        Some("done"),
        "the round must dispatch and complete after compaction"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "the model must be hit exactly once, after the gate compacted"
    );
}

/// Default-off: with `max_round_input_tokens` unset the gate is inert — the
/// same oversized conversation dispatches without any gate-driven compaction,
/// proving behaviour is unchanged when the knob is `None`.
#[tokio::test]
async fn round_input_gate_noop_when_unset() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(LlmEvent::Started),
        Ok(LlmEvent::TextDelta("done".to_string())),
        Ok(LlmEvent::Completed {
            response_id: Some("resp".to_string()),
            cost: CostSnapshot::default(),
            stop_reason: None,
            reasoning_only_stop: false,
        }),
    ]]));
    let seed = round_gate_seed_conversation();
    // Compaction fully disabled so the *only* path that could emit
    // `ContextCompacted` is the (unset) gate; if the gate were active it would
    // still try to force-compact. With the knob unset, nothing fires.
    let config = AppConfig {
        max_round_input_tokens: None,
        context_compaction: ContextCompactionConfig {
            enabled: false,
            enabled_mid_turn: false,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider.clone());
    agent.conversation_state.lock().await.conversation = seed;

    let mut rx = agent.start_turn("continue".to_string(), CancellationToken::new());
    let mut compacted = false;
    let mut completed = None;
    let mut failed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ContextCompacted { .. } => compacted = true,
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            AgentEvent::Failed { error, .. } => failed = Some(error.to_string()),
            _ => {}
        }
    }

    assert!(
        !compacted,
        "an unset round-input gate must never trigger compaction"
    );
    assert!(
        failed.is_none(),
        "the turn must not be gated; got: {failed:?}"
    );
    assert_eq!(
        completed.as_deref(),
        Some("done"),
        "the oversized round must dispatch unchanged when the gate is off"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "the model must be hit once, with no gate interference"
    );
}

#[tokio::test]
async fn provider_context_overflow_compacts_and_retries_once() {
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ContextOverflow {
                provider: "mock".to_string(),
                signal: squeezy_llm::overflow::OverflowSignal::ErrorPattern(
                    "context_length_exceeded".to_string(),
                ),
            }),
            Err(SqueezyError::ProviderStream(
                "context_length_exceeded".to_string(),
            )),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done after compact".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_after_compact".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: None,
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let config = AppConfig {
        context_compaction: ContextCompactionConfig {
            enabled: false,
            enabled_mid_turn: false,
            recent_items: 2,
            min_items: 4,
            fallback_window_tokens: 0,
            ..ContextCompactionConfig::default()
        },
        ..AppConfig::default()
    };
    let agent = Agent::new(config, provider.clone());
    agent.conversation_state.lock().await.conversation = mid_turn_test_conversation();

    let mut rx = agent.start_turn("continue".to_string(), CancellationToken::new());
    let mut compacted = None;
    let mut completed = None;
    let mut failed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::ContextCompacted { report, .. } => compacted = Some(report),
            AgentEvent::Completed { message, .. } => completed = Some(message.content),
            AgentEvent::Failed { error, .. } => failed = Some(error.to_string()),
            _ => {}
        }
    }

    assert!(
        failed.is_none(),
        "overflow should compact and retry once instead of failing: {failed:?}"
    );
    let report = compacted.expect("provider overflow should force compaction");
    assert!(
        report.record.after.estimated_tokens < report.record.before.estimated_tokens,
        "overflow compaction should shrink context: {} -> {}",
        report.record.before.estimated_tokens,
        report.record.after.estimated_tokens,
    );
    assert_eq!(completed.as_deref(), Some("done after compact"));
    assert_eq!(
        provider.requests().len(),
        2,
        "the first overflowing call should be retried exactly once after compaction"
    );
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
    broker.seed_session(
        &CostSnapshot {
            estimated_usd_micros: Some(6_000),
            ..Default::default()
        },
        squeezy_llm::TokenCalibration::default(),
    );
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
    broker.seed_session(
        &CostSnapshot {
            estimated_usd_micros: Some(12_457),
            ..Default::default()
        },
        squeezy_llm::TokenCalibration::default(),
    );
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
        web_call_stats: None,
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
        ..
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
        web_call_stats: None,
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
        web_call_stats: None,
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
        web_call_stats: None,
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
async fn malformed_function_call_retries_with_corrective_nudge() {
    // Gemini-style malformed tool-call arguments: round 0 stops with
    // MalformedFunctionCall and no usable tool call. The agent should
    // inject a corrective nudge and recover on the retry instead of ending
    // the turn empty.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_malformed".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::MalformedFunctionCall),
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("The fix is in src/foo.rs.".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_clean".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::EndTurn),
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let mut config = AppConfig::default();
    config.routing.enabled = false;
    let agent = Agent::new(config, provider.clone());
    let mut rx = agent.start_turn("find the bug".to_string(), CancellationToken::new());
    let mut completed_text = None;
    let mut failed = false;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Completed { message, .. } => completed_text = Some(message.content),
            AgentEvent::Failed { .. } => failed = true,
            _ => {}
        }
    }
    assert!(!failed, "malformed tool call should recover, not fail");
    assert_eq!(completed_text.as_deref(), Some("The fix is in src/foo.rs."));
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "malformed round plus one corrective retry"
    );
    let retry = &requests[1];
    let has_nudge = retry.input.iter().any(|item| match item {
        LlmInputItem::UserText(text) => text.contains("could not be parsed"),
        _ => false,
    });
    assert!(has_nudge, "retry must carry the corrective JSON nudge");
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
async fn pause_turn_without_tool_calls_reissues_before_failing() {
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_pause".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::PauseTurn),
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("done".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_done".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::EndTurn),
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(AppConfig::default(), provider.clone());
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

    assert!(saw_success, "pause_turn reissue should allow completion");
    assert!(!saw_failure, "successful pause_turn reissue must not fail");
    assert_eq!(
        provider.requests().len(),
        2,
        "agent must issue a second provider request after pause_turn"
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
            content_parts: None,
            is_error: false,
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

#[test]
fn assistant_text_has_unresolved_intent_handles_multibyte_tail() {
    // A multibyte char straddling the `tail_start + 40` byte offset must not
    // panic: `é` lands at byte 44/45, exactly the slice end for `"i'll "`.
    let text = format!("I'll {}é and continue", "x".repeat(39));
    let _ = assistant_text_has_unresolved_intent(&text);
}

#[test]
fn unresolved_intent_anchors_on_final_clause_not_midanswer() {
    // Strong-model shape: an intent phrase used mid-answer, but the
    // message CONCLUDES. Anchoring on the final clause means this is not
    // treated as an unresolved promise (the dominant false positive).
    assert!(!assistant_text_has_unresolved_intent(
        "Let me check: yes, the bug is in foo.rs. The fix is to add a guard.",
    ));
}

#[test]
fn unresolved_intent_skips_offer_idiom() {
    // "Let me know if you'd like me to check ..." is a closing offer,
    // not abandoned tool work.
    assert!(!assistant_text_has_unresolved_intent(
        "I fixed the parser. Let me know if you'd like me to check the other files.",
    ));
}

#[test]
fn unresolved_intent_fires_when_final_clause_announces_action() {
    // Multi-sentence message that ENDS on an announced, undelivered action.
    assert!(assistant_text_has_unresolved_intent(
        "I read the file. Now let me search the repository for callers.",
    ));
}

#[test]
fn unresolved_intent_fires_on_dangling_colon() {
    // A trailing ':' is itself an "about to act" signal.
    assert!(assistant_text_has_unresolved_intent(
        "Now let me grep for the symbol:",
    ));
}

#[test]
fn unresolved_intent_keeps_dotted_tokens_intact() {
    // The '.' in "src/lib.rs" must not be read as a sentence boundary,
    // or the intent that precedes it would be lost.
    assert!(assistant_text_has_unresolved_intent(
        "I'll edit src/lib.rs to add the guard.",
    ));
}

#[test]
fn retry_ack_recognizes_bare_done_confirmation() {
    // The G2 "reply DONE if complete" path: a bare confirmation collapses
    // back to the prior answer, but added content does not.
    assert!(assistant_text_is_retry_ack("DONE"));
    assert!(assistant_text_is_retry_ack("Done."));
    assert!(assistant_text_is_retry_ack("`DONE`"));
    assert!(assistant_text_is_retry_ack("**Done.**"));
    // A short, content-free completeness confirmation is still an ack.
    assert!(assistant_text_is_retry_ack(
        "The previous output is the complete answer."
    ));
    assert!(!assistant_text_is_retry_ack(
        "Done — I also updated the changelog.",
    ));
    // A response that OPENS like a confirmation ("the previous response
    // is ...") but actually negates it and supplies the missing content
    // must NOT be treated as an ack — it carries the real continuation.
    assert!(!assistant_text_is_retry_ack(
        "The previous response is incomplete; the missing file is src/foo.rs.",
    ));
    assert!(!assistant_text_is_retry_ack(
        "The previous answer is wrong — the correct value is 42 because the cache resets at midnight UTC.",
    ));
}

#[test]
fn merge_retried_keeps_prior_answer_when_retry_confirms_done() {
    // G1+G2: confirm-or-continue nudge -> a done model replies DONE, and
    // the prior substantive answer is preserved verbatim (nothing dropped).
    let mut deferred = String::new();
    append_deferred_visible_assistant_text(
        &mut deferred,
        "The function `needle` is defined once in src/lib.rs at line 12.",
        true,
    );
    let merged = merge_retried_visible_assistant_text(&mut deferred, "DONE", true);
    assert_eq!(
        merged,
        "The function `needle` is defined once in src/lib.rs at line 12."
    );
}

#[test]
fn merge_retried_appends_real_continuation() {
    // A genuine stall recovery: the retry produced new substantive text,
    // appended after the prior visible text — nothing is dropped.
    let mut deferred = String::new();
    append_deferred_visible_assistant_text(&mut deferred, "I scanned the tree.", true);
    let merged = merge_retried_visible_assistant_text(
        &mut deferred,
        "The entrypoint is `main` in cli.rs.",
        true,
    );
    assert_eq!(
        merged,
        "I scanned the tree.\n\nThe entrypoint is `main` in cli.rs."
    );
}

#[test]
fn merge_retried_appends_continuation_that_references_the_prior() {
    // The retry response opens by referencing the prior output but then
    // negates it and delivers the missing content. It is a real
    // continuation and must be APPENDED, not discarded as an ack.
    let mut deferred = String::new();
    append_deferred_visible_assistant_text(&mut deferred, "I summarized the config.", true);
    let continuation = "The previous response is incomplete; the missing file is src/foo.rs.";
    let merged = merge_retried_visible_assistant_text(&mut deferred, continuation, true);
    assert_eq!(
        merged,
        format!("I summarized the config.\n\n{continuation}"),
    );
}

#[test]
fn unresolved_intent_skips_real_complete_answer_ending_in_question() {
    // Regression witness from a real incident (gctoolkit session
    // 1780784071532-67685-1, turn-2): the user asked "what model are you
    // and why didn't you load the skill yet?". The model gave a complete,
    // well-behaved 2.2k-char answer that ENDS on a permission question,
    // but whose BODY contains mid-answer intent phrases ("I'll find out
    // when I try", "I'll run them") that the old whole-text scan matched —
    // firing a spurious `promised_action` retry whose pushy "call the tool
    // now" nudge then drove unrequested file edits. Anchoring on the final
    // clause clears the answer, so no retry fires.
    let answer = "You're right to call that out — let me address both honestly. \
On the model: I don't have a reliable way to verify my exact underlying model \
from inside this environment, so I won't guess. I operate here as \"Squeezy\". \
Those commands may or may not run in this shell — I'll find out when I try, and \
I'll run them and surface any config/auth errors to you rather than retrying \
blindly. So before I touch CPUSummary.java (a clean record conversion, ~4 caller \
sites), the correct next step per the skill is to run `guidelines get`. Want me \
to proceed with that and continue the modernization?";
    assert!(!assistant_text_has_unresolved_intent(answer));
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
        (DispatchCommand::Config { section: None }, "config"),
        (DispatchCommand::Model, "model"),
        (
            DispatchCommand::Plans {
                args: String::new(),
            },
            "plans",
        ),
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
        (
            DispatchCommand::ToolVerbosity { value: None },
            "tool-verbosity",
        ),
        (
            DispatchCommand::Theme {
                theme: Some("dark".to_string()),
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
                force: false,
            },
            "session-export-html",
        ),
        (DispatchCommand::Clear, "clear"),
        (DispatchCommand::Pin { target: None }, "pin"),
        (DispatchCommand::Cheap, "cheap"),
        (DispatchCommand::Parent, "parent"),
        (DispatchCommand::Router { value: None }, "router"),
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
async fn steer_cancels_active_turn_before_starting_replacement() {
    let provider = Arc::new(SteerInterruptProvider::new());
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut first_rx = agent.start_turn("keep working".to_string(), CancellationToken::new());
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if provider.requests().len() == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("first turn should reach the provider before steering");

    let mut replacement_rx = agent.steer("change direction".to_string(), CancellationToken::new());

    let saw_cancelled = tokio::time::timeout(Duration::from_millis(500), async {
        while let Some(event) = first_rx.recv().await {
            match event {
                AgentEvent::Cancelled { .. } => return true,
                AgentEvent::Failed { error, .. } => panic!("first turn failed: {error}"),
                _ => {}
            }
        }
        false
    })
    .await
    .expect("steer should cancel the first turn promptly");
    assert!(saw_cancelled, "first turn did not observe cancellation");

    let mut completed = None;
    while let Some(event) = replacement_rx.recv().await {
        match event {
            AgentEvent::Completed {
                message,
                response_id,
                ..
            } => completed = Some((message.content, response_id)),
            AgentEvent::Failed { error, .. } => panic!("replacement turn failed: {error}"),
            _ => {}
        }
    }

    assert_eq!(
        completed,
        Some((
            "replacement done".to_string(),
            Some("resp_steered".to_string())
        )),
        "steer should run the replacement turn to completion"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "old and replacement turns should run");
    assert!(
        requests[0].input.iter().any(|item| matches!(
            item,
            LlmInputItem::UserText(text) if text == "keep working"
        )),
        "first prompt missing from first request: {:?}",
        requests[0].input
    );
    assert!(
        requests[1].input.iter().any(|item| matches!(
            item,
            LlmInputItem::UserText(text) if text == "change direction"
        )),
        "replacement prompt missing from second request: {:?}",
        requests[1].input
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_cancelled_steered_turn_does_not_drop_replacement_context() {
    let provider = Arc::new(DelayedFirstCancelProvider::new());
    let agent = Agent::new(AppConfig::default(), provider.clone());

    let mut old_rx = agent.start_turn("keep working".to_string(), CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if provider.requests().len() == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("old turn should reach the provider");

    let mut replacement_rx = agent.steer("change direction".to_string(), CancellationToken::new());
    let mut replacement_completed = false;
    while let Some(event) = replacement_rx.recv().await {
        match event {
            AgentEvent::Completed { message, .. } => {
                assert_eq!(message.content, "replacement done");
                replacement_completed = true;
                break;
            }
            AgentEvent::Failed { error, .. } => panic!("replacement turn failed: {error}"),
            _ => {}
        }
    }
    assert!(replacement_completed, "replacement turn should complete");

    provider.release_first_response();
    let saw_old_cancelled = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(event) = old_rx.recv().await {
            match event {
                AgentEvent::Cancelled { .. } => return true,
                AgentEvent::Failed { error, .. } => panic!("old turn failed: {error}"),
                _ => {}
            }
        }
        false
    })
    .await
    .expect("old turn should finish cancellation after release");
    assert!(saw_old_cancelled, "old turn should report cancellation");

    let mut after_rx = agent.next_turn("after steering".to_string(), CancellationToken::new());
    while let Some(event) = after_rx.recv().await {
        if let AgentEvent::Failed { error, .. } = event {
            panic!("after turn failed: {error}");
        }
    }

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        3,
        "old, replacement, and after turns should all request the provider"
    );
    let after_input = &requests[2].input;
    assert!(
        after_input.iter().any(|item| matches!(
            item,
            LlmInputItem::UserText(text) if text == "change direction"
        )),
        "late old cancellation dropped the replacement user prompt from context: {after_input:?}"
    );
    assert!(
        after_input.iter().any(|item| matches!(
            item,
            LlmInputItem::AssistantText(text) if text == "replacement done"
        )),
        "late old cancellation dropped the replacement answer from context: {after_input:?}"
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

// Verifies that a long-running subagent body emits `AgentEvent::ToolProgress`
// heartbeats on the parent's event channel even before the subagent fires
// its first inner tool call.
//
// Regression for the no-graph `explore` deadlock: when `excluded_tools`
// strips the subagent's graph-tool whitelist down to glob/grep/read_file,
// the subagent's first model round can spend tens of seconds reasoning
// about how to substitute for the missing tools. The drain task in
// `run_subagent` only forwards `ToolProgress` events from inside the
// subagent (and those only fire while an inner tool is running), so the
// parent's per-event timeout (60s in the eval driver) would expire with
// nothing but a `SubagentStarted` line in the trace, abandoning the turn
// with $0 cost. The fix wraps the `run_subagent` await in a per-tick
// progress emitter on the parent's `tx`, mirroring the per-tool ticker
// used elsewhere, so the explore call looks like any other long-running
// tool from the parent's perspective.
#[tokio::test]
async fn explore_subagent_emits_tool_progress_heartbeats_during_slow_first_round() {
    use std::task::{Context as TaskContext, Poll};

    // Custom stream that returns `Pending` until the configured delay
    // elapses, then yields its queued events one by one.
    struct DelayedStream {
        delay: Option<Pin<Box<tokio::time::Sleep>>>,
        events: VecDeque<Result<LlmEvent>>,
    }

    impl Stream for DelayedStream {
        type Item = Result<LlmEvent>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Self::Item>> {
            if let Some(sleep) = self.delay.as_mut() {
                match sleep.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(()) => {
                        self.delay = None;
                    }
                }
            }
            match self.events.pop_front() {
                Some(event) => Poll::Ready(Some(event)),
                None => Poll::Ready(None),
            }
        }
    }

    struct DelayedSubagentProvider {
        responses: Mutex<VecDeque<(Duration, Vec<Result<LlmEvent>>)>>,
    }

    impl LlmProvider for DelayedSubagentProvider {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
            let (delay, events) = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .unwrap_or((Duration::from_millis(0), Vec::new()));
            let sleep = if delay.is_zero() {
                None
            } else {
                Some(Box::pin(tokio::time::sleep(delay)))
            };
            Box::pin(DelayedStream {
                delay: sleep,
                events: events.into(),
            })
        }
    }

    // Parent round 1: model immediately calls `explore`.
    // Subagent round 1: sleeps ~1.6s before yielding any event, then
    // returns a one-line text answer. The sleep is long enough to cross
    // the 1s `TOOL_PROGRESS_INTERVAL` so we observe at least one
    // heartbeat tick from the parent-side progress emitter.
    // Parent round 2: closes the turn.
    let responses = VecDeque::from(vec![
        (
            Duration::from_millis(0),
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "call_explore_heartbeat".to_string(),
                    name: "explore".to_string(),
                    arguments: json!({"prompt": "investigate slow subagent heartbeat path"}),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_parent_heartbeat_1".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ],
        ),
        (
            Duration::from_millis(1600),
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("ok".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_sub_heartbeat_1".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ],
        ),
        (
            Duration::from_millis(0),
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("done".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_parent_heartbeat_2".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: None,
                    reasoning_only_stop: false,
                }),
            ],
        ),
    ]);
    let provider = Arc::new(DelayedSubagentProvider {
        responses: Mutex::new(responses),
    });

    let root = temp_workspace("explore_subagent_heartbeat");
    fs::create_dir_all(root.join(".git")).expect("create .git marker");
    let agent = Agent::new(
        AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        },
        provider.clone(),
    );

    let mut rx = agent.start_turn(
        "trigger an explore subagent".to_string(),
        CancellationToken::new(),
    );
    let mut subagent_started = false;
    let mut subagent_completed = false;
    let mut explore_progress_count: u32 = 0;
    while let Some(event) = rx.recv().await {
        match &event {
            AgentEvent::SubagentStarted { agent, .. } if agent == "explore" => {
                subagent_started = true;
            }
            AgentEvent::ToolProgress {
                tool_name, call_id, ..
            } if tool_name == "explore"
                && call_id == "call_explore_heartbeat"
                && subagent_started =>
            {
                explore_progress_count += 1;
            }
            AgentEvent::SubagentCompleted { agent, .. } if agent == "explore" => {
                subagent_completed = true;
            }
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. } => break,
            _ => {}
        }
    }

    assert!(
        subagent_started,
        "explore subagent must have emitted SubagentStarted"
    );
    assert!(
        subagent_completed,
        "explore subagent must have emitted SubagentCompleted"
    );
    assert!(
        explore_progress_count >= 1,
        "parent must receive at least one ToolProgress heartbeat \
         tagged with tool_name=explore between SubagentStarted and \
         SubagentCompleted so the eval driver's 60s event_timeout \
         does not abandon a subagent whose first model round is \
         silent; got {explore_progress_count}",
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

fn graph_indexing_fallback_result(tool_name: &str) -> squeezy_tools::ToolResult {
    // Mirrors the wire shape produced by
    // `squeezy_tools::graph_tools::graph_unavailable_result(call, true)`
    // (the `still_indexing = true` branch added in `fddd56e7`). Kept in
    // sync there is a build-time guarantee — the agent only retries when
    // *this* shape is observed, so divergence would cause the retry to
    // silently stop firing.
    squeezy_tools::ToolResult {
        call_id: "call-graph-indexing".to_string(),
        tool_name: tool_name.to_string(),
        status: ToolStatus::Success,
        content: json!({
            "tool": tool_name,
            "graph_available": false,
            "reason": "semantic graph is still being indexed; retry this tool call",
            "packets": [],
            "fallback": {
                "status": "graph_indexing",
                "retryable": true,
            }
        }),
        cost_hint: squeezy_tools::ToolCostHint::default(),
        receipt: squeezy_tools::ToolReceipt {
            output_sha256: "0".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
        web_call_stats: None,
    }
}

fn graph_success_result(tool_name: &str) -> squeezy_tools::ToolResult {
    squeezy_tools::ToolResult {
        call_id: "call-graph-success".to_string(),
        tool_name: tool_name.to_string(),
        status: ToolStatus::Success,
        content: json!({
            "tool": tool_name,
            "graph_available": true,
            "packets": [{"id": "pkt-1"}],
        }),
        cost_hint: squeezy_tools::ToolCostHint::default(),
        receipt: squeezy_tools::ToolReceipt {
            output_sha256: "1".repeat(64),
            content_sha256: None,
        },
        spill_model_output: None,
        web_call_stats: None,
    }
}

#[test]
fn graph_indexing_detector_matches_post_fddd56e7_fallback() {
    let result = graph_indexing_fallback_result("definition_search");
    assert!(super::is_graph_indexing_retryable_fallback(&result));
}

#[test]
fn graph_indexing_detector_rejects_non_graph_tool() {
    // The detector is gated on the tool family. A grep result that
    // happens to carry an identical-looking `fallback` blob must not be
    // retried — the registry only emits this shape for graph tools.
    let mut result = graph_indexing_fallback_result("definition_search");
    result.tool_name = "grep".to_string();
    assert!(!super::is_graph_indexing_retryable_fallback(&result));
}

#[test]
fn graph_indexing_detector_rejects_structurally_unavailable_result() {
    // `still_indexing = false` means the workspace has no graph at all;
    // retrying would just burn the budget. The detector must distinguish
    // it from the transient cold-start signal.
    let mut result = graph_indexing_fallback_result("definition_search");
    result.content = json!({
        "tool": "definition_search",
        "graph_available": false,
        "reason": "semantic graph is unavailable for this workspace",
        "packets": [],
        "fallback": {
            "status": "graph_unavailable",
            "retryable": false,
        }
    });
    assert!(!super::is_graph_indexing_retryable_fallback(&result));
}

#[test]
fn graph_indexing_detector_rejects_missing_fallback() {
    let mut result = graph_indexing_fallback_result("definition_search");
    result.content = json!({
        "tool": "definition_search",
        "graph_available": true,
        "packets": [],
    });
    assert!(!super::is_graph_indexing_retryable_fallback(&result));
}

#[tokio::test]
async fn maybe_retry_graph_indexing_invokes_executor_exactly_once_on_indexing() {
    // Regression for the cold-open Scala/Ruby trace: the first graph
    // call returns `graph_indexing`, the second succeeds, and the
    // executor closure must be reached exactly once so the model only
    // ever sees the successful packet — never the stub.
    let initial = graph_indexing_fallback_result("definition_search");
    let success = graph_success_result("definition_search");
    let cancel = tokio_util::sync::CancellationToken::new();
    let executor_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor_calls_handle = Arc::clone(&executor_calls);
    let success_for_executor = success.clone();
    let observed =
        super::maybe_retry_graph_indexing(initial, &cancel, Duration::from_millis(0), move || {
            executor_calls_handle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let success = success_for_executor.clone();
            async move { success }
        })
        .await;
    assert_eq!(
        executor_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "executor must be invoked exactly once when initial is graph_indexing",
    );
    assert_eq!(
        observed.content, success.content,
        "the retry's success payload must surface to the model, not the stub",
    );
    assert_eq!(observed.tool_name, success.tool_name);
}

#[tokio::test]
async fn maybe_retry_graph_indexing_passes_through_when_initial_already_succeeded() {
    // No retry, no sleep — the success result returns unchanged. This
    // is the dominant case (warm graph, or first turn of a previously
    // opened workspace).
    let success = graph_success_result("definition_search");
    let cancel = tokio_util::sync::CancellationToken::new();
    let executor_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor_calls_handle = Arc::clone(&executor_calls);
    let observed = super::maybe_retry_graph_indexing(
        success.clone(),
        &cancel,
        Duration::from_millis(0),
        move || {
            executor_calls_handle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { panic!("executor must not be reached on a healthy initial result") }
        },
    )
    .await;
    assert_eq!(
        executor_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no retry executor call when the initial result is already healthy",
    );
    assert_eq!(observed.content, success.content);
}

#[tokio::test]
async fn maybe_retry_graph_indexing_skips_when_cancelled_before_sleep() {
    // A cancelled turn must short-circuit before sleeping so the agent
    // tears down promptly. The model still sees the indexing fallback;
    // the agent has already given up.
    let initial = graph_indexing_fallback_result("definition_search");
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let executor_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor_calls_handle = Arc::clone(&executor_calls);
    let observed = super::maybe_retry_graph_indexing(
        initial.clone(),
        &cancel,
        Duration::from_millis(0),
        move || {
            executor_calls_handle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { panic!("cancelled turn must not invoke the executor") }
        },
    )
    .await;
    assert_eq!(executor_calls.load(std::sync::atomic::Ordering::SeqCst), 0,);
    assert_eq!(observed.content, initial.content);
}

#[test]
fn attachment_shape_excludes_removed_stored_bytes() {
    fn make(id: &str, status: ContextAttachmentStatus, bytes: usize) -> ContextAttachment {
        ContextAttachment {
            id: id.to_string(),
            source: ContextAttachmentSource::Paste,
            kind: ContextAttachmentKind::Text,
            status,
            label: id.to_string(),
            path: None,
            original_sha256: String::new(),
            redacted_sha256: None,
            original_bytes: bytes,
            stored_bytes: bytes,
            preview_bytes: 0,
            redactions: 0,
            preview: String::new(),
            truncated: false,
            image_media_type: None,
            image_data_base64: None,
        }
    }

    let attachments = vec![
        make("a", ContextAttachmentStatus::Attached, 1000),
        make("b", ContextAttachmentStatus::Removed, 500),
    ];
    let shape = attachment_shape(&attachments);
    assert_eq!(shape.stored_bytes, 1000);
    assert_eq!(shape.active, 1);
    assert_eq!(shape.removed, 1);
    assert_eq!(shape.total, 2);
}

#[test]
fn large_non_image_attachment_threshold_ignores_images_and_removed_items() {
    fn make(
        id: &str,
        kind: ContextAttachmentKind,
        status: ContextAttachmentStatus,
        bytes: usize,
    ) -> ContextAttachment {
        ContextAttachment {
            id: id.to_string(),
            source: ContextAttachmentSource::Paste,
            kind,
            status,
            label: id.to_string(),
            path: None,
            original_sha256: String::new(),
            redacted_sha256: None,
            original_bytes: bytes,
            stored_bytes: bytes,
            preview_bytes: 0,
            redactions: 0,
            preview: String::new(),
            truncated: false,
            image_media_type: None,
            image_data_base64: None,
        }
    }

    let attachments = vec![
        make(
            "text",
            ContextAttachmentKind::Text,
            ContextAttachmentStatus::Attached,
            4096,
        ),
        make(
            "image",
            ContextAttachmentKind::Image,
            ContextAttachmentStatus::Attached,
            100_000,
        ),
        make(
            "removed",
            ContextAttachmentKind::Log,
            ContextAttachmentStatus::Removed,
            100_000,
        ),
    ];

    assert!(has_large_non_image_attachment(&attachments, 4096));
    assert!(!has_large_non_image_attachment(&attachments, 4097));
    assert!(!has_large_non_image_attachment(&attachments, 0));
}

#[test]
fn tool_round_path_collector_counts_distinct_path_like_values() {
    let calls = vec![ToolCall {
        call_id: "call-1".to_string(),
        name: "grep".to_string(),
        arguments: json!({
            "paths": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"],
        }),
    }];
    let result = squeezy_tools::ToolResult {
        call_id: "call-1".to_string(),
        tool_name: "grep".to_string(),
        status: ToolStatus::Success,
        content: json!({
            "matches": [
                {"path": "src/e.rs"},
                {"path": "src/f.rs"},
                {"path": "src/g.rs"},
                {"path": "src/h.rs"}
            ]
        }),
        cost_hint: ToolCostHint::default(),
        receipt: ToolReceipt {
            output_sha256: String::new(),
            content_sha256: None,
        },
        spill_model_output: None,
        web_call_stats: None,
    };
    let pending = SeenToolOutputs::default().prepare_results(vec![result]);
    let mut paths = BTreeSet::new();

    let observed = collect_tool_round_paths(&calls, &pending, 3, &mut paths);

    assert_eq!(observed, 1);
    assert_eq!(paths.len(), ROUTING_DIVERSITY_DISTINCT_PATHS);
}

#[test]
fn tool_round_path_collector_ignores_dotted_non_path_tokens() {
    let calls = vec![ToolCall {
        call_id: "call-1".to_string(),
        name: "grep".to_string(),
        arguments: json!({
            "pattern": "example.com",
            "version": "3.14",
            "symbol": "foo.bar",
        }),
    }];
    let result = squeezy_tools::ToolResult {
        call_id: "call-1".to_string(),
        tool_name: "grep".to_string(),
        status: ToolStatus::Success,
        content: json!({
            "matches": [
                {"text": "example.com"},
                {"text": "3.14"},
                {"text": "foo.bar"},
                {"text": "v1.2.3"},
                {"path": "lib.rs"},
                {"path": "src/main.rs"}
            ],
            "summary": "saw 3.14 and example.com in foo.bar"
        }),
        cost_hint: ToolCostHint::default(),
        receipt: ToolReceipt {
            output_sha256: String::new(),
            content_sha256: None,
        },
        spill_model_output: None,
        web_call_stats: None,
    };
    let pending = SeenToolOutputs::default().prepare_results(vec![result]);
    let mut paths = BTreeSet::new();

    let observed = collect_tool_round_paths(&calls, &pending, 3, &mut paths);

    assert_eq!(observed, 1);
    assert_eq!(
        paths,
        BTreeSet::from(["lib.rs".to_string(), "src/main.rs".to_string()])
    );
}

// --- M2: successful-edit extraction for expired-context masking ---------

fn edit_output(call_id: &str) -> (LlmInputItem, String, ToolStatus) {
    (
        LlmInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: "{}".to_string(),
            content_parts: None,
            is_error: false,
        },
        "apply_patch".to_string(),
        ToolStatus::Success,
    )
}

#[test]
fn collect_successful_edits_extracts_search_replace_spans() {
    let calls = vec![ToolCall {
        call_id: "e1".to_string(),
        name: "apply_patch".to_string(),
        arguments: json!({
            "patches": [
                { "path": "foo.rs", "search": "old body one", "replace": "new" },
                { "path": "bar.rs", "search": "old body two", "replace": "new" },
            ]
        }),
    }];
    let outputs = vec![edit_output("e1")];
    let edits = collect_successful_edits(&calls, &outputs);
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0].path, "foo.rs");
    assert_eq!(edits[0].changed_spans, vec!["old body one".to_string()]);
    assert!(!edits[0].whole_file);
    assert_eq!(edits[1].path, "bar.rs");
    assert_eq!(edits[1].changed_spans, vec!["old body two".to_string()]);
}

#[test]
fn collect_successful_edits_skips_errored_and_denied_edits() {
    let calls = vec![
        ToolCall {
            call_id: "ok".to_string(),
            name: "apply_patch".to_string(),
            arguments: json!({ "patches": [{ "path": "a.rs", "search": "x", "replace": "y" }] }),
        },
        ToolCall {
            call_id: "err".to_string(),
            name: "apply_patch".to_string(),
            arguments: json!({ "patches": [{ "path": "b.rs", "search": "x", "replace": "y" }] }),
        },
        ToolCall {
            call_id: "denied".to_string(),
            name: "apply_patch".to_string(),
            arguments: json!({ "patches": [{ "path": "c.rs", "search": "x", "replace": "y" }] }),
        },
    ];
    let mk = |id: &str, status: ToolStatus| {
        (
            LlmInputItem::FunctionCallOutput {
                call_id: id.to_string(),
                output: "{}".to_string(),
                content_parts: None,
                is_error: status != ToolStatus::Success,
            },
            "apply_patch".to_string(),
            status,
        )
    };
    let outputs = vec![
        mk("ok", ToolStatus::Success),
        mk("err", ToolStatus::Error),
        mk("denied", ToolStatus::Denied),
    ];
    let edits = collect_successful_edits(&calls, &outputs);
    assert_eq!(
        edits.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        vec!["a.rs"],
        "only the successful edit's path should be extracted",
    );
}

#[test]
fn collect_successful_edits_marks_write_file_whole_file() {
    let calls = vec![ToolCall {
        call_id: "w1".to_string(),
        name: "write_file".to_string(),
        arguments: json!({ "path": "gen.rs", "contents": "fn x() {}" }),
    }];
    let outputs = vec![(
        LlmInputItem::FunctionCallOutput {
            call_id: "w1".to_string(),
            output: "{}".to_string(),
            content_parts: None,
            is_error: false,
        },
        "write_file".to_string(),
        ToolStatus::Success,
    )];
    let edits = collect_successful_edits(&calls, &outputs);
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].path, "gen.rs");
    assert!(edits[0].whole_file, "write_file is a full-file overwrite");
    assert!(edits[0].changed_spans.is_empty());
}

#[test]
fn collect_successful_edits_skips_create_delete_move_ops() {
    // Only `search_replace` operations expose a stale in-file span. A
    // create/delete/move op has no prior read snapshot to splice.
    let calls = vec![ToolCall {
        call_id: "op1".to_string(),
        name: "apply_patch".to_string(),
        arguments: json!({
            "operations": [
                { "kind": "create_file", "path": "new.rs", "contents": "x" },
                { "kind": "delete_file", "path": "gone.rs" },
                { "kind": "search_replace", "path": "edit.rs", "search": "stale span here", "replace": "fresh" },
            ]
        }),
    }];
    let outputs = vec![edit_output("op1")];
    let edits = collect_successful_edits(&calls, &outputs);
    assert_eq!(
        edits.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        vec!["edit.rs"],
        "only the search_replace op contributes a changed span",
    );
    assert_eq!(edits[0].changed_spans, vec!["stale span here".to_string()]);
}

#[test]
fn conversation_shape_attributes_load_skill_outputs_to_skills_bucket() {
    use serde_json::json;

    let conversation = vec![
        LlmInputItem::FunctionCall {
            call_id: "call-skill".to_string(),
            name: "load_skill".to_string(),
            arguments: json!({"name": "example-skill"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call-skill".to_string(),
            output: "SKILL BODY ".repeat(10), // 110 bytes
            content_parts: None,
            is_error: false,
        },
        LlmInputItem::FunctionCall {
            call_id: "call-grep".to_string(),
            name: "grep".to_string(),
            arguments: json!({"pattern": "x"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call-grep".to_string(),
            output: "match ".repeat(5), // 30 bytes
            content_parts: None,
            is_error: false,
        },
    ];

    let shape = conversation_shape(&conversation);
    // Both outputs land in tool_output_bytes; only the load_skill one is also
    // attributed to skill_output_bytes (carved out, not double counted).
    assert_eq!(shape.function_outputs, 2);
    assert_eq!(shape.tool_output_bytes, 110 + 30);
    assert_eq!(shape.skill_output_bytes, 110);
    // A non-load_skill output never leaks into the skills bucket.
    assert!(shape.skill_output_bytes < shape.tool_output_bytes);
}

#[test]
fn derive_model_context_window_arms_for_curated_dormant_for_unknown() {
    let provider = MockProvider::named("openai", vec![]);
    // A curated model resolves a real window and arms compaction.
    let mut config = AppConfig {
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string(),
        ..AppConfig::default()
    };
    assert!(
        derive_model_context_window(&config, &provider, None).is_some(),
        "curated model must arm compaction with a registry window"
    );
    // An unknown model stays dormant rather than arming off the 272K guess.
    config.model = "no-such-model-zzz-999".to_string();
    assert!(
        derive_model_context_window(&config, &provider, None).is_none(),
        "unknown model must keep mid-turn compaction dormant"
    );
    // An explicit operator window arms it even for an unknown model.
    assert_eq!(
        derive_model_context_window(&config, &provider, Some(123_000)),
        Some(123_000),
    );
}

#[test]
fn model_fits_conversation_skips_reroute_when_window_too_small() {
    let mut config = AppConfig::default();
    let slug = squeezy_core::provider_slug(&config.provider);
    let key = format!("{slug}:cheap-x");
    // ~5K tokens of conversation.
    let convo = vec![LlmInputItem::UserText("x".repeat(20_000))];
    // A tiny pinned window for the cheap model must NOT fit → stay on parent.
    config.model_limits.insert(
        key.clone(),
        squeezy_core::ModelLimitOverride {
            context_window: Some(100),
        },
    );
    assert!(
        !model_fits_conversation(&config, slug, None, "cheap-x", &convo, None),
        "a conversation larger than the cheap model's window must not reroute"
    );
    // A roomy pinned window fits → reroute is allowed.
    config.model_limits.insert(
        key,
        squeezy_core::ModelLimitOverride {
            context_window: Some(1_000_000),
        },
    );
    assert!(
        model_fits_conversation(&config, slug, None, "cheap-x", &convo, None),
        "a conversation that fits the cheap model's window may reroute"
    );
}

#[test]
fn model_fits_conversation_honors_global_override() {
    let config = AppConfig::default();
    let slug = squeezy_core::provider_slug(&config.provider);
    // ~5K tokens; "cheap-y" has no per-model entry and is unknown to the catalog.
    let convo = vec![LlmInputItem::UserText("x".repeat(20_000))];
    // A small GLOBAL [context].model_context_window must still constrain a cheap
    // reroute even with no per-model entry, mirroring the parent path.
    assert!(
        !model_fits_conversation(&config, slug, Some(100), "cheap-y", &convo, None),
        "a small global window must block a cheap reroute"
    );
    assert!(
        model_fits_conversation(&config, slug, Some(1_000_000), "cheap-y", &convo, None),
        "a large global window allows the reroute"
    );
}

#[tokio::test]
async fn mcp_background_queue_serves_issued_tickets_in_order() {
    let queue = Arc::new(McpBackgroundQueue::default());
    let events = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let first = queue.issue_ticket();
    let second = queue.issue_ticket();

    let second_task = {
        let queue = queue.clone();
        let events = events.clone();
        tokio::spawn(async move {
            queue.wait_for_turn(second).await;
            events.lock().await.push("second");
            queue.finish_turn();
        })
    };

    tokio::task::yield_now().await;
    assert!(
        events.lock().await.is_empty(),
        "later ticket must not run before the earlier issued ticket"
    );

    let first_task = {
        let queue = queue.clone();
        let events = events.clone();
        tokio::spawn(async move {
            queue.wait_for_turn(first).await;
            events.lock().await.push("first");
            queue.finish_turn();
        })
    };

    tokio::time::timeout(Duration::from_secs(5), first_task)
        .await
        .expect("first task should finish")
        .expect("first task should not panic");
    tokio::time::timeout(Duration::from_secs(5), second_task)
        .await
        .expect("second task should finish")
        .expect("second task should not panic");

    assert_eq!(&*events.lock().await, &["first", "second"]);
}

// ---------------------------------------------------------------------------
// Cross-surface redaction tests (Idea 3)
// ---------------------------------------------------------------------------

/// Synthetic secret used in cross-surface redaction tests. Uses the
/// `sk-` prefix so it matches the built-in OpenAI-key pattern
/// (`sk-[A-Za-z0-9]{48}`) regardless of how it appears in a field (raw
/// value in metadata, embedded in a command string, etc.). It is also
/// long enough to match regardless of exact length checks in the regex.
/// The `redact_tool_call` test additionally wraps it in an
/// `Authorization: Bearer` header to exercise the bearer-token pattern.
const SYNTHETIC_SECRET: &str = "sk-testkey-abcdefghijklmnopqrstuvwxyz012345678901";

fn test_redactor() -> Arc<Redactor> {
    let config = squeezy_core::RedactionConfig::default();
    Arc::new(config.redactor().expect("redactor"))
}

/// Build a minimal PermissionRequest that embeds the synthetic secret in
/// metadata so we can verify `redact_permission_request` strips it.
fn secret_permission_request() -> PermissionRequest {
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert(
        "command".to_string(),
        format!("OPENAI_API_KEY={SYNTHETIC_SECRET} cargo build"),
    );
    metadata.insert("description".to_string(), SYNTHETIC_SECRET.to_string());
    PermissionRequest {
        call_id: "test-redact".to_string(),
        tool_name: "shell".to_string(),
        capability: PermissionCapability::Shell,
        target: format!("shell:echo {SYNTHETIC_SECRET}"),
        risk: PermissionRisk::High,
        summary: format!("run shell with secret {SYNTHETIC_SECRET}"),
        metadata,
        suggested_rules: vec![],
    }
}

#[test]
fn redact_permission_request_strips_secret_from_metadata_target_and_summary() {
    // Approval prompts are redacted before display to the user and before
    // being emitted in session-log approval events. This test checks that
    // the synthetic secret is absent from every surface the user or a log
    // reader might see.
    let redactor = test_redactor();
    let request = secret_permission_request();

    // Sanity: the raw request contains the secret.
    assert!(
        request.target.contains(SYNTHETIC_SECRET)
            || request.summary.contains(SYNTHETIC_SECRET)
            || request
                .metadata
                .values()
                .any(|v| v.contains(SYNTHETIC_SECRET)),
        "test precondition: secret must appear in raw request"
    );

    let redacted = redact_permission_request(request, &redactor);

    assert!(
        !redacted.target.contains(SYNTHETIC_SECRET),
        "secret must be redacted from target"
    );
    assert!(
        !redacted.summary.contains(SYNTHETIC_SECRET),
        "secret must be redacted from summary"
    );
    for (key, value) in &redacted.metadata {
        assert!(
            !value.contains(SYNTHETIC_SECRET),
            "secret must be redacted from metadata[{key}]"
        );
    }
}

#[test]
fn redact_tool_call_arguments_strips_secret() {
    // Tool-call arguments are redacted before being stored in the session log
    // and before the model sees them as function-call outputs on re-reads.
    let redactor = test_redactor();
    let call = ToolCall {
        call_id: "test-redact-call".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({
            "command": format!("curl -H 'Authorization: Bearer {SYNTHETIC_SECRET}' https://api.example.com"),
            "description": format!("authenticate with {SYNTHETIC_SECRET}")
        }),
    };

    // Sanity: raw call contains secret.
    let raw = call.arguments.to_string();
    assert!(
        raw.contains(SYNTHETIC_SECRET),
        "test precondition: secret must appear in raw arguments"
    );

    let redacted = redact_tool_call(call, &redactor);
    let redacted_str = redacted.arguments.to_string();
    assert!(
        !redacted_str.contains(SYNTHETIC_SECRET),
        "secret must be absent from redacted tool-call arguments; got: {redacted_str:?}"
    );
}

#[tokio::test]
async fn agent_shutdown_renews_mcp_shutdown_token() {
    let agent = Agent::new_ephemeral(AppConfig::default(), Arc::new(MockProvider::new(vec![])));
    let old_child = agent.mcp_shutdown_child_token();

    agent.shutdown().await;

    assert!(
        old_child.is_cancelled(),
        "shutdown must cancel already-issued MCP background tokens"
    );
    assert!(
        !agent.mcp_shutdown_child_token().is_cancelled(),
        "Agent remains reusable after shutdown, so new MCP work must receive a live token"
    );
}

/// Regression: subagent loop previously dropped `LlmEvent::Refusal` deltas,
/// causing an empty summary when the provider stopped with `StopReason::Refusal`.
/// After the fix the refusal prose should appear in the `SubagentFailed.error`,
/// and the refusal round's billable cost must be folded into the subagent's
/// metrics (so the parent's by-subagent ledger never silently reports zero
/// for a refusal round).
#[tokio::test]
async fn subagent_refusal_surfaces_prose_in_failed_event() {
    // Round 1: parent calls `delegate`.
    // Round 2 (subagent): provider emits Refusal delta + Completed(Refusal).
    // Round 3: parent receives SubagentFailed and completes the turn.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "del_refusal".to_string(),
                name: "delegate".to_string(),
                arguments: json!({"prompt": "do something unsafe"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_parent_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::ToolUse),
                reasoning_only_stop: false,
            }),
        ],
        // Subagent round: OpenAI-style refusal delta + Refusal stop reason
        // with non-zero token counts so the cost-fold assertion below has
        // something to bind to.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::Refusal {
                content: "I cannot assist with that request.".to_string(),
            }),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_sub_1".to_string()),
                cost: CostSnapshot {
                    input_tokens: Some(180),
                    output_tokens: Some(40),
                    estimated_usd_micros: Some(12_500),
                    ..CostSnapshot::default()
                },
                stop_reason: Some(StopReason::Refusal),
                reasoning_only_stop: false,
            }),
        ],
        // Parent: acknowledges the failed subagent and finishes.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("understood".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_parent_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::EndTurn),
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(AppConfig::default(), provider);
    let mut rx = agent.start_turn("delegate unsafe task".to_string(), CancellationToken::new());
    let mut failed_error: Option<String> = None;
    let mut failed_metrics: Option<TurnMetrics> = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::SubagentFailed { error, metrics, .. } => {
                failed_error = Some(error);
                failed_metrics = Some(metrics);
            }
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. } => break,
            _ => {}
        }
    }
    let error = failed_error.expect("SubagentFailed must fire when subagent is refused");
    assert!(
        error.contains("refused") || error.contains("cannot"),
        "SubagentFailed error must include refusal prose, got: {error}"
    );
    let metrics = failed_metrics.expect("SubagentFailed must carry the refusal round's metrics");
    assert_eq!(
        metrics.provider.input_tokens,
        Some(180),
        "refusal round's input tokens must be folded into subagent metrics",
    );
    assert_eq!(metrics.provider.output_tokens, Some(40));
    assert_eq!(metrics.provider.estimated_usd_micros, Some(12_500));
}

/// Regression: Anthropic-style providers emit refusal text as ordinary
/// `TextDelta` events rather than `LlmEvent::Refusal` deltas, then close the
/// stream with `StopReason::Refusal` and no tool calls. The subagent loop
/// must fall back to `assistant_message` so the parent's `SubagentFailed`
/// still carries the refusal prose, never an empty error.
#[tokio::test]
async fn subagent_refusal_falls_back_to_text_delta_when_no_refusal_event() {
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: "del_textdelta_refusal".to_string(),
                name: "delegate".to_string(),
                arguments: json!({"prompt": "do something unsafe"}),
            })),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_parent_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::ToolUse),
                reasoning_only_stop: false,
            }),
        ],
        // Subagent round: prose arrives via TextDelta (no `Refusal` event),
        // then the stream closes with StopReason::Refusal.
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta(
                "I cannot help with that request.".to_string(),
            )),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_sub_1".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::Refusal),
                reasoning_only_stop: false,
            }),
        ],
        vec![
            Ok(LlmEvent::Started),
            Ok(LlmEvent::TextDelta("understood".to_string())),
            Ok(LlmEvent::Completed {
                response_id: Some("resp_parent_2".to_string()),
                cost: CostSnapshot::default(),
                stop_reason: Some(StopReason::EndTurn),
                reasoning_only_stop: false,
            }),
        ],
    ]));
    let agent = Agent::new(AppConfig::default(), provider);
    let mut rx = agent.start_turn(
        "delegate text-delta refusal".to_string(),
        CancellationToken::new(),
    );
    let mut failed_error: Option<String> = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::SubagentFailed { error, .. } => {
                failed_error = Some(error);
            }
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. } => break,
            _ => {}
        }
    }
    let error = failed_error.expect(
        "SubagentFailed must fire when the subagent stops with Refusal even \
         without a dedicated `LlmEvent::Refusal` event",
    );
    assert!(
        error.contains("refused") && error.contains("cannot"),
        "SubagentFailed error must propagate the TextDelta refusal prose, got: {error}",
    );
}

/// Regression: the refusal early-return is gated on `tool_calls.is_empty()`.
/// If a provider asks for one or more tool calls *and* signals
/// `StopReason::Refusal`, that contradiction is resolved by executing the
/// requested tools — not by abandoning the round with a refusal error. This
/// test locks the gate in by ensuring the subagent runs to completion
/// successfully when a refusal stop arrives alongside a queued tool call.
///
/// Wrapped in `run_high_stack_async_test` because the subagent's two-round
/// loop nested inside the parent's `TurnRuntime::run` pushes the async
/// state machine past the default thread stack on macOS/ARM64 debug
/// builds. See `docs/internal/TEST_STACK_POSTURE.md`.
#[test]
fn subagent_refusal_does_not_fire_when_tool_calls_pending() {
    run_high_stack_async_test(async {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "del_refusal_with_tools".to_string(),
                    name: "delegate".to_string(),
                    arguments: json!({"prompt": "search for foo"}),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_parent_1".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::ToolUse),
                    reasoning_only_stop: false,
                }),
            ],
            // Subagent round 1: provider emits a Refusal delta *and* a ToolCall,
            // then closes with StopReason::Refusal. The loop must execute the
            // pending tool call rather than early-returning a refusal.
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::Refusal {
                    content: "I cannot do that directly.".to_string(),
                }),
                Ok(LlmEvent::ToolCall(LlmToolCall {
                    call_id: "sub_grep".to_string(),
                    name: "grep".to_string(),
                    arguments: json!({"pattern": "foo", "include": ["*.rs"]}),
                })),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_sub_1".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::Refusal),
                    reasoning_only_stop: false,
                }),
            ],
            // Subagent round 2: tool result returned, subagent emits a normal
            // summary and finishes cleanly. This must produce SubagentCompleted,
            // not SubagentFailed.
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("searched the tree".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_sub_2".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::EndTurn),
                    reasoning_only_stop: false,
                }),
            ],
            // Parent acknowledges the completed subagent and finishes.
            vec![
                Ok(LlmEvent::Started),
                Ok(LlmEvent::TextDelta("done".to_string())),
                Ok(LlmEvent::Completed {
                    response_id: Some("resp_parent_2".to_string()),
                    cost: CostSnapshot::default(),
                    stop_reason: Some(StopReason::EndTurn),
                    reasoning_only_stop: false,
                }),
            ],
        ]));
        let agent = Agent::new(AppConfig::default(), provider);
        let mut rx = agent.start_turn(
            "delegate refusal-with-tools".to_string(),
            CancellationToken::new(),
        );
        let mut subagent_failed = false;
        let mut subagent_completed = false;
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::SubagentFailed { .. } => subagent_failed = true,
                AgentEvent::SubagentCompleted { .. } => subagent_completed = true,
                AgentEvent::Completed { .. } | AgentEvent::Failed { .. } => break,
                _ => {}
            }
        }
        assert!(
            !subagent_failed,
            "refusal early-return must NOT fire when tool calls are pending",
        );
        assert!(
            subagent_completed,
            "subagent should run its pending tool call to completion when a \
             Refusal stop arrives alongside ToolCall events",
        );
    });
}

fn local_shell_failure_result(
    command: &str,
    exit_code: i64,
    stderr: &str,
) -> squeezy_tools::ToolResult {
    squeezy_tools::ToolResult {
        call_id: "shell-call".to_string(),
        tool_name: "shell".to_string(),
        status: ToolStatus::Error,
        content: json!({
            "command": command,
            "stdout": "",
            "stderr": stderr,
            "exit_code": exit_code,
            "error": format!("exit {exit_code}"),
        }),
        cost_hint: ToolCostHint::default(),
        receipt: ToolReceipt {
            output_sha256: String::new(),
            content_sha256: None,
        },
        spill_model_output: None,
        web_call_stats: None,
    }
}

#[test]
fn local_tool_completion_message_appends_shell_hint_on_nonempty_stderr() {
    // stderr came back populated (the shell printed a syntax-error message,
    // a `permission denied`, etc.). Surface the effective-shell hint so the
    // user knows which shell to retarget via `SQUEEZY_SHELL`.
    let result = local_shell_failure_result("echo $(", 1, "sh: 1: syntax error");
    let message = super::local_tool_completion_message(Some(&result));
    assert!(message.contains("sh: 1: syntax error"), "{message}");
    assert!(
        message.contains("set SQUEEZY_SHELL to change"),
        "stderr-populated failure must include the shell hint: {message}",
    );
}

#[test]
fn local_tool_completion_message_appends_shell_hint_on_shellish_exit_codes() {
    // 127 = command not found, 126 = not executable, 2 = bash/sh syntax
    // error. All three are shell-attributable even when stderr happens to
    // be empty (some shells emit to stdout, mocks may drop it), so the
    // hint should still appear.
    for exit_code in [2, 126, 127] {
        let result = local_shell_failure_result("nope", exit_code, "");
        let message = super::local_tool_completion_message(Some(&result));
        assert!(
            message.contains("set SQUEEZY_SHELL to change"),
            "exit_code {exit_code} must surface the shell hint even with empty stderr: {message}",
        );
    }
}

#[test]
fn local_tool_completion_message_omits_shell_hint_for_benign_failures() {
    // A `grep` that finds nothing exits 1 with empty stderr — the failure
    // is informational, not shell-attributable. Suggesting a shell change
    // here would be noise.
    let result = local_shell_failure_result("grep needle file", 1, "");
    let message = super::local_tool_completion_message(Some(&result));
    assert!(
        !message.contains("set SQUEEZY_SHELL to change"),
        "exit-1 with empty stderr must NOT spuriously suggest the shell: {message}",
    );
}

#[test]
fn local_tool_completion_message_omits_shell_hint_on_success() {
    // Successful completions never carry the failure hint.
    let mut result = local_shell_failure_result("ls", 0, "");
    result.status = ToolStatus::Success;
    result.content = json!({
        "command": "ls",
        "stdout": "foo bar",
        "stderr": "",
        "exit_code": 0,
    });
    let message = super::local_tool_completion_message(Some(&result));
    assert!(
        !message.contains("set SQUEEZY_SHELL to change"),
        "success path must not surface the shell hint: {message}",
    );
}
