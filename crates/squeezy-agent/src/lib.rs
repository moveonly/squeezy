use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs, io,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, RwLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_core::Stream;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextAttachmentSource, ContextAttachmentStatus,
    ContextCompactionRecord, ContextCompactionState, ContextCompactionTrigger, ContextEstimate,
    ContextPin, CostSnapshot, DEFAULT_ANTHROPIC_MODEL, DEFAULT_AZURE_OPENAI_MODEL,
    DEFAULT_BEDROCK_MODEL, DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES, DEFAULT_GOOGLE_MODEL,
    DEFAULT_OLLAMA_MODEL, DEFAULT_OPENAI_MODEL, PROJECT_SETTINGS_FILE, PermissionAction,
    PermissionCapability, PermissionRequest, PermissionRule, PermissionRuleSource, PermissionScope,
    PermissionVerdict, ProviderConfig, Redactor, ResponseVerbosity, Role, SessionMetrics,
    SessionMode, SqueezyError, StreamRedactor, SubagentConfig, TaskStateSnapshot, TaskStateStatus,
    ToolSchemaConfig, TranscriptItem, TurnId, TurnMetrics, context_attachment_preview,
    context_attachment_storage_text, default_settings_path, detect_context_attachment_kind,
    escape_toml_basic_string,
};
use squeezy_llm::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, LlmToolSpec,
    RequestTokenEstimate, capabilities_for, estimate_cost, estimate_request_context,
    fetch_ollama_context_window,
};
use squeezy_skills::{HelpAnswer, SqueezyHelp, matches_squeezy_help_input};
use squeezy_store::{
    BugReportBundle, BugReportOptions, CleanupReport, ResumeItem, SessionEvent, SessionHandle,
    SessionMetadata, SessionQuery, SessionRecord, SessionReplayEvent, SessionReplayEventKind,
    SessionReplayTape, SessionResumeState, SessionStatus, SessionStore, SqueezyStore,
    StoredReadSnapshot, StoredToolReceipt,
};
use squeezy_telemetry::{
    ErrorKind, FeedbackClient, FeedbackSubmitResult, PreparedFeedback, ReportUpload,
    TelemetryClient, TelemetryEvent, ToolCostProperties, ToolStatusKind as TelemetryToolStatusKind,
    ToolTelemetryReport, prepare_feedback,
};
use squeezy_tools::{
    McpElicitationHandler, McpElicitationRequest, McpElicitationResponse, McpStatusSnapshot,
    ToolCall, ToolCostHint, ToolOutputConfig, ToolReceipt, ToolRegistry, ToolRegistryRuntime,
    ToolResult, ToolRuntimeConfig, ToolSpec, ToolStatus, WebToolConfig, sha256_hex,
};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod exploration_compiler;

use exploration_compiler::{ExplorationTurnState, compile_exploration_plan};

const MAX_TOOL_ROUNDS: usize = 32;
const MAX_CONTROL_ONLY_TOOL_ROUNDS: usize = 2;
const LOCAL_SHELL_TIMEOUT_MS: u64 = 10_000;
const LOCAL_SHELL_OUTPUT_BYTE_CAP: usize = 32_000;
const TASK_STATE_TOOL_NAME: &str = "update_task_state";
const LOAD_TOOL_SCHEMA_TOOL_NAME: &str = "load_tool_schema";
const DELEGATE_TOOL_NAME: &str = "delegate";
const EXPLORE_TOOL_NAME: &str = "explore";
pub const MAX_JOB_NOTIFICATIONS: usize = 20;
pub const MAX_JOBS_RETAINED: usize = 200;
const JOB_SUMMARY_MAX_CHARS: usize = 320;
const SUBAGENT_SUMMARY_CHARS_PER_TOKEN: usize = 4;
// Compaction summary truncation budgets. These are character (not byte)
// caps because they pass through `compact_text` → `truncate_chars`. They
// stay collocated so a future audit can read the total summary growth
// in one place rather than chasing literals across `build_compaction_summary`.
const COMPACTION_PREVIOUS_SUMMARY_MAX_CHARS: usize = 1_200;
const COMPACTION_PIN_SUMMARY_MAX_CHARS: usize = 400;
const COMPACTION_DURABLE_LINE_MAX_CHARS: usize = 320;
const COMPACTION_TOOL_ARGS_MAX_CHARS: usize = 260;
const COMPACTION_TOOL_OUTPUT_MAX_CHARS: usize = 260;
const COMPACTION_RECEIPT_MAX_CHARS: usize = 260;
const COMPACTION_UNRESOLVED_MAX_CHARS: usize = 240;
const COMPACTION_ATTACHMENT_PREVIEW_MAX_CHARS: usize = 220;
const COMPACTION_DURABLE_LINES_LIMIT: usize = 24;
const COMPACTION_UNRESOLVED_LINES_LIMIT: usize = 8;
const COMPACTION_RECEIPT_LINES_LIMIT: usize = 12;
const COMPACTION_MAX_HISTORY: usize = 20;

async fn next_llm_stream_event(
    stream: &mut LlmStream,
    cancel: &CancellationToken,
    idle_timeout: Duration,
) -> squeezy_core::Result<Option<LlmEvent>> {
    let next = tokio::select! {
        _ = cancel.cancelled() => return Ok(Some(LlmEvent::Cancelled)),
        next = tokio::time::timeout(idle_timeout, stream.next()) => next,
    };
    match next {
        Ok(Some(event)) => event.map(Some),
        Ok(None) => Ok(None),
        Err(_) => Err(SqueezyError::ProviderStream(format!(
            "idle timeout waiting for model stream after {}ms",
            idle_timeout.as_millis()
        ))),
    }
}

#[derive(Debug, Clone, Default)]
struct ConversationState {
    previous_response_id: Option<String>,
    conversation: Vec<LlmInputItem>,
    transcript: Vec<TranscriptItem>,
    context_attachments: Vec<ContextAttachment>,
    context_compaction: ContextCompactionState,
    cost: CostSnapshot,
    metrics: SessionMetrics,
    redactions: u64,
}

impl ConversationState {
    fn from_resume(state: SessionResumeState, metadata: &SessionMetadata) -> Self {
        Self {
            previous_response_id: state.previous_response_id,
            conversation: state
                .conversation
                .into_iter()
                .map(resume_item_to_llm_input)
                .collect(),
            transcript: state.transcript,
            context_attachments: state.context_attachments,
            context_compaction: state.context_compaction,
            cost: metadata.cost.clone(),
            metrics: metadata.metrics.clone(),
            redactions: metadata.redactions,
        }
    }

    fn to_resume_state(&self) -> SessionResumeState {
        SessionResumeState {
            resume_available: true,
            previous_response_id: self.previous_response_id.clone(),
            conversation: self
                .conversation
                .iter()
                .cloned()
                .map(llm_input_to_resume_item)
                .collect(),
            transcript: self.transcript.clone(),
            context_attachments: self.context_attachments.clone(),
            context_compaction: self.context_compaction.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReplayReport {
    pub session_id: String,
    pub turns: usize,
    pub events_replayed: usize,
    pub request_count: usize,
    pub tool_results: usize,
    pub final_answer: String,
}

#[derive(Debug)]
struct ReplayRuntime {
    tape: SessionReplayTape,
    cursor: StdMutex<usize>,
    strict_requests: bool,
}

impl ReplayRuntime {
    fn new(tape: SessionReplayTape, strict_requests: bool) -> Self {
        Self {
            tape,
            cursor: StdMutex::new(0),
            strict_requests,
        }
    }

    fn model_events_for_request(
        &self,
        request: &LlmRequest,
    ) -> Vec<squeezy_core::Result<LlmEvent>> {
        match self.try_model_events_for_request(request) {
            Ok(events) => events.into_iter().map(Ok).collect(),
            Err(error) => vec![Err(error)],
        }
    }

    fn try_model_events_for_request(
        &self,
        request: &LlmRequest,
    ) -> squeezy_core::Result<Vec<LlmEvent>> {
        let request_event = self.pop_expected(SessionReplayEventKind::ModelRequest)?;
        let expected = request_event
            .payload
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let actual = replay_hash(request);
        if self.strict_requests && expected != actual {
            return Err(SqueezyError::Agent(format!(
                "replay model request diverged: expected {expected}, got {actual}"
            )));
        }

        let mut events = Vec::new();
        loop {
            let event = self.pop_next_non_user()?;
            match event.kind {
                SessionReplayEventKind::ModelStarted => events.push(LlmEvent::Started),
                SessionReplayEventKind::ModelTextDelta => events.push(LlmEvent::TextDelta(
                    event
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )),
                SessionReplayEventKind::ModelToolCall => {
                    let call = serde_json::from_value::<LlmToolCall>(
                        event.payload.get("call").cloned().unwrap_or(Value::Null),
                    )
                    .map_err(|err| {
                        SqueezyError::Agent(format!("invalid replay model tool call: {err}"))
                    })?;
                    events.push(LlmEvent::ToolCall(call));
                }
                SessionReplayEventKind::ModelCompleted => {
                    let response_id = event
                        .payload
                        .get("response_id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    let cost = serde_json::from_value::<CostSnapshot>(
                        event.payload.get("cost").cloned().unwrap_or(Value::Null),
                    )
                    .unwrap_or_default();
                    events.push(LlmEvent::Completed { response_id, cost });
                    return Ok(events);
                }
                SessionReplayEventKind::ModelCancelled => {
                    events.push(LlmEvent::Cancelled);
                    return Ok(events);
                }
                other => {
                    return Err(SqueezyError::Agent(format!(
                        "unexpected replay event while reading model stream: {other:?}"
                    )));
                }
            }
        }
    }

    fn replay_tool_results(&self, calls: &[ToolCall]) -> squeezy_core::Result<Vec<ToolResult>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let call_event = self.pop_expected(SessionReplayEventKind::ToolCall)?;
            let expected = call_event
                .payload
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let actual = replay_hash(call);
            if expected != actual {
                return Err(SqueezyError::Agent(format!(
                    "replay tool call diverged for {}: expected {expected}, got {actual}",
                    call.call_id
                )));
            }

            let result_event = self.pop_expected(SessionReplayEventKind::ToolResult)?;
            let mut result = serde_json::from_value::<ToolResult>(
                result_event
                    .payload
                    .get("result")
                    .cloned()
                    .unwrap_or(Value::Null),
            )
            .map_err(|err| SqueezyError::Agent(format!("invalid replay tool result: {err}")))?;
            if result.call_id != call.call_id {
                return Err(SqueezyError::Agent(format!(
                    "replay tool result call_id diverged: expected {}, got {}",
                    call.call_id, result.call_id
                )));
            }
            if let Some(model_output) = result_event
                .payload
                .get("model_output")
                .and_then(Value::as_str)
            {
                result = result.with_spill_model_output(model_output.to_string());
            }
            results.push(result);
        }
        Ok(results)
    }

    fn consumed(&self) -> usize {
        *self.cursor.lock().expect("replay cursor")
    }

    fn finish(&self) -> squeezy_core::Result<()> {
        let mut cursor = self.cursor.lock().expect("replay cursor");
        while let Some(event) = self.tape.events.get(*cursor) {
            if matches!(
                event.kind,
                SessionReplayEventKind::UserMessage | SessionReplayEventKind::CostDecision
            ) {
                *cursor += 1;
                continue;
            }
            return Err(SqueezyError::Agent(format!(
                "replay finished with unconsumed event {:?} at sequence {}",
                event.kind, event.sequence
            )));
        }
        Ok(())
    }

    fn request_count(&self) -> usize {
        self.tape
            .events
            .iter()
            .filter(|event| event.kind == SessionReplayEventKind::ModelRequest)
            .count()
    }

    fn tool_result_count(&self) -> usize {
        self.tape
            .events
            .iter()
            .filter(|event| event.kind == SessionReplayEventKind::ToolResult)
            .count()
    }

    fn pop_expected(
        &self,
        expected: SessionReplayEventKind,
    ) -> squeezy_core::Result<SessionReplayEvent> {
        let event = self.pop_next_non_user()?;
        if event.kind == expected {
            return Ok(event);
        }
        Err(SqueezyError::Agent(format!(
            "unexpected replay event: expected {expected:?}, got {:?}",
            event.kind
        )))
    }

    fn pop_next_non_user(&self) -> squeezy_core::Result<SessionReplayEvent> {
        let mut cursor = self.cursor.lock().expect("replay cursor");
        while let Some(event) = self.tape.events.get(*cursor) {
            *cursor += 1;
            if !matches!(
                event.kind,
                SessionReplayEventKind::UserMessage | SessionReplayEventKind::CostDecision
            ) {
                return Ok(event.clone());
            }
        }
        Err(SqueezyError::Agent(
            "replay trace ended before the agent turn completed".to_string(),
        ))
    }
}

#[derive(Debug)]
struct ReplayProvider {
    name: &'static str,
    runtime: Arc<ReplayRuntime>,
}

impl ReplayProvider {
    fn new(name: &'static str, runtime: Arc<ReplayRuntime>) -> Self {
        Self { name, runtime }
    }
}

impl LlmProvider for ReplayProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(
        &self,
        request: LlmRequest,
        _cancel: CancellationToken,
    ) -> squeezy_llm::LlmStream {
        let events = self.runtime.model_events_for_request(&request);
        let stream: Pin<Box<dyn Stream<Item = squeezy_core::Result<LlmEvent>> + Send>> =
            Box::pin(futures_util::stream::iter(events));
        stream
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextAttachmentUpdate {
    pub attachment: ContextAttachment,
    pub duplicate: bool,
    pub active: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptShape {
    pub items: usize,
    pub user: usize,
    pub assistant: usize,
    pub system: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationShape {
    pub items: usize,
    pub user_text: usize,
    pub assistant_text: usize,
    pub function_calls: usize,
    pub function_outputs: usize,
    pub text_bytes: usize,
    pub tool_output_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentShape {
    pub total: usize,
    pub active: usize,
    pub removed: usize,
    pub unsupported: usize,
    pub stored_bytes: usize,
    pub redactions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAccountingSnapshot {
    pub session_id: Option<String>,
    pub provider: &'static str,
    pub model: String,
    pub mode: SessionMode,
    pub store_responses: bool,
    pub previous_response_id: Option<String>,
    pub cost: CostSnapshot,
    pub metrics: SessionMetrics,
    pub redactions: u64,
    pub transcript: TranscriptShape,
    pub conversation: ConversationShape,
    pub attachments: AttachmentShape,
    pub transmitted_request: RequestTokenEstimate,
    pub full_history_request: RequestTokenEstimate,
}

impl SessionAccountingSnapshot {
    pub fn provider_stored_context_active(&self) -> bool {
        self.store_responses && self.previous_response_id.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCompactionReport {
    pub record: ContextCompactionRecord,
    pub summary: String,
}

pub type JobId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Shell,
    Verify,
    Indexing,
    Benchmark,
    Compaction,
    Tool,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Verify => "verify",
            Self::Indexing => "indexing",
            Self::Benchmark => "benchmark",
            Self::Compaction => "compaction",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobProgress {
    pub completed: Option<u64>,
    pub total: Option<u64>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSnapshot {
    pub id: JobId,
    pub kind: JobKind,
    pub status: JobStatus,
    pub title: String,
    pub progress: Option<JobProgress>,
    pub result_summary: Option<String>,
    pub output_handle: Option<String>,
    pub turn_id: Option<TurnId>,
    pub tool_name: Option<String>,
    pub call_id: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub ended_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobNotification {
    pub job_id: JobId,
    pub kind: JobKind,
    pub status: JobStatus,
    pub title: String,
    pub summary: String,
    pub ts_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    Updated(JobSnapshot),
    Notification(JobNotification),
}

#[derive(Clone)]
pub struct JobRegistry {
    state: Arc<std::sync::Mutex<JobRegistryState>>,
    next_id: Arc<AtomicU64>,
    tx: broadcast::Sender<JobEvent>,
}

#[derive(Debug, Default)]
struct JobRegistryState {
    jobs: BTreeMap<JobId, JobRecord>,
    notifications: VecDeque<JobNotification>,
}

#[derive(Debug, Clone)]
struct JobRecord {
    snapshot: JobSnapshot,
    cancel: CancellationToken,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(128);
        Self {
            state: Arc::new(std::sync::Mutex::new(JobRegistryState::default())),
            next_id: Arc::new(AtomicU64::new(1)),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> Vec<JobSnapshot> {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state
            .jobs
            .values()
            .map(|record| record.snapshot.clone())
            .collect()
    }

    pub fn notifications(&self) -> Vec<JobNotification> {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.notifications.iter().cloned().collect()
    }

    pub fn get(&self, id: JobId) -> Option<JobSnapshot> {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.jobs.get(&id).map(|record| record.snapshot.clone())
    }

    pub fn create(
        &self,
        kind: JobKind,
        title: impl Into<String>,
        turn_id: Option<TurnId>,
        tool_name: Option<String>,
        call_id: Option<String>,
        cancel: CancellationToken,
    ) -> JobSnapshot {
        let now = unix_timestamp_millis();
        let snapshot = JobSnapshot {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            kind,
            status: JobStatus::Queued,
            title: title.into(),
            progress: Some(JobProgress {
                completed: None,
                total: None,
                message: "queued".to_string(),
            }),
            result_summary: None,
            output_handle: None,
            turn_id,
            tool_name,
            call_id,
            created_at_ms: now,
            updated_at_ms: now,
            ended_at_ms: None,
        };
        self.update_record(snapshot.clone(), Some(cancel), false);
        snapshot
    }

    pub fn start(&self, id: JobId) -> Option<JobSnapshot> {
        self.update(id, false, |snapshot| {
            snapshot.status = JobStatus::Running;
            snapshot.progress = Some(JobProgress {
                completed: None,
                total: None,
                message: "running".to_string(),
            });
        })
    }

    pub fn progress(
        &self,
        id: JobId,
        completed: Option<u64>,
        total: Option<u64>,
        message: impl Into<String>,
    ) -> Option<JobSnapshot> {
        self.update(id, false, |snapshot| {
            snapshot.progress = Some(JobProgress {
                completed,
                total,
                message: message.into(),
            });
        })
    }

    pub fn finish(
        &self,
        id: JobId,
        status: JobStatus,
        summary: impl Into<String>,
        output_handle: Option<String>,
    ) -> Option<JobSnapshot> {
        let summary = truncate_chars(&summary.into(), JOB_SUMMARY_MAX_CHARS);
        self.update(id, true, |snapshot| {
            snapshot.status = status;
            snapshot.result_summary = Some(summary);
            snapshot.output_handle = output_handle;
            snapshot.progress = Some(JobProgress {
                completed: Some(1),
                total: Some(1),
                message: status.as_str().to_string(),
            });
            snapshot.ended_at_ms = Some(unix_timestamp_millis());
        })
    }

    pub fn cancel(&self, id: JobId) -> bool {
        let cancel = {
            let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            let Some(record) = state.jobs.get(&id) else {
                return false;
            };
            if !record.snapshot.status.is_active() {
                return false;
            }
            record.cancel.clone()
        };
        cancel.cancel();
        let _ = self.progress(id, None, None, "cancellation requested");
        true
    }

    fn update(
        &self,
        id: JobId,
        notify: bool,
        update: impl FnOnce(&mut JobSnapshot),
    ) -> Option<JobSnapshot> {
        let (snapshot, notification) = {
            let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            let record = state.jobs.get_mut(&id)?;
            update(&mut record.snapshot);
            record.snapshot.updated_at_ms = unix_timestamp_millis();
            let snapshot = record.snapshot.clone();
            let notification = if notify {
                push_job_notification(&mut state, &snapshot);
                state.notifications.back().cloned()
            } else {
                None
            };
            if snapshot.status.is_terminal() {
                prune_completed_jobs(&mut state);
            }
            (snapshot, notification)
        };
        let _ = self.tx.send(JobEvent::Updated(snapshot.clone()));
        if let Some(notification) = notification {
            let _ = self.tx.send(JobEvent::Notification(notification));
        }
        Some(snapshot)
    }

    fn update_record(
        &self,
        snapshot: JobSnapshot,
        cancel: Option<CancellationToken>,
        notify: bool,
    ) {
        let notification = {
            let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            let record = JobRecord {
                snapshot: snapshot.clone(),
                cancel: cancel.unwrap_or_default(),
            };
            state.jobs.insert(snapshot.id, record);
            let notification = if notify {
                push_job_notification(&mut state, &snapshot);
                state.notifications.back().cloned()
            } else {
                None
            };
            prune_completed_jobs(&mut state);
            notification
        };
        let _ = self.tx.send(JobEvent::Updated(snapshot));
        if let Some(notification) = notification {
            let _ = self.tx.send(JobEvent::Notification(notification));
        }
    }
}

fn push_job_notification(state: &mut JobRegistryState, snapshot: &JobSnapshot) {
    let summary = snapshot
        .result_summary
        .clone()
        .or_else(|| {
            snapshot
                .progress
                .as_ref()
                .map(|progress| progress.message.clone())
        })
        .unwrap_or_else(|| snapshot.status.as_str().to_string());
    state.notifications.push_back(JobNotification {
        job_id: snapshot.id,
        kind: snapshot.kind,
        status: snapshot.status,
        title: snapshot.title.clone(),
        summary,
        ts_unix_ms: unix_timestamp_millis(),
    });
    while state.notifications.len() > MAX_JOB_NOTIFICATIONS {
        state.notifications.pop_front();
    }
}

fn prune_completed_jobs(state: &mut JobRegistryState) {
    if state.jobs.len() <= MAX_JOBS_RETAINED {
        return;
    }
    let mut terminal: Vec<(JobId, u64)> = state
        .jobs
        .iter()
        .filter(|(_, record)| record.snapshot.status.is_terminal())
        .map(|(id, record)| (*id, record.snapshot.ended_at_ms.unwrap_or(0)))
        .collect();
    terminal.sort_by_key(|(_, ended_at)| *ended_at);
    let mut to_remove = state.jobs.len().saturating_sub(MAX_JOBS_RETAINED);
    for (id, _) in terminal {
        if to_remove == 0 {
            break;
        }
        state.jobs.remove(&id);
        to_remove -= 1;
    }
}

#[derive(Clone)]
pub struct Agent {
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    jobs: JobRegistry,
    telemetry: TelemetryClient,
    redactor: Arc<Redactor>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
    session_log: Option<SessionHandle>,
    conversation_state: Arc<Mutex<ConversationState>>,
    next_turn_id: Arc<AtomicU64>,
    next_approval_id: Arc<AtomicU64>,
    next_attachment_id: Arc<AtomicU64>,
    /// In-memory permission rules added via "Allow user/project rule" during
    /// the current process. Persisted to disk on a best-effort basis; this
    /// vector also makes the rule take effect immediately for subsequent
    /// tool calls without having to wait for a settings reload.
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    /// Active session mode. Stored as an `AtomicU8` so reads on the hot
    /// permission/advertisement paths cannot deadlock, cannot be poisoned by
    /// a panicking writer, and never need a fallback enum value: every byte
    /// we observe was previously written via `SessionMode::to_u8`.
    session_mode: Arc<AtomicU8>,
    loaded_tool_schemas: Arc<Mutex<Vec<String>>>,
    store: Option<Arc<SqueezyStore>>,
    replay: Option<Arc<ReplayRuntime>>,
}

impl Agent {
    pub fn new(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Self {
        let session_log = start_session_log(&config, provider.name());
        Self::build(
            config,
            provider,
            session_log,
            ConversationState::default(),
            None,
        )
    }

    pub fn resume(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_id: &str,
    ) -> squeezy_core::Result<(Self, Vec<TranscriptItem>)> {
        let store = SessionStore::open(&config);
        let handle = store.open_session(session_id.to_string());
        let resume_state = handle.read_resume_state()?;
        if !resume_state.resume_available {
            return Err(SqueezyError::Agent(format!(
                "session {session_id} is not resumable"
            )));
        }
        let metadata = handle.metadata()?;
        let transcript = resume_state.transcript.clone();
        let conversation_state = ConversationState::from_resume(resume_state, &metadata);
        let agent = Self::build(
            config,
            provider,
            Some(handle.clone()),
            conversation_state,
            None,
        );
        let _ = handle.update_metadata(|metadata| {
            metadata.status = SessionStatus::Running;
            metadata.ended_at_ms = None;
            metadata.resume_available = true;
        });
        let _ = handle.append_event(SessionEvent::new(
            "session_resumed",
            None,
            Some("session resumed".to_string()),
            json!({}),
        ));
        Ok((agent, transcript))
    }

    pub async fn replay_session(
        mut config: AppConfig,
        session_id: &str,
    ) -> squeezy_core::Result<SessionReplayReport> {
        let store = SessionStore::open(&config);
        let record = store.show(session_id)?;
        let tape = record.replay.clone().ok_or_else(|| {
            SqueezyError::Agent(format!("session {session_id} has no replay tape"))
        })?;
        if tape.events.is_empty() {
            return Err(SqueezyError::Agent(format!(
                "session {session_id} has an empty replay tape"
            )));
        }
        if tape.warnings > 0 {
            return Err(SqueezyError::Agent(format!(
                "session {session_id} replay tape has {} unreadable events",
                tape.warnings
            )));
        }

        let recorded_root = PathBuf::from(&record.metadata.workspace_root);
        if recorded_root.exists() {
            config.workspace_root = recorded_root;
        }
        Self::replay_tape(
            config,
            session_id,
            tape,
            &record.metadata.provider,
            record.metadata.model,
            record.metadata.mode,
        )
        .await
    }

    pub async fn replay_tape(
        mut config: AppConfig,
        session_id: impl Into<String>,
        tape: SessionReplayTape,
        provider_name: &str,
        model: String,
        mode: SessionMode,
    ) -> squeezy_core::Result<SessionReplayReport> {
        let session_id = session_id.into();
        config.model = model;
        config.session_mode = mode;
        let user_inputs = replay_user_inputs(&tape);
        if user_inputs.is_empty() {
            return Err(SqueezyError::Agent(format!(
                "session {session_id} replay tape has no user turns"
            )));
        }

        let runtime = Arc::new(ReplayRuntime::new(tape, true));
        let provider = Arc::new(ReplayProvider::new(
            replay_provider_name(provider_name),
            runtime.clone(),
        ));
        let agent = Self::build(
            config,
            provider,
            None,
            ConversationState::default(),
            Some(runtime.clone()),
        );

        let mut final_answer = String::new();
        for input in &user_inputs {
            let mut rx = agent.start_turn(input.clone(), CancellationToken::new());
            while let Some(event) = rx.recv().await {
                match event {
                    AgentEvent::AssistantDelta { delta, .. } => final_answer.push_str(&delta),
                    AgentEvent::Completed { message, .. } if final_answer.is_empty() => {
                        final_answer = message.content;
                    }
                    AgentEvent::Failed { error, .. } => return Err(error),
                    _ => {}
                }
            }
        }

        runtime.finish()?;
        Ok(SessionReplayReport {
            session_id,
            turns: user_inputs.len(),
            events_replayed: runtime.consumed(),
            request_count: runtime.request_count(),
            tool_results: runtime.tool_result_count(),
            final_answer,
        })
    }

    fn build(
        mut config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_log: Option<SessionHandle>,
        conversation_state: ConversationState,
        replay: Option<Arc<ReplayRuntime>>,
    ) -> Self {
        let output_config = ToolOutputConfig {
            spill_threshold_bytes: config.tool_spill_threshold_bytes,
            preview_bytes: config.tool_preview_bytes,
            retention_days: config.tool_output_retention_days,
            output_dir: config.cache.tool_outputs.clone(),
        };
        let web_config = WebToolConfig {
            exa_mcp_url: config.exa_mcp_url.clone(),
            exa_api_key: env::var(&config.exa_api_key_env).ok(),
        };
        // Compile the redactor exactly once and share it with the tool
        // registry. Pattern compilation can never fail here because the
        // surrounding config was already validated when loading.
        let redactor = Arc::new(
            config
                .redaction
                .redactor()
                .expect("validated redaction config must compile"),
        );
        // Open the persistent state store exactly once and share the handle
        // with the tool registry. redb only allows a single live `Database`
        // per file (see `state_store_open_rejects_a_second_handle_on_the_same_file`),
        // so the registry's graph manager must reuse this handle instead of
        // opening its own — otherwise the second open would fail silently
        // and graph partitions would never be persisted.
        let store = SqueezyStore::open(&config.workspace_root, config.cache.root.as_deref())
            .ok()
            .map(Arc::new);
        let registry_runtime = ToolRegistryRuntime::new(store.clone(), redactor.clone());
        let tools = ToolRegistry::new_with_configs_skills_and_mcp(
            config.workspace_root.clone(),
            ToolRuntimeConfig {
                output: output_config.clone(),
                web: web_config.clone(),
                shell_sandbox: config.permissions.shell_sandbox.clone(),
                mcp_servers: config.mcp_servers.clone(),
                checkpoints_enabled: config.checkpoints_enabled,
            },
            config.skills.clone(),
            &config.graph,
            registry_runtime.clone(),
        )
        .unwrap_or_else(|_| {
            // Workspace root unavailable; fall back to the current
            // directory but keep the configured redactor and graph
            // policy so the agent never silently downgrades to
            // default patterns or default crawl options.
            ToolRegistry::new_with_configs_skills_and_mcp(
                ".",
                ToolRuntimeConfig {
                    output: output_config,
                    web: web_config,
                    shell_sandbox: config.permissions.shell_sandbox.clone(),
                    mcp_servers: config.mcp_servers.clone(),
                    checkpoints_enabled: config.checkpoints_enabled,
                },
                config.skills.clone(),
                &config.graph,
                registry_runtime,
            )
            .expect("current directory must be a valid tool root")
        });
        if let Some(preamble) = tools.skills_preamble() {
            if preamble.omitted_count > 0 {
                log_session_event(
                    session_log.as_ref(),
                    &redactor,
                    "skills_preamble_truncated",
                    None,
                    Some(format!(
                        "{} skill(s) omitted from available skills preamble",
                        preamble.omitted_count
                    )),
                    json!({ "omitted_count": preamble.omitted_count }),
                );
            }
            config.instructions = format!("{}\n\n{}", config.instructions, preamble.body);
        }
        let ambiguous_skills = tools.ambiguous_skill_names();
        if !ambiguous_skills.is_empty() {
            log_session_event(
                session_log.as_ref(),
                &redactor,
                "skills_warning",
                None,
                Some(format!(
                    "{} ambiguous skill name(s) require explicit selection",
                    ambiguous_skills.len()
                )),
                json!({ "ambiguous_names": ambiguous_skills }),
            );
        }
        let initial_session_mode = config.session_mode;
        let session_metrics = Arc::new(Mutex::new(conversation_state.metrics.clone()));
        let next_attachment_id = next_attachment_counter(&conversation_state.context_attachments);
        Self {
            telemetry: TelemetryClient::from_config(&config),
            config,
            provider,
            tools,
            jobs: JobRegistry::new(),
            redactor,
            session_metrics,
            session_log,
            conversation_state: Arc::new(Mutex::new(conversation_state)),
            next_turn_id: Arc::new(AtomicU64::new(1)),
            next_approval_id: Arc::new(AtomicU64::new(1)),
            next_attachment_id: Arc::new(AtomicU64::new(next_attachment_id)),
            session_rules: Arc::new(RwLock::new(Vec::new())),
            session_mode: Arc::new(AtomicU8::new(initial_session_mode.to_u8())),
            loaded_tool_schemas: Arc::new(Mutex::new(Vec::new())),
            store,
            replay,
        }
    }

    /// Snapshot of session-scoped permission rules. Primarily intended for
    /// tests and debug surfaces; the live rule list lives behind a lock and
    /// is consulted on every permission decision.
    pub fn session_rules_snapshot(&self) -> Vec<PermissionRule> {
        self.session_rules
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    pub fn session_mode(&self) -> SessionMode {
        load_session_mode(&self.session_mode)
    }

    /// Set the current session mode. Returns true when the mode actually
    /// changed so callers (notably the TUI) can avoid emitting "switched to"
    /// status when the request was a no-op.
    pub fn set_session_mode(&self, mode: SessionMode, source: &'static str) -> bool {
        let previous_u8 = self.session_mode.swap(mode.to_u8(), Ordering::AcqRel);
        let previous = SessionMode::from_u8(previous_u8).unwrap_or_else(|| {
            // Unreachable in practice: every write goes through this method
            // or the constructor, both of which use `to_u8`. Log defensively
            // and treat it as a real change so the new value still wins.
            tracing::warn!(
                target: "squeezy::permissions",
                discriminant = previous_u8,
                "unexpected session mode discriminant; treating as different",
            );
            match mode {
                SessionMode::Plan => SessionMode::Build,
                SessionMode::Build => SessionMode::Plan,
            }
        });
        if previous == mode {
            return false;
        }
        log_session_mode_transition(previous, mode, source);
        true
    }

    pub fn toggle_session_mode(&self, source: &'static str) -> SessionMode {
        let next = match self.session_mode() {
            SessionMode::Plan => SessionMode::Build,
            SessionMode::Build => SessionMode::Plan,
        };
        self.set_session_mode(next, source);
        next
    }

    /// Execute a single tool call from the TUI / local UX path rather than
    /// from inside an agent turn. The "manual" group id mirrors how the agent
    /// labels human-driven invocations so checkpoint grouping stays
    /// consistent across both entry points.
    pub async fn execute_local_tool(&self, call: ToolCall) -> ToolResult {
        self.tools
            .execute_for_group(call, CancellationToken::new(), "manual".to_string())
            .await
    }

    pub fn subscribe_jobs(&self) -> broadcast::Receiver<JobEvent> {
        self.jobs.subscribe()
    }

    pub fn jobs_snapshot(&self) -> Vec<JobSnapshot> {
        self.jobs.snapshot()
    }

    pub fn job_notifications(&self) -> Vec<JobNotification> {
        self.jobs.notifications()
    }

    pub fn job_snapshot(&self, id: JobId) -> Option<JobSnapshot> {
        self.jobs.get(id)
    }

    pub fn cancel_job(&self, id: JobId) -> bool {
        self.jobs.cancel(id)
    }

    pub fn start_local_tool_job(&self, call: ToolCall) -> JobSnapshot {
        let kind = job_kind_for_tool(&call.name).unwrap_or(JobKind::Tool);
        let title = self.tools.describe_call(&call);
        let cancel = CancellationToken::new();
        let snapshot = self.jobs.create(
            kind,
            title,
            None,
            Some(call.name.clone()),
            Some(call.call_id.clone()),
            cancel.clone(),
        );
        log_job_lifecycle(
            self.session_log.as_ref(),
            &self.redactor,
            "job_queued",
            &snapshot,
        );
        let tools = self.tools.clone();
        let jobs = self.jobs.clone();
        let session_log = self.session_log.clone();
        let redactor = self.redactor.clone();
        let job_id = snapshot.id;
        tokio::spawn(async move {
            if let Some(started) = jobs.start(job_id) {
                log_job_lifecycle(session_log.as_ref(), &redactor, "job_started", &started);
            }
            let result = tools
                .execute_for_group(call, cancel, format!("job-{job_id}"))
                .await;
            let status = job_status_for_tool_status(result.status);
            let summary = tool_result_summary(&result);
            let output_handle = tool_result_output_handle(&result);
            if let Some(done) = jobs.finish(job_id, status, summary, output_handle) {
                log_job_lifecycle(session_log.as_ref(), &redactor, "job_finished", &done);
            }
        });
        snapshot
    }

    pub async fn flush_telemetry(&self) {
        let _ = self.telemetry.flush().await;
    }

    pub fn session_id(&self) -> Option<String> {
        self.session_log
            .as_ref()
            .map(|handle| handle.session_id().to_string())
    }

    pub async fn session_accounting_snapshot(&self) -> SessionAccountingSnapshot {
        let state = self.conversation_state.lock().await.clone();
        let mode = load_session_mode(&self.session_mode);
        let context_window_override = match &self.config.provider {
            ProviderConfig::Ollama(ollama) => {
                fetch_ollama_context_window(&ollama.base_url, &self.config.model).await
            }
            _ => None,
        };
        let loaded_tool_schemas = self.loaded_tool_schemas.lock().await.clone();
        let full_history_request = self.accounting_request(
            state.conversation.clone(),
            None,
            false,
            mode,
            self.config.store_responses,
            &loaded_tool_schemas,
        );
        let transmitted_input =
            if self.config.store_responses && state.previous_response_id.is_some() {
                Vec::new()
            } else {
                state.conversation.clone()
            };
        let transmitted_request = self.accounting_request(
            transmitted_input,
            state.previous_response_id.clone(),
            self.config.store_responses,
            mode,
            self.config.store_responses,
            &loaded_tool_schemas,
        );
        SessionAccountingSnapshot {
            session_id: self.session_id(),
            provider: self.provider.name(),
            model: self.config.model.clone(),
            mode,
            store_responses: self.config.store_responses,
            previous_response_id: state.previous_response_id.clone(),
            cost: state.cost,
            metrics: state.metrics,
            redactions: state.redactions,
            transcript: transcript_shape(&state.transcript),
            conversation: conversation_shape(&state.conversation),
            attachments: attachment_shape(&state.context_attachments),
            transmitted_request: estimate_request_context(
                self.provider.name(),
                &self.config.model,
                &transmitted_request,
                context_window_override,
            ),
            full_history_request: estimate_request_context(
                self.provider.name(),
                &self.config.model,
                &full_history_request,
                context_window_override,
            ),
        }
    }

    fn accounting_request(
        &self,
        input: Vec<LlmInputItem>,
        previous_response_id: Option<String>,
        store: bool,
        mode: SessionMode,
        include_response_state: bool,
        loaded_tool_schemas: &[String],
    ) -> LlmRequest {
        let native_text_verbosity = capabilities_for(self.provider.name(), &self.config.model)
            .is_some_and(|capabilities| capabilities.text_verbosity);
        let raw_instructions = instructions_with_response_verbosity(
            &self.config.instructions,
            self.config.tui.response_verbosity,
            native_text_verbosity,
        );
        let request_instructions = self.redactor.redact(&raw_instructions).text;
        let mut all_tool_specs = core_control_tools(&self.config.subagents);
        all_tool_specs.extend(self.tools.specs().iter().cloned().map(advertised_tool));
        LlmRequest {
            model: self.config.model.clone(),
            instructions: instructions_with_tool_index(
                &request_instructions,
                &all_tool_specs,
                mode,
                &self.config.tools,
            ),
            input: redact_llm_input_items(&input, &self.redactor),
            max_output_tokens: self.config.max_output_tokens,
            response_verbosity: request_response_verbosity(&self.config, self.provider.name()),
            reasoning_effort: request_reasoning_effort(&self.config, self.provider.name()),
            previous_response_id: if include_response_state {
                previous_response_id
            } else {
                None
            },
            tools: request_tool_specs(
                &all_tool_specs,
                mode,
                &self.config.tools,
                loaded_tool_schemas,
            ),
            store,
        }
    }

    pub fn list_sessions(
        &self,
        query: &SessionQuery,
    ) -> squeezy_core::Result<Vec<SessionMetadata>> {
        SessionStore::open(&self.config).list(query)
    }

    pub fn show_session(&self, session_id: &str) -> squeezy_core::Result<SessionRecord> {
        SessionStore::open(&self.config).show(session_id)
    }

    pub fn export_session(&self, session_id: &str) -> squeezy_core::Result<Value> {
        SessionStore::open(&self.config).export(session_id)
    }

    pub fn prepare_feedback(&self, message: &str) -> squeezy_core::Result<PreparedFeedback> {
        prepare_feedback(&self.config, message, "tui")
    }

    pub async fn submit_feedback(
        &self,
        feedback: &PreparedFeedback,
    ) -> squeezy_core::Result<FeedbackSubmitResult> {
        FeedbackClient::from_config(&self.config)
            .submit_feedback(feedback)
            .await
            .map_err(|error| SqueezyError::Tool(error.to_string()))
    }

    pub fn build_bug_report(
        &self,
        session_id: &str,
        options: BugReportOptions,
    ) -> squeezy_core::Result<BugReportBundle> {
        SessionStore::open(&self.config).build_bug_report(&self.config, session_id, options)
    }

    pub async fn submit_bug_report(
        &self,
        bundle: &BugReportBundle,
    ) -> squeezy_core::Result<FeedbackSubmitResult> {
        let sections = bundle
            .sections
            .iter()
            .map(|section| section.name.clone())
            .collect::<Vec<_>>();
        FeedbackClient::from_config(&self.config)
            .submit_report(ReportUpload {
                report_id: &bundle.report_id,
                session_id: &bundle.session_id,
                archive_bytes: &bundle.archive_bytes,
                redactions: bundle.redactions,
                sections,
                source: "tui",
            })
            .await
            .map_err(|error| SqueezyError::Tool(error.to_string()))
    }

    pub fn cleanup_sessions(&self, ids: &[String]) -> squeezy_core::Result<CleanupReport> {
        // Refuse to delete the session that this agent is currently writing
        // to. Removing it under our feet would orphan future event writes and
        // leave a session that no longer exists on disk but still appears in
        // `metadata`/`resume_state` until the process exits.
        let active = self.session_id();
        if let Some(active_id) = &active
            && ids.iter().any(|id| id == active_id)
        {
            return Err(SqueezyError::Agent(format!(
                "refusing to clean up the active session {active_id}; finish or exit first"
            )));
        }
        SessionStore::open(&self.config).cleanup_excluding(ids, active.as_deref())
    }

    pub fn resume_current(
        &mut self,
        session_id: &str,
    ) -> squeezy_core::Result<Vec<TranscriptItem>> {
        let (agent, transcript) =
            Self::resume(self.config.clone(), self.provider.clone(), session_id)?;
        *self = agent;
        Ok(transcript)
    }

    pub async fn finish_session(&self, status: SessionStatus) {
        let Some(session) = &self.session_log else {
            return;
        };
        let state = self.conversation_state.lock().await.clone();
        let _ = session.write_resume_state(&state.to_resume_state());
        let _ = session.finish(status, state.cost, state.metrics, state.redactions);
    }

    pub async fn attach_pasted_context(
        &self,
        text: String,
    ) -> squeezy_core::Result<ContextAttachmentUpdate> {
        self.attach_context_bytes(
            ContextAttachmentSource::Paste,
            "pasted context".to_string(),
            None,
            text.into_bytes(),
        )
        .await
    }

    pub async fn attach_file_context(
        &self,
        path: PathBuf,
    ) -> squeezy_core::Result<ContextAttachmentUpdate> {
        let resolved = if path.is_absolute() {
            path
        } else {
            self.config.workspace_root.join(path)
        };
        let bytes = fs::read(&resolved)?;
        let label = resolved
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attached file")
            .to_string();
        let display_path = resolved
            .strip_prefix(&self.config.workspace_root)
            .unwrap_or(&resolved)
            .display()
            .to_string();
        self.attach_context_bytes(
            ContextAttachmentSource::File,
            label,
            Some(display_path),
            bytes,
        )
        .await
    }

    pub async fn detach_context_attachment(
        &self,
        id: &str,
    ) -> squeezy_core::Result<ContextAttachment> {
        let mut state = self.conversation_state.lock().await;
        let Some(index) = state
            .context_attachments
            .iter()
            .position(|attachment| attachment.id == id && attachment.is_active())
        else {
            return Err(SqueezyError::Agent(format!(
                "attachment {id} is not active"
            )));
        };
        state.context_attachments[index].status = ContextAttachmentStatus::Removed;
        let attachment = state.context_attachments[index].clone();
        self.persist_context_attachments(&state)?;
        if let Some(session) = &self.session_log {
            let _ = session.write_context_attachment(&attachment, None);
        }
        drop(state);
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "context_removed",
            None,
            Some(format!("removed {}", attachment.id)),
            json!({ "attachment": attachment.clone() }),
        );
        Ok(attachment)
    }

    pub async fn context_attachments_snapshot(&self) -> Vec<ContextAttachment> {
        self.conversation_state
            .lock()
            .await
            .context_attachments
            .iter()
            .filter(|attachment| attachment.is_active())
            .cloned()
            .collect()
    }

    pub async fn context_compaction_snapshot(&self) -> ContextCompactionState {
        self.conversation_state
            .lock()
            .await
            .context_compaction
            .clone()
    }

    pub async fn context_estimate_snapshot(&self) -> ContextEstimate {
        let state = self.conversation_state.lock().await;
        estimate_context(&state.conversation)
    }

    pub async fn compact_context_manual(&self) -> squeezy_core::Result<ContextCompactionReport> {
        let mut state = self.conversation_state.lock().await;
        let mut conversation = state.conversation.clone();
        let mut context_compaction = state.context_compaction.clone();
        let attachments = state.context_attachments.clone();
        let report = compact_conversation(
            &mut conversation,
            &mut context_compaction,
            &attachments,
            self.store.as_deref(),
            &self.config,
            ContextCompactionTrigger::Manual,
            true,
        )
        .ok_or_else(|| SqueezyError::Agent("not enough context to compact".to_string()))?;
        state.conversation = conversation;
        state.context_compaction = context_compaction;
        state.previous_response_id = None;
        if let Some(session) = &self.session_log {
            session.write_resume_state(&state.to_resume_state())?;
        }
        drop(state);
        self.log_compaction_event(&report);
        Ok(report)
    }

    pub async fn pin_context_entry(
        &self,
        label: String,
        summary: String,
        source: String,
    ) -> squeezy_core::Result<ContextPin> {
        let mut state = self.conversation_state.lock().await;
        let pin = ContextPin {
            id: next_context_pin_id(&state.context_compaction.pinned),
            label: truncate_chars(&collapse_status_text(&label), 80),
            summary: truncate_chars(&collapse_status_text(&summary), 800),
            source: truncate_chars(&collapse_status_text(&source), 80),
            created_unix_ms: unix_timestamp_millis(),
        };
        state.context_compaction.pinned.push(pin.clone());
        if let Some(session) = &self.session_log {
            session.write_resume_state(&state.to_resume_state())?;
        }
        drop(state);
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "context_pin_added",
            None,
            Some(format!("pinned {}", pin.id)),
            json!({ "pin": pin.clone() }),
        );
        Ok(pin)
    }

    pub async fn unpin_context_entry(&self, id: &str) -> squeezy_core::Result<ContextPin> {
        let mut state = self.conversation_state.lock().await;
        let Some(index) = state
            .context_compaction
            .pinned
            .iter()
            .position(|pin| pin.id == id)
        else {
            return Err(SqueezyError::Agent(format!("pin {id} not found")));
        };
        let pin = state.context_compaction.pinned.remove(index);
        if let Some(session) = &self.session_log {
            session.write_resume_state(&state.to_resume_state())?;
        }
        drop(state);
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "context_pin_removed",
            None,
            Some(format!("unpinned {}", pin.id)),
            json!({ "pin": pin.clone() }),
        );
        Ok(pin)
    }

    fn log_compaction_event(&self, report: &ContextCompactionReport) {
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "context_compacted",
            None,
            Some(format!(
                "compacted context gen={} {}->{} estimated tokens",
                report.record.generation,
                report.record.before.estimated_tokens,
                report.record.after.estimated_tokens
            )),
            json!({
                "record": report.record,
                "summary": report.summary,
            }),
        );
    }

    async fn attach_context_bytes(
        &self,
        source: ContextAttachmentSource,
        label: String,
        path: Option<String>,
        bytes: Vec<u8>,
    ) -> squeezy_core::Result<ContextAttachmentUpdate> {
        let original_sha256 = sha256_hex(&bytes);
        let original_bytes = bytes.len();
        let text = std::str::from_utf8(&bytes).ok();
        let kind = detect_context_attachment_kind(Some(&label), &bytes, text);

        let mut state = self.conversation_state.lock().await;
        if let Some(existing) = state
            .context_attachments
            .iter()
            .find(|attachment| {
                attachment.original_sha256 == original_sha256 && attachment.is_active()
            })
            .cloned()
        {
            drop(state);
            log_session_event(
                self.session_log.as_ref(),
                &self.redactor,
                "context_deduped",
                None,
                Some(format!("deduped {}", existing.id)),
                json!({ "attachment": existing.clone() }),
            );
            return Ok(ContextAttachmentUpdate {
                attachment: existing,
                duplicate: true,
                active: true,
            });
        }

        let id = self.next_context_attachment_id();
        let redacted_label = self.redactor.redact(&label).text;
        let redacted_path = path.map(|value| self.redactor.redact(&value).text);
        if !kind.is_supported_text() {
            let attachment = ContextAttachment {
                id,
                source,
                kind,
                status: ContextAttachmentStatus::Unsupported,
                label: redacted_label,
                path: redacted_path,
                original_sha256,
                redacted_sha256: None,
                original_bytes,
                stored_bytes: 0,
                preview_bytes: 0,
                redactions: 0,
                preview: String::new(),
                truncated: false,
            };
            state.context_attachments.push(attachment.clone());
            self.persist_context_attachments(&state)?;
            if let Some(session) = &self.session_log {
                let _ = session.write_context_attachment(&attachment, None);
            }
            drop(state);
            log_session_event(
                self.session_log.as_ref(),
                &self.redactor,
                "context_unsupported",
                None,
                Some(format!("unsupported {}", attachment.id)),
                json!({ "attachment": attachment.clone() }),
            );
            return Ok(ContextAttachmentUpdate {
                attachment,
                duplicate: false,
                active: false,
            });
        }

        let text = text.unwrap_or_default();
        let (bounded_text, truncated) =
            context_attachment_storage_text(text, DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES);
        let redacted = self.redactor.redact(&bounded_text);
        let (preview, _) =
            context_attachment_preview(&redacted.text, self.config.tool_preview_bytes);
        let attachment = ContextAttachment {
            id,
            source,
            kind,
            status: ContextAttachmentStatus::Attached,
            label: redacted_label,
            path: redacted_path,
            original_sha256,
            redacted_sha256: Some(sha256_hex(redacted.text.as_bytes())),
            original_bytes,
            stored_bytes: redacted.text.len(),
            preview_bytes: preview.len(),
            redactions: redacted.redactions,
            preview,
            truncated,
        };
        state.redactions += attachment.redactions;
        state.context_attachments.push(attachment.clone());
        self.persist_context_attachments(&state)?;
        if let Some(session) = &self.session_log {
            let _ = session.write_context_attachment(&attachment, Some(&redacted.text));
        }
        drop(state);
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "context_attached",
            None,
            Some(format!("attached {}", attachment.id)),
            json!({ "attachment": attachment.clone() }),
        );
        Ok(ContextAttachmentUpdate {
            attachment,
            duplicate: false,
            active: true,
        })
    }

    fn persist_context_attachments(&self, state: &ConversationState) -> squeezy_core::Result<()> {
        // Only persist resume state here. `metadata.resume_available` is set
        // to `true` at session start and `metadata.redactions` is re-synced
        // by `persist_turn_state` on the next completed turn, so we avoid
        // the redundant read-modify-write of `metadata.json` (which also
        // keeps the session_id-bearing metadata out of the attachment flow
        // for static analyzers).
        if let Some(session) = &self.session_log {
            session.write_resume_state(&state.to_resume_state())?;
        }
        Ok(())
    }

    fn next_context_attachment_id(&self) -> String {
        let next = self.next_attachment_id.fetch_add(1, Ordering::Relaxed);
        format!("att-{next:04}")
    }

    pub fn start_turn(
        &self,
        input: String,
        cancel: CancellationToken,
    ) -> mpsc::Receiver<AgentEvent> {
        self.start_turn_with_response_verbosity(input, cancel, self.config.tui.response_verbosity)
    }

    pub fn start_turn_with_response_verbosity(
        &self,
        input: String,
        cancel: CancellationToken,
        response_verbosity: ResponseVerbosity,
    ) -> mpsc::Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel(128);
        let provider = self.provider.clone();
        let mut config = self.config.clone();
        config.tui.response_verbosity = response_verbosity;
        let tools = self.tools.clone();
        let jobs = self.jobs.clone();
        let telemetry = self.telemetry.clone();
        let redactor = self.redactor.clone();
        let session_metrics = self.session_metrics.clone();
        let turn_id = TurnId::new(self.next_turn_id.fetch_add(1, Ordering::Relaxed));
        let approval_ids = self.next_approval_id.clone();
        let session_rules = self.session_rules.clone();
        let session_mode = self.session_mode.clone();
        let session_log = self.session_log.clone();
        let conversation_state = self.conversation_state.clone();
        let store = self.store.clone();
        let task_state = Arc::new(Mutex::new(None));
        let loaded_tool_schemas = self.loaded_tool_schemas.clone();
        let replay = self.replay.clone();

        tokio::spawn(async move {
            let redacted_input = redactor.redact(&input);
            let task_title = redacted_input.text.clone();
            let failure_session_log = session_log.clone();
            // Echo the user message into the TUI before kicking MCP
            // discovery so a slow/flaky external server never delays the
            // prompt the user just submitted.
            if tx
                .send(AgentEvent::UserMessage {
                    turn_id,
                    message: TranscriptItem::user(redacted_input.text.clone()),
                })
                .await
                .is_err()
            {
                return;
            }
            if let Some(call) = local_shell_command_call(&task_title) {
                complete_local_tool_turn(
                    turn_id,
                    task_title,
                    call,
                    redacted_input.redactions,
                    LocalToolTurnDeps {
                        tx: tx.clone(),
                        provider: provider.clone(),
                        tools: tools.clone(),
                        jobs: jobs.clone(),
                        redactor: redactor.clone(),
                        session_log: session_log.clone(),
                        conversation_state: conversation_state.clone(),
                        session_metrics: session_metrics.clone(),
                        telemetry: telemetry.clone(),
                        config: config.clone(),
                        task_state: task_state.clone(),
                        session_mode: session_mode.clone(),
                        cancel: cancel.clone(),
                        approval_ids: approval_ids.clone(),
                        session_rules: session_rules.clone(),
                        loaded_tool_schemas: loaded_tool_schemas.clone(),
                    },
                )
                .await;
                return;
            }
            // Cheap pre-check first so unrelated coding turns do not pay for a
            // full `inspect_redacted()` rendering on every turn.
            if matches_squeezy_help_input(&task_title)
                && let Some(answer) =
                    SqueezyHelp::new(config.inspect_redacted()).answer_for_input(&task_title)
            {
                complete_squeezy_help_turn(
                    turn_id,
                    task_title,
                    answer,
                    redacted_input.redactions,
                    HelpTurnDeps {
                        tx: tx.clone(),
                        redactor: redactor.clone(),
                        session_log: session_log.clone(),
                        conversation_state: conversation_state.clone(),
                        session_metrics: session_metrics.clone(),
                        telemetry: telemetry.clone(),
                        config: config.clone(),
                        task_state: task_state.clone(),
                        session_mode: session_mode.clone(),
                    },
                )
                .await;
                return;
            }
            let mut all_tool_specs = core_control_tools(&config.subagents);
            all_tool_specs.extend(tools.specs().iter().cloned().map(advertised_tool));
            warn_unknown_tool_schema_names(&all_tool_specs, &config.tools);
            refresh_mcp_tools_in_background(
                tools.clone(),
                cancel.clone(),
                session_log.clone(),
                redactor.clone(),
                tx.clone(),
                turn_id,
            );

            let outcome = TurnRuntime {
                turn_id,
                provider,
                config,
                tools,
                jobs,
                telemetry: telemetry.clone(),
                redactor: redactor.clone(),
                session_metrics,
                all_tool_specs,
                tx: tx.clone(),
                cancel,
                approval_ids,
                seed_redactions: redacted_input.redactions,
                session_rules,
                session_mode,
                session_log,
                conversation_state,
                store,
                task_state: task_state.clone(),
                loaded_tool_schemas,
                replay,
            }
            .run(task_title.clone())
            .await;

            if let Err(error) = outcome {
                let error = redact_error(error, &redactor);
                let latest_task_state = task_state.lock().await.clone();
                publish_task_state_update(
                    &tx,
                    failure_session_log.as_ref(),
                    &redactor,
                    &task_state,
                    turn_id,
                    TaskStateSnapshot::terminal_from(
                        latest_task_state.as_ref(),
                        task_title,
                        TaskStateStatus::Failed,
                        Some(error.to_string()),
                    ),
                )
                .await;
                if let Some(session) = failure_session_log {
                    let _ = session.append_event(SessionEvent::new(
                        "failed",
                        Some(turn_id.to_string()),
                        Some(error.to_string()),
                        json!({ "error": error.to_string() }),
                    ));
                    let _ = session.update_metadata(|metadata| {
                        metadata.status = SessionStatus::Failed;
                        metadata.latest_summary = Some(error.to_string());
                    });
                }
                telemetry.spawn(TelemetryEvent::failure_seen(error_kind(&error)));
                let _ = tx.send(AgentEvent::Failed { turn_id, error }).await;
            }
        });

        rx
    }
}

struct HelpTurnDeps {
    tx: mpsc::Sender<AgentEvent>,
    redactor: Arc<Redactor>,
    session_log: Option<SessionHandle>,
    conversation_state: Arc<Mutex<ConversationState>>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
    telemetry: TelemetryClient,
    config: AppConfig,
    task_state: Arc<Mutex<Option<TaskStateSnapshot>>>,
    session_mode: Arc<AtomicU8>,
}

struct LocalToolTurnDeps {
    tx: mpsc::Sender<AgentEvent>,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    jobs: JobRegistry,
    redactor: Arc<Redactor>,
    session_log: Option<SessionHandle>,
    conversation_state: Arc<Mutex<ConversationState>>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
    telemetry: TelemetryClient,
    config: AppConfig,
    task_state: Arc<Mutex<Option<TaskStateSnapshot>>>,
    session_mode: Arc<AtomicU8>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    loaded_tool_schemas: Arc<Mutex<Vec<String>>>,
}

async fn complete_squeezy_help_turn(
    turn_id: TurnId,
    task_title: String,
    answer: HelpAnswer,
    seed_redactions: u64,
    deps: HelpTurnDeps,
) {
    let HelpTurnDeps {
        tx,
        redactor,
        session_log,
        conversation_state,
        session_metrics,
        telemetry,
        config,
        task_state,
        session_mode,
    } = deps;
    let user_item = LlmInputItem::UserText(task_title.clone());
    let user_transcript = TranscriptItem::user(task_title.clone());
    let rendered = redactor.redact(&answer.render_markdown());
    let message = TranscriptItem::assistant(rendered.text);
    let metrics = TurnMetrics {
        redactions: seed_redactions + rendered.redactions,
        ..TurnMetrics::default()
    };
    let cost = CostSnapshot::default();

    log_session_event(
        session_log.as_ref(),
        &redactor,
        "user_message",
        Some(turn_id),
        user_item_summary(&user_item),
        json!({}),
    );
    publish_task_state_update(
        &tx,
        session_log.as_ref(),
        &redactor,
        &task_state,
        turn_id,
        TaskStateSnapshot::starting(task_title.clone()),
    )
    .await;
    let _ = tx.send(AgentEvent::Started { turn_id }).await;
    let _ = tx
        .send(AgentEvent::AssistantDelta {
            turn_id,
            delta: message.content.clone(),
        })
        .await;
    let latest_task_state = task_state.lock().await.clone();
    publish_task_state_update(
        &tx,
        session_log.as_ref(),
        &redactor,
        &task_state,
        turn_id,
        TaskStateSnapshot::terminal_from(
            latest_task_state.as_ref(),
            task_title.clone(),
            TaskStateStatus::Completed,
            Some(format!("Squeezy help: {}", answer.topic)),
        ),
    )
    .await;

    {
        let mut state = conversation_state.lock().await;
        state.conversation.push(user_item);
        state
            .conversation
            .push(LlmInputItem::AssistantText(message.content.clone()));
        // Help turns never call the provider, so any prior response-chain id
        // (e.g. OpenAI Responses) stays valid for the next real turn. Leaving
        // it untouched avoids forcing the following turn to resend full history.
        state.transcript.push(user_transcript);
        state.transcript.push(message.clone());
        merge_cost(&mut state.cost, &cost);
        state.metrics.merge_turn(&metrics);
        state.redactions += metrics.redactions;
        if let Some(session) = &session_log {
            let _ = session.write_resume_state(&state.to_resume_state());
            let _ = session.update_metadata(|metadata| {
                metadata.cost = state.cost.clone();
                metadata.metrics = state.metrics.clone();
                metadata.redactions = state.redactions;
                metadata.resume_available = true;
                metadata.mode = load_session_mode(&session_mode);
            });
        }
    }

    log_session_event(
        session_log.as_ref(),
        &redactor,
        "squeezy_help",
        Some(turn_id),
        Some(answer.topic.clone()),
        json!({
            "topic": answer.topic,
            "status": answer.status,
            "citations": answer.citations,
            "config_sections": answer.config_sections,
        }),
    );
    log_session_event(
        session_log.as_ref(),
        &redactor,
        "assistant_completed",
        Some(turn_id),
        Some(format!(
            "Squeezy help: {}",
            message.content.lines().next().unwrap_or("help")
        )),
        json!({
            "response_id": null,
            "cost": cost,
            "metrics": metrics,
        }),
    );

    telemetry.spawn(TelemetryEvent::turn_completed(
        &config,
        turn_id.get(),
        metrics.clone(),
    ));
    session_metrics.lock().await.merge_turn(&metrics);
    let _ = tx
        .send(AgentEvent::Completed {
            turn_id,
            message,
            response_id: None,
            cost,
            metrics,
        })
        .await;
}

async fn complete_local_tool_turn(
    turn_id: TurnId,
    task_title: String,
    call: ToolCall,
    seed_redactions: u64,
    deps: LocalToolTurnDeps,
) {
    let LocalToolTurnDeps {
        tx,
        provider,
        tools,
        jobs,
        redactor,
        session_log,
        conversation_state,
        session_metrics,
        telemetry,
        config,
        task_state,
        session_mode,
        cancel,
        approval_ids,
        session_rules,
        loaded_tool_schemas,
    } = deps;
    let user_item = LlmInputItem::UserText(task_title.clone());
    let user_transcript = TranscriptItem::user(task_title.clone());

    log_session_event(
        session_log.as_ref(),
        &redactor,
        "user_message",
        Some(turn_id),
        user_item_summary(&user_item),
        json!({}),
    );
    publish_task_state_update(
        &tx,
        session_log.as_ref(),
        &redactor,
        &task_state,
        turn_id,
        TaskStateSnapshot::starting(task_title.clone()),
    )
    .await;
    let _ = tx.send(AgentEvent::Started { turn_id }).await;
    let _ = tx
        .send(AgentEvent::ToolCallQueued {
            turn_id,
            call: redact_tool_call(call.clone(), &redactor),
        })
        .await;

    let all_tool_specs = Vec::new();
    let exploration_state = Arc::new(Mutex::new(ExplorationTurnState::from_plan(None)));
    let mut broker = CostBroker::new(&config);
    let results = execute_tool_calls(
        vec![call],
        ToolExecutionContext {
            turn_id,
            provider,
            tools: &tools,
            jobs: &jobs,
            config: &config,
            telemetry: telemetry.clone(),
            redactor: redactor.clone(),
            tx: tx.clone(),
            cancel,
            approval_ids,
            session_rules,
            session_mode: session_mode.clone(),
            session_log: session_log.clone(),
            task_state: task_state.clone(),
            all_tool_specs: &all_tool_specs,
            loaded_tool_schemas,
            exploration_state,
        },
        &mut broker,
    )
    .await;

    let message_text = local_tool_completion_message(results.first());
    let rendered = redactor.redact(&message_text);
    let message = TranscriptItem::assistant(rendered.text);
    let mut metrics = broker.metrics.clone();
    metrics.redactions += seed_redactions + rendered.redactions;
    let cost = CostSnapshot::default();
    let _ = tx
        .send(AgentEvent::AssistantDelta {
            turn_id,
            delta: message.content.clone(),
        })
        .await;
    let terminal_status = if results
        .first()
        .is_some_and(|result| result.status == ToolStatus::Success)
    {
        TaskStateStatus::Completed
    } else {
        TaskStateStatus::Failed
    };
    let terminal_summary = results
        .first()
        .map(|result| {
            if result.status == ToolStatus::Success {
                "local command completed".to_string()
            } else {
                format!("local command failed: {}", tool_failure_detail(result))
            }
        })
        .unwrap_or_else(|| "local command produced no result".to_string());
    let latest_task_state = task_state.lock().await.clone();
    publish_task_state_update(
        &tx,
        session_log.as_ref(),
        &redactor,
        &task_state,
        turn_id,
        TaskStateSnapshot::terminal_from(
            latest_task_state.as_ref(),
            task_title.clone(),
            terminal_status,
            Some(terminal_summary),
        ),
    )
    .await;

    {
        let mut state = conversation_state.lock().await;
        state.conversation.push(user_item);
        state
            .conversation
            .push(LlmInputItem::AssistantText(message.content.clone()));
        state.transcript.push(user_transcript);
        state.transcript.push(message.clone());
        merge_cost(&mut state.cost, &cost);
        state.metrics.merge_turn(&metrics);
        state.redactions += metrics.redactions;
        if let Some(session) = &session_log {
            let _ = session.write_resume_state(&state.to_resume_state());
            let _ = session.update_metadata(|metadata| {
                metadata.cost = state.cost.clone();
                metadata.metrics = state.metrics.clone();
                metadata.redactions = state.redactions;
                metadata.resume_available = true;
                metadata.mode = load_session_mode(&session_mode);
            });
        }
    }

    log_session_event(
        session_log.as_ref(),
        &redactor,
        "local_command",
        Some(turn_id),
        results.first().map(tool_result_summary),
        json!({ "command": task_title }),
    );
    log_session_event(
        session_log.as_ref(),
        &redactor,
        "assistant_completed",
        Some(turn_id),
        Some("local workspace command".to_string()),
        json!({
            "response_id": null,
            "cost": cost,
            "metrics": metrics,
        }),
    );

    telemetry.spawn(TelemetryEvent::turn_completed(
        &config,
        turn_id.get(),
        metrics.clone(),
    ));
    session_metrics.lock().await.merge_turn(&metrics);
    let _ = tx
        .send(AgentEvent::Completed {
            turn_id,
            message,
            response_id: None,
            cost,
            metrics,
        })
        .await;
}

fn refresh_mcp_tools_in_background(
    tools: ToolRegistry,
    cancel: CancellationToken,
    session_log: Option<SessionHandle>,
    redactor: Arc<Redactor>,
    tx: mpsc::Sender<AgentEvent>,
    turn_id: TurnId,
) {
    tokio::spawn(async move {
        let outcome = tools.refresh_mcp_tools(cancel).await;
        log_session_event(
            session_log.as_ref(),
            &redactor,
            "mcp_status_updated",
            Some(turn_id),
            None,
            serde_json::to_value(&outcome.status).unwrap_or_else(|_| json!({})),
        );
        let _ = tx
            .send(AgentEvent::McpStatusUpdated {
                turn_id,
                snapshot: outcome.status.clone(),
            })
            .await;
        for error in outcome.errors {
            log_session_event(
                session_log.as_ref(),
                &redactor,
                "mcp_discovery_error",
                Some(turn_id),
                Some(error.clone()),
                json!({ "error": error }),
            );
        }
    });
}

fn local_shell_command_call(input: &str) -> Option<ToolCall> {
    let command = local_shell_command(input)?;
    Some(ToolCall {
        call_id: "local-shell-1".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": command,
            "description": "run the user-requested local command",
            "timeout_ms": LOCAL_SHELL_TIMEOUT_MS,
            "output_byte_cap": LOCAL_SHELL_OUTPUT_BYTE_CAP,
            "output_mode": "raw",
            "direct_user_shell": true,
        }),
    })
}

fn local_shell_command(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.lines().count() > 1 {
        return None;
    }
    trimmed.strip_prefix('!').and_then(nonempty_shell_command)
}

fn nonempty_shell_command(command: &str) -> Option<String> {
    let command = command.trim().trim_matches('`').trim();
    (!command.is_empty()).then(|| command.to_string())
}

fn local_tool_completion_message(result: Option<&ToolResult>) -> String {
    let Some(result) = result else {
        return "Local command produced no result.".to_string();
    };
    let command = result
        .content
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("local command");
    let stdout = result
        .content
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let stderr = result
        .content
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    match result.status {
        ToolStatus::Success => {
            if !stdout.is_empty() {
                stdout.to_string()
            } else if !stderr.is_empty() {
                stderr.to_string()
            } else {
                format!("`{command}` completed successfully.")
            }
        }
        ToolStatus::Cancelled => format!("`{command}` was cancelled."),
        _ => {
            let detail = tool_failure_detail(result);
            if !stderr.is_empty() {
                format!("`{command}` failed: {detail}\n\n{stderr}")
            } else {
                format!("`{command}` failed: {detail}")
            }
        }
    }
}

struct TurnRuntime {
    turn_id: TurnId,
    provider: Arc<dyn LlmProvider>,
    config: AppConfig,
    tools: ToolRegistry,
    jobs: JobRegistry,
    telemetry: TelemetryClient,
    redactor: Arc<Redactor>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
    all_tool_specs: Vec<AdvertisedTool>,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
    // Redactions that already happened on the raw user input before the
    // turn loop began; folded into the first round's metrics so the
    // session metric never undercounts user-side scrubbing.
    seed_redactions: u64,
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    session_mode: Arc<AtomicU8>,
    session_log: Option<SessionHandle>,
    conversation_state: Arc<Mutex<ConversationState>>,
    store: Option<Arc<SqueezyStore>>,
    task_state: Arc<Mutex<Option<TaskStateSnapshot>>>,
    loaded_tool_schemas: Arc<Mutex<Vec<String>>>,
    replay: Option<Arc<ReplayRuntime>>,
}

fn request_response_verbosity(
    config: &AppConfig,
    provider_name: &str,
) -> Option<ResponseVerbosity> {
    capabilities_for(provider_name, &config.model)
        .filter(|capabilities| capabilities.text_verbosity)
        .map(|_| config.tui.response_verbosity)
}

fn request_reasoning_effort(
    config: &AppConfig,
    provider_name: &str,
) -> Option<squeezy_core::ReasoningEffort> {
    let effort = config.reasoning_effort?;
    capabilities_for(provider_name, &config.model)
        .filter(|capabilities| capabilities.reasoning_effort)
        .map(|_| effort)
}

/// Appends a "Pinned context" block to the per-turn instructions.
///
/// Pins are user-curated durable facts. They must be visible to the
/// model on every turn — not only after compaction lands the summary —
/// otherwise `/pin` is purely UI until the conversation crosses the
/// compaction threshold. Each pin contributes one line; long summaries
/// are clipped via `compact_text` so the instructions stay bounded.
fn instructions_with_pinned_context(instructions: &str, pinned: &[ContextPin]) -> String {
    if pinned.is_empty() {
        return instructions.to_string();
    }
    let mut block = String::from("Pinned context (preserve across this turn):");
    for pin in pinned {
        block.push_str(&format!(
            "\n- {} {}: {}",
            pin.id,
            pin.label,
            compact_text(&pin.summary, COMPACTION_PIN_SUMMARY_MAX_CHARS),
        ));
    }
    format!("{instructions}\n\n{block}")
}

fn instructions_with_response_verbosity(
    instructions: &str,
    verbosity: ResponseVerbosity,
    native_text_verbosity: bool,
) -> String {
    // Cost-first: skip the prompt-side hint when the model already
    // accepts the `text.verbosity` API parameter (one signal is enough)
    // and when the value is the implicit default (Normal). This keeps
    // the system prompt lean on the common path.
    if native_text_verbosity || verbosity == ResponseVerbosity::Normal {
        return instructions.to_string();
    }
    let guidance = match verbosity {
        ResponseVerbosity::Concise => {
            "Response verbosity: concise. Prefer short, direct answers unless the task requires detail."
        }
        ResponseVerbosity::Verbose => {
            "Response verbosity: verbose. Include fuller rationale, context, and verification details when useful."
        }
        ResponseVerbosity::Normal => unreachable!("handled above"),
    };
    format!("{instructions}\n\n{guidance}")
}

impl TurnRuntime {
    async fn run(mut self, input: String) -> squeezy_core::Result<()> {
        let task_title = input.clone();
        let activation = self.tools.activate_skills_for_input(&input)?;
        let base_instructions = match self.tools.format_active_skills(&activation.skills) {
            Some(skills) => format!("{}\n\n{}", self.config.instructions, skills),
            None => self.config.instructions.clone(),
        };
        let native_text_verbosity = capabilities_for(self.provider.name(), &self.config.model)
            .is_some_and(|capabilities| capabilities.text_verbosity);
        let verbosity_instructions = instructions_with_response_verbosity(
            &base_instructions,
            self.config.tui.response_verbosity,
            native_text_verbosity,
        );
        let mut prior_state = self.conversation_state.lock().await.clone();
        // Pinned context must reach the model on every turn, not only
        // after a compaction has occurred. Inline it into the per-turn
        // instructions so a `/pin` is immediately visible to the model
        // even on sessions that never cross the compaction threshold.
        let raw_instructions = instructions_with_pinned_context(
            &verbosity_instructions,
            &prior_state.context_compaction.pinned,
        );
        let active_attachments = prior_state
            .context_attachments
            .iter()
            .filter(|attachment| attachment.is_active())
            .cloned()
            .collect::<Vec<_>>();
        let user_transcript =
            TranscriptItem::user(format_user_text_with_context(&input, &active_attachments));
        let user_item = LlmInputItem::UserText(format_user_text_with_context(
            &activation.task_input,
            &active_attachments,
        ));
        let mut conversation = prior_state.conversation.clone();
        conversation.push(user_item.clone());
        let mut context_compaction = prior_state.context_compaction.clone();
        if let Some(report) = maybe_compact_conversation(
            &mut conversation,
            &mut context_compaction,
            &active_attachments,
            self.store.as_deref(),
            &self.config,
            ContextCompactionTrigger::Auto,
        ) {
            self.log_event(
                "context_compacted",
                Some(self.turn_id),
                Some(format!(
                    "compacted context gen={} {}->{} estimated tokens",
                    report.record.generation,
                    report.record.before.estimated_tokens,
                    report.record.after.estimated_tokens
                )),
                json!({
                    "record": report.record,
                    "summary": report.summary,
                }),
            );
            let _ = self
                .tx
                .send(AgentEvent::ContextCompacted {
                    turn_id: self.turn_id,
                    report,
                })
                .await;
        }
        // Response-id reuse is gated on the compaction generation being
        // unchanged for this turn. Invariant: `maybe_compact_conversation`
        // is the sole bumper of `context_compaction.generation` between
        // a turn's `prior_state` snapshot and this point — if some future
        // caller starts bumping it elsewhere (e.g. on resume), the
        // previous_response_id must be invalidated the same way to keep
        // the provider state consistent.
        let mut previous_response_id = if self.config.store_responses {
            if context_compaction.generation == prior_state.context_compaction.generation {
                prior_state.previous_response_id.take()
            } else {
                None
            }
        } else {
            None
        };
        let mut next_input = if previous_response_id.is_some() && self.config.store_responses {
            vec![user_item.clone()]
        } else {
            conversation.clone()
        };
        let mut total_cost = CostSnapshot::default();
        let mut seen_tool_outputs = SeenToolOutputs::from_store(self.store.clone());
        let mut broker = CostBroker::new(&self.config);
        let exploration_plan = self
            .config
            .exploration_compiler
            .then(|| compile_exploration_plan(&input))
            .flatten();
        let exploration_state = Arc::new(Mutex::new(ExplorationTurnState::from_plan(
            exploration_plan.as_ref(),
        )));
        broker.metrics.redactions += std::mem::take(&mut self.seed_redactions);
        // Instructions are static across the turn's tool rounds; redact
        // them once so the cost is not paid (or double-counted) per round.
        let redacted_instructions = self.redactor.redact(&raw_instructions);
        broker.metrics.redactions += redacted_instructions.redactions;
        let mut request_instructions = redacted_instructions.text;
        let mut active_skill_names = activation
            .skills
            .iter()
            .map(|skill| skill.summary.name.clone())
            .collect::<BTreeSet<_>>();
        // Holding a single stream redactor across rounds keeps the tail
        // buffer alive so a secret straddling a tool-call boundary is
        // still redacted before being released downstream.
        let mut assistant_stream = StreamRedactor::new(self.redactor.clone());
        // The Completed event's message is the concatenation of every
        // AssistantDelta we have already emitted plus the final flushed
        // tail. Building it as we go (rather than re-redacting the raw
        // text at the end) keeps ordinals stable between what streamed
        // into the TUI and what lands in the transcript.
        let mut assistant_message = String::new();
        self.log_event(
            "user_message",
            Some(self.turn_id),
            user_item_summary(&user_item),
            json!({}),
        );
        self.record_replay(
            SessionReplayEventKind::UserMessage,
            json!({ "input": input }),
        );
        self.publish_task_state(TaskStateSnapshot::starting(task_title.clone()))
            .await;
        if self.cancel.is_cancelled() {
            self.finish_cancelled_turn(&task_title).await;
            return Ok(());
        }

        if let Some(plan) = exploration_plan.clone()
            && !plan.calls.is_empty()
        {
            broker.metrics.planner_turns += 1;
            broker.metrics.planner_tool_calls += plan.calls.len() as u64;
            self.log_event(
                "exploration_plan",
                Some(self.turn_id),
                Some(format!("{} planner preflight", plan.intent.as_str())),
                json!({
                    "intent": plan.intent.as_str(),
                    "query": plan.query,
                    "calls": plan
                        .calls
                        .iter()
                        .map(|call| call.name.clone())
                        .collect::<Vec<_>>(),
                }),
            );
            let planned_calls = plan.calls;
            let mut planner_items = planned_calls
                .iter()
                .cloned()
                .map(|call| llm_function_call_item(call, &self.redactor))
                .collect::<Vec<_>>();
            let results = if let Some(replay) = &self.replay {
                replay_tool_calls(
                    replay,
                    planned_calls.clone(),
                    self.turn_id,
                    self.tx.clone(),
                    &mut broker,
                )
                .await?
            } else {
                execute_tool_calls(
                    planned_calls.clone(),
                    ToolExecutionContext {
                        turn_id: self.turn_id,
                        provider: self.provider.clone(),
                        tools: &self.tools,
                        jobs: &self.jobs,
                        config: &self.config,
                        telemetry: self.telemetry.clone(),
                        redactor: self.redactor.clone(),
                        tx: self.tx.clone(),
                        cancel: self.cancel.clone(),
                        approval_ids: self.approval_ids.clone(),
                        session_rules: self.session_rules.clone(),
                        session_mode: self.session_mode.clone(),
                        session_log: self.session_log.clone(),
                        task_state: self.task_state.clone(),
                        all_tool_specs: &self.all_tool_specs,
                        loaded_tool_schemas: self.loaded_tool_schemas.clone(),
                        exploration_state: exploration_state.clone(),
                    },
                    &mut broker,
                )
                .await
            };
            if self.cancel.is_cancelled() || results.iter().any(cancelled_tool_result) {
                self.finish_cancelled_turn(&task_title).await;
                return Ok(());
            }
            if self.append_implicit_skill_instructions(
                &results,
                &mut active_skill_names,
                &mut request_instructions,
                &mut broker.metrics,
            ) {
                previous_response_id = None;
            }
            // The planner is advisory: once the preflight block has executed,
            // the model has the planner outputs (success or not) in context, so
            // we lift the raw-read guard to avoid locking the turn on misfires
            // or non-`Success` graph results.
            exploration_state.lock().await.mark_preflight_complete();
            let results = seen_tool_outputs.prepare_results(results);
            let results = pack_tool_results(results, self.config.max_tool_result_bytes_per_round);
            self.record_replay_tool_results(&planned_calls, &results);
            for pending in &results {
                broker.record_model_result(&pending.result);
            }
            seen_tool_outputs.remember_results(&results);

            let outputs = results
                .into_iter()
                .map(|pending| {
                    let output = pending.result.model_output();
                    LlmInputItem::FunctionCallOutput {
                        call_id: pending.result.call_id,
                        output,
                    }
                })
                .collect::<Vec<_>>();
            planner_items.extend(outputs.clone());
            conversation.extend(planner_items.clone());
            for output in &outputs {
                self.log_event(
                    "tool_result",
                    Some(self.turn_id),
                    tool_output_summary(output),
                    json!({ "output": resume_item_for_json(output.clone()), "source": "exploration_compiler" }),
                );
            }
            if self.config.store_responses {
                next_input = vec![user_item.clone()];
                next_input.extend(planner_items);
            } else {
                next_input = conversation.clone();
            }
        }

        let mut last_tool_round_summary = None;
        let mut loop_guard = ToolLoopGuard::default();
        for _round in 0..MAX_TOOL_ROUNDS {
            if self.cancel.is_cancelled() {
                self.finish_cancelled_turn(&task_title).await;
                return Ok(());
            }
            let active_mode = load_session_mode(&self.session_mode);
            let loaded_tool_schemas = self.loaded_tool_schemas.lock().await.clone();
            let request = LlmRequest {
                model: self.config.model.clone(),
                instructions: instructions_with_tool_index(
                    &request_instructions,
                    &self.all_tool_specs,
                    active_mode,
                    &self.config.tools,
                ),
                input: redact_llm_input_items(&next_input, &self.redactor),
                max_output_tokens: self.config.max_output_tokens,
                response_verbosity: request_response_verbosity(&self.config, self.provider.name()),
                reasoning_effort: request_reasoning_effort(&self.config, self.provider.name()),
                previous_response_id: previous_response_id.clone(),
                tools: request_tool_specs(
                    &self.all_tool_specs,
                    active_mode,
                    &self.config.tools,
                    &loaded_tool_schemas,
                ),
                store: self.config.store_responses,
            };
            let request_model = request.model.clone();
            self.record_replay_request(&request);
            let mut stream = self
                .provider
                .stream_response(request.clone(), self.cancel.clone());
            let mut tool_calls = Vec::new();
            let mut completed = false;
            let mut response_id = None;
            let mut completed_cost = CostSnapshot::default();

            while let Some(event) =
                next_llm_stream_event(&mut stream, &self.cancel, self.config.stream_idle_timeout)
                    .await?
            {
                if self.cancel.is_cancelled() {
                    self.finish_cancelled_turn(&task_title).await;
                    return Ok(());
                }
                match event {
                    LlmEvent::Started => {
                        self.record_replay_model_started();
                        if self
                            .tx
                            .send(AgentEvent::Started {
                                turn_id: self.turn_id,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    LlmEvent::TextDelta(delta) => {
                        let chunk = assistant_stream.push(&delta);
                        if chunk.text.is_empty() {
                            continue;
                        }
                        self.record_replay_model_text_delta(&chunk.text);
                        assistant_message.push_str(&chunk.text);
                        if self
                            .tx
                            .send(AgentEvent::AssistantDelta {
                                turn_id: self.turn_id,
                                delta: chunk.text,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    LlmEvent::ToolCall(tool_call) => {
                        let call = ToolCall {
                            call_id: tool_call.call_id,
                            name: tool_call.name,
                            arguments: tool_call.arguments,
                        };
                        self.record_replay_model_tool_call(&call);
                        self.log_event(
                            "tool_call",
                            Some(self.turn_id),
                            Some(call.name.clone()),
                            json!({
                                "call_id": call.call_id,
                                "tool": call.name,
                                "arguments": call.arguments,
                            }),
                        );
                        if self
                            .tx
                            .send(AgentEvent::ToolCallQueued {
                                turn_id: self.turn_id,
                                call: redact_tool_call(call.clone(), &self.redactor),
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        tool_calls.push(call);
                    }
                    LlmEvent::Completed {
                        response_id: id,
                        mut cost,
                    } => {
                        if cost.estimated_usd_micros.is_none() {
                            cost.estimated_usd_micros =
                                estimate_cost(self.provider.name(), &request_model, &cost);
                        }
                        broker.metrics.record_provider(&cost);
                        merge_cost(&mut total_cost, &cost);
                        completed_cost = cost;
                        response_id = id;
                        completed = true;
                        break;
                    }
                    LlmEvent::Cancelled => {
                        self.finish_cancelled_turn(&task_title).await;
                        return Ok(());
                    }
                }
            }

            if !completed {
                if let Some(tail) = self
                    .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                    .await
                {
                    self.record_replay_model_text_delta(&tail);
                }
                broker.metrics.redactions += assistant_stream.total_redactions();
                let message = TranscriptItem::assistant(std::mem::take(&mut assistant_message));
                conversation.push(LlmInputItem::AssistantText(message.content.clone()));
                self.publish_terminal_task_state(TaskStateStatus::Completed, None, &task_title)
                    .await;
                self.persist_turn_state(TurnPersistInput {
                    conversation: &conversation,
                    response_id: previous_response_id.clone(),
                    user: user_transcript.clone(),
                    assistant: message.clone(),
                    cost: &total_cost,
                    metrics: &broker.metrics,
                    context_compaction: context_compaction.clone(),
                })
                .await;
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message,
                        response_id: None,
                        cost: total_cost,
                        metrics: broker.metrics.clone(),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }

            if tool_calls.is_empty() {
                if let Some(tail) = self
                    .flush_assistant_stream(&mut assistant_stream, &mut assistant_message)
                    .await
                {
                    self.record_replay_model_text_delta(&tail);
                }
                self.record_replay_model_completed(response_id.clone(), &completed_cost);
                broker.metrics.redactions += assistant_stream.total_redactions();
                let message = TranscriptItem::assistant(std::mem::take(&mut assistant_message));
                conversation.push(LlmInputItem::AssistantText(message.content.clone()));
                self.publish_terminal_task_state(TaskStateStatus::Completed, None, &task_title)
                    .await;
                self.persist_turn_state(TurnPersistInput {
                    conversation: &conversation,
                    response_id: response_id.clone(),
                    user: user_transcript.clone(),
                    assistant: message.clone(),
                    cost: &total_cost,
                    metrics: &broker.metrics,
                    context_compaction: context_compaction.clone(),
                })
                .await;
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message,
                        response_id,
                        cost: total_cost,
                        metrics: broker.metrics.clone(),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }

            self.record_replay_model_completed(response_id.clone(), &completed_cost);

            let results = if let Some(replay) = &self.replay {
                replay_tool_calls(
                    replay,
                    tool_calls.clone(),
                    self.turn_id,
                    self.tx.clone(),
                    &mut broker,
                )
                .await?
            } else {
                execute_tool_calls(
                    tool_calls.clone(),
                    ToolExecutionContext {
                        turn_id: self.turn_id,
                        provider: self.provider.clone(),
                        tools: &self.tools,
                        jobs: &self.jobs,
                        config: &self.config,
                        telemetry: self.telemetry.clone(),
                        redactor: self.redactor.clone(),
                        tx: self.tx.clone(),
                        cancel: self.cancel.clone(),
                        approval_ids: self.approval_ids.clone(),
                        session_rules: self.session_rules.clone(),
                        session_mode: self.session_mode.clone(),
                        session_log: self.session_log.clone(),
                        task_state: self.task_state.clone(),
                        all_tool_specs: &self.all_tool_specs,
                        loaded_tool_schemas: self.loaded_tool_schemas.clone(),
                        exploration_state: exploration_state.clone(),
                    },
                    &mut broker,
                )
                .await
            };
            if self.cancel.is_cancelled() || results.iter().any(cancelled_tool_result) {
                self.finish_cancelled_turn(&task_title).await;
                return Ok(());
            }
            last_tool_round_summary = tool_round_failure_summary(&results);
            if let Some(reason) = loop_guard.observe_round(&tool_calls, &results) {
                return Err(SqueezyError::Agent(reason));
            }
            let implicit_instructions_added = self.append_implicit_skill_instructions(
                &results,
                &mut active_skill_names,
                &mut request_instructions,
                &mut broker.metrics,
            );
            let results = seen_tool_outputs.prepare_results(results);
            let results = pack_tool_results(results, self.config.max_tool_result_bytes_per_round);
            self.record_replay_tool_results(&tool_calls, &results);
            for pending in &results {
                broker.record_model_result(&pending.result);
            }
            seen_tool_outputs.remember_results(&results);

            let outputs = results
                .into_iter()
                .map(|pending| {
                    let output = pending.result.model_output();
                    LlmInputItem::FunctionCallOutput {
                        call_id: pending.result.call_id,
                        output,
                    }
                })
                .collect::<Vec<_>>();
            conversation.extend(
                tool_calls
                    .iter()
                    .cloned()
                    .map(|call| llm_function_call_item(call, &self.redactor)),
            );
            conversation.extend(outputs.clone());
            for output in &outputs {
                self.log_event(
                    "tool_result",
                    Some(self.turn_id),
                    tool_output_summary(output),
                    json!({ "output": resume_item_for_json(output.clone()) }),
                );
            }

            if self.config.store_responses {
                previous_response_id = if implicit_instructions_added {
                    None
                } else {
                    response_id
                };
                next_input = outputs;
            } else {
                previous_response_id = None;
                next_input = conversation.clone();
            }
        }

        let suffix = last_tool_round_summary
            .map(|summary| format!(" · {summary}"))
            .unwrap_or_default();
        Err(SqueezyError::Agent(format!(
            "stopped after {MAX_TOOL_ROUNDS} tool rounds{suffix}"
        )))
    }

    fn append_implicit_skill_instructions(
        &self,
        results: &[ToolResult],
        active_skill_names: &mut BTreeSet<String>,
        request_instructions: &mut String,
        metrics: &mut TurnMetrics,
    ) -> bool {
        let names = implicit_skill_names(results, active_skill_names);
        if names.is_empty() {
            return false;
        }

        let mut loaded = Vec::new();
        for name in names {
            match self.tools.load_skill_for_instructions(&name) {
                Ok(skill) => {
                    active_skill_names.insert(name);
                    loaded.push(skill);
                }
                Err(error) => {
                    self.log_event(
                        "skill_activation_failed",
                        Some(self.turn_id),
                        Some(format!("implicit skill activation failed: {name}")),
                        json!({
                            "name": name,
                            "source": "implicit",
                            "error": error.to_string(),
                        }),
                    );
                }
            }
        }
        let Some(block) = self.tools.format_active_skills(&loaded) else {
            return false;
        };
        let redacted = self.redactor.redact(&block);
        metrics.redactions += redacted.redactions;
        request_instructions.push_str("\n\n");
        request_instructions.push_str(&redacted.text);
        self.log_event(
            "skill_activation",
            Some(self.turn_id),
            Some(format!("{} implicit skill(s) activated", loaded.len())),
            json!({
                "source": "implicit",
                "skills": loaded
                    .iter()
                    .map(|skill| skill.summary.name.clone())
                    .collect::<Vec<_>>(),
            }),
        );
        true
    }

    async fn finish_turn(&self, metrics: &TurnMetrics) {
        self.telemetry.spawn(TelemetryEvent::turn_completed(
            &self.config,
            self.turn_id.get(),
            metrics.clone(),
        ));
        self.session_metrics.lock().await.merge_turn(metrics);
    }

    async fn persist_turn_state(&self, input: TurnPersistInput<'_>) {
        let TurnPersistInput {
            conversation,
            response_id,
            user,
            assistant,
            cost,
            metrics,
            context_compaction,
        } = input;
        let mut state = self.conversation_state.lock().await;
        state.conversation = conversation.to_vec();
        state.previous_response_id = if self.config.store_responses {
            response_id.clone()
        } else {
            None
        };
        state.transcript.push(user);
        state.transcript.push(assistant.clone());
        // Pins added concurrently to this turn (via /pin) are pushed into
        // `state.context_compaction.pinned` under the same lock. Merge them
        // into the locally tracked compaction state so the pre-turn clone
        // does not silently clobber a pin landed mid-turn.
        let mut merged_compaction = context_compaction;
        merge_concurrent_pins(&mut merged_compaction, &state.context_compaction.pinned);
        state.context_compaction = merged_compaction;
        merge_cost(&mut state.cost, cost);
        state.metrics.merge_turn(metrics);
        state.redactions += metrics.redactions;
        if let Some(session) = &self.session_log {
            let _ = session.write_resume_state(&state.to_resume_state());
            let _ = session.update_metadata(|metadata| {
                metadata.cost = state.cost.clone();
                metadata.metrics = state.metrics.clone();
                metadata.redactions = state.redactions;
                metadata.resume_available = true;
                metadata.mode = load_session_mode(&self.session_mode);
            });
        }
        drop(state);
        let summary = self.current_task_summary().await.unwrap_or_else(|| {
            if assistant.content.trim().is_empty() {
                "assistant completed".to_string()
            } else {
                assistant.content.clone()
            }
        });
        self.record_replay(
            SessionReplayEventKind::CostDecision,
            json!({
                "cost": cost,
                "metrics": metrics,
            }),
        );
        self.log_event(
            "assistant_completed",
            Some(self.turn_id),
            Some(summary),
            json!({
                "response_id": response_id,
                "cost": cost,
                "metrics": metrics,
            }),
        );
    }

    async fn publish_task_state(&self, snapshot: TaskStateSnapshot) {
        publish_task_state_update(
            &self.tx,
            self.session_log.as_ref(),
            &self.redactor,
            &self.task_state,
            self.turn_id,
            snapshot,
        )
        .await;
    }

    async fn publish_terminal_task_state(
        &self,
        status: TaskStateStatus,
        summary: Option<String>,
        fallback_task: &str,
    ) {
        let latest = self.task_state.lock().await.clone();
        self.publish_task_state(TaskStateSnapshot::terminal_from(
            latest.as_ref(),
            fallback_task.to_string(),
            status,
            summary,
        ))
        .await;
    }

    async fn finish_cancelled_turn(&self, task_title: &str) {
        self.publish_terminal_task_state(
            TaskStateStatus::Cancelled,
            Some("turn cancelled".to_string()),
            task_title,
        )
        .await;
        self.log_event(
            "cancelled",
            Some(self.turn_id),
            Some("turn cancelled".to_string()),
            json!({}),
        );
        self.record_replay(SessionReplayEventKind::ModelCancelled, json!({}));
        let _ = self
            .tx
            .send(AgentEvent::Cancelled {
                turn_id: self.turn_id,
            })
            .await;
    }

    async fn current_task_summary(&self) -> Option<String> {
        self.task_state
            .lock()
            .await
            .as_ref()
            .map(TaskStateSnapshot::compact_summary)
    }

    fn log_event(
        &self,
        kind: &str,
        turn_id: Option<TurnId>,
        summary: Option<String>,
        payload: Value,
    ) {
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            kind,
            turn_id,
            summary,
            payload,
        );
    }

    fn record_replay(&self, kind: SessionReplayEventKind, payload: Value) {
        if self.replay.is_some() {
            return;
        }
        if let Some(session) = &self.session_log {
            let payload = redact_json_payload(payload, &self.redactor);
            let _ = session.append_replay_event(SessionReplayEvent::new(
                kind,
                Some(self.turn_id.to_string()),
                payload,
            ));
        }
    }

    fn record_replay_request(&self, request: &LlmRequest) {
        self.record_replay(
            SessionReplayEventKind::ModelRequest,
            json!({
                "hash": replay_hash(request),
                "request": request,
            }),
        );
    }

    fn record_replay_model_started(&self) {
        self.record_replay(SessionReplayEventKind::ModelStarted, json!({}));
    }

    fn record_replay_model_text_delta(&self, text: &str) {
        self.record_replay(
            SessionReplayEventKind::ModelTextDelta,
            json!({ "text": text }),
        );
    }

    fn record_replay_model_tool_call(&self, call: &ToolCall) {
        let call = redact_tool_call(call.clone(), &self.redactor);
        self.record_replay(
            SessionReplayEventKind::ModelToolCall,
            json!({
                "call": {
                    "call_id": call.call_id,
                    "name": call.name,
                    "arguments": call.arguments,
                },
            }),
        );
    }

    fn record_replay_model_completed(&self, response_id: Option<String>, cost: &CostSnapshot) {
        self.record_replay(
            SessionReplayEventKind::ModelCompleted,
            json!({
                "response_id": response_id,
                "cost": cost,
            }),
        );
    }

    fn record_replay_tool_results(&self, calls: &[ToolCall], results: &[PendingToolResult]) {
        for (call, pending) in calls.iter().zip(results.iter()) {
            let redacted_call = redact_tool_call(call.clone(), &self.redactor);
            self.record_replay(
                SessionReplayEventKind::ToolCall,
                json!({
                    "hash": replay_hash(&redacted_call),
                    "call": redacted_call,
                }),
            );
            self.record_replay(
                SessionReplayEventKind::ToolResult,
                json!({
                    "result": &pending.result,
                    "model_output": pending.result.model_output(),
                }),
            );
        }
    }

    /// Flushes any text the stream redactor is still holding behind its
    /// tail buffer, emitting it as a final AssistantDelta and appending
    /// it to the running message accumulator. Idempotent on an already
    /// flushed stream.
    async fn flush_assistant_stream(
        &self,
        assistant_stream: &mut StreamRedactor,
        assistant_message: &mut String,
    ) -> Option<String> {
        let tail = assistant_stream.finish();
        if tail.text.is_empty() {
            return None;
        }
        let text = tail.text;
        assistant_message.push_str(&text);
        let _ = self
            .tx
            .send(AgentEvent::AssistantDelta {
                turn_id: self.turn_id,
                delta: text.clone(),
            })
            .await;
        Some(text)
    }
}

struct TurnPersistInput<'a> {
    conversation: &'a [LlmInputItem],
    response_id: Option<String>,
    user: TranscriptItem,
    assistant: TranscriptItem,
    cost: &'a CostSnapshot,
    metrics: &'a TurnMetrics,
    context_compaction: ContextCompactionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentKind {
    Delegate,
    Explore,
}

impl SubagentKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delegate => "delegate",
            Self::Explore => "explore",
        }
    }
}

/// Re-applies any pins added concurrently while the turn was running.
///
/// The turn runner builds its own `ContextCompactionState` clone at turn
/// start. If `/pin` or `/unpin` lands while the turn is in flight, the
/// authoritative state is on the shared `conversation_state` mutex; the
/// pre-turn clone we are about to persist would otherwise silently lose
/// those concurrent pin edits. This helper unions in pins that exist in
/// the latest snapshot but are missing from the pre-turn state.
fn merge_concurrent_pins(compaction: &mut ContextCompactionState, latest_pins: &[ContextPin]) {
    for pin in latest_pins {
        if !compaction
            .pinned
            .iter()
            .any(|existing| existing.id == pin.id)
        {
            compaction.pinned.push(pin.clone());
        }
    }
}

#[derive(Clone)]
struct ToolExecutionContext<'a> {
    turn_id: TurnId,
    provider: Arc<dyn LlmProvider>,
    tools: &'a ToolRegistry,
    jobs: &'a JobRegistry,
    config: &'a AppConfig,
    telemetry: TelemetryClient,
    redactor: Arc<Redactor>,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    session_mode: Arc<AtomicU8>,
    session_log: Option<SessionHandle>,
    task_state: Arc<Mutex<Option<TaskStateSnapshot>>>,
    all_tool_specs: &'a [AdvertisedTool],
    loaded_tool_schemas: Arc<Mutex<Vec<String>>>,
    exploration_state: Arc<Mutex<ExplorationTurnState>>,
}

struct McpElicitationHandlerScope<'a> {
    tools: &'a ToolRegistry,
}

impl Drop for McpElicitationHandlerScope<'_> {
    fn drop(&mut self) {
        self.tools.set_mcp_elicitation_handler(None);
    }
}

fn install_mcp_elicitation_handler<'a>(
    context: &'a ToolExecutionContext<'_>,
) -> McpElicitationHandlerScope<'a> {
    let turn_id = context.turn_id;
    let tx = context.tx.clone();
    let cancel = context.cancel.clone();
    let handler: McpElicitationHandler = Arc::new(move |request| {
        let tx = tx.clone();
        let cancel = cancel.clone();
        Box::pin(async move {
            let (response_tx, response_rx) = oneshot::channel();
            let send_request = tx.send(AgentEvent::McpElicitationRequested {
                turn_id,
                request,
                response_tx,
            });
            let send_result = tokio::select! {
                _ = cancel.cancelled() => return McpElicitationResponse::cancel(),
                result = send_request => result,
            };
            if send_result.is_err() {
                return McpElicitationResponse::decline();
            }
            tokio::select! {
                _ = cancel.cancelled() => McpElicitationResponse::cancel(),
                response = response_rx => response.unwrap_or_else(|_| McpElicitationResponse::decline()),
            }
        })
    });
    context.tools.set_mcp_elicitation_handler(Some(handler));
    McpElicitationHandlerScope {
        tools: context.tools,
    }
}

async fn handle_task_state_call(context: &ToolExecutionContext<'_>, call: &ToolCall) -> ToolResult {
    let snapshot = match serde_json::from_value::<TaskStateSnapshot>(call.arguments.clone()) {
        Ok(snapshot) => snapshot.normalized(),
        Err(error) => {
            return control_tool_result(
                call,
                ToolStatus::Error,
                json!({ "ok": false, "error": format!("invalid task state: {error}") }),
            );
        }
    };
    publish_task_state_update(
        &context.tx,
        context.session_log.as_ref(),
        &context.redactor,
        &context.task_state,
        context.turn_id,
        snapshot.clone(),
    )
    .await;
    control_tool_result(
        call,
        ToolStatus::Success,
        json!({ "ok": true, "summary": snapshot.compact_summary() }),
    )
}

async fn handle_load_tool_schema_call(
    context: &ToolExecutionContext<'_>,
    call: &ToolCall,
) -> ToolResult {
    let Some(name) = call
        .arguments
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return control_tool_result(
            call,
            ToolStatus::Error,
            json!({ "ok": false, "error": "missing required string field: name" }),
        );
    };

    let Some(tool) = context
        .all_tool_specs
        .iter()
        .find(|tool| tool.spec.name == name)
    else {
        return control_tool_result(
            call,
            ToolStatus::Error,
            json!({ "ok": false, "name": name, "error": "unknown tool" }),
        );
    };

    let active_mode = load_session_mode(&context.session_mode);
    if mode_refuses_capability(active_mode, tool.capability) {
        return control_tool_result(
            call,
            ToolStatus::Denied,
            json!({
                "ok": false,
                "name": name,
                "status": "refused",
                "capability": tool.capability.as_str(),
                "mode": active_mode.as_str(),
                "error": "tool schema is not allowed in the current session mode"
            }),
        );
    }

    if tool_is_core_schema(tool, &context.config.tools) {
        return control_tool_result(
            call,
            ToolStatus::Success,
            json!({
                "ok": true,
                "name": name,
                "status": "already_attached",
                "position": "core"
            }),
        );
    }

    let mut loaded = context.loaded_tool_schemas.lock().await;
    if let Some(position) = loaded.iter().position(|loaded_name| loaded_name == name) {
        return control_tool_result(
            call,
            ToolStatus::Success,
            json!({
                "ok": true,
                "name": name,
                "status": "already_attached",
                "position": position
            }),
        );
    }
    loaded.push(name.to_string());
    let position = loaded.len() - 1;
    control_tool_result(
        call,
        ToolStatus::Success,
        json!({
            "ok": true,
            "name": name,
            "status": "attached",
            "position": position
        }),
    )
}

#[derive(Debug, Clone)]
struct SubagentRequest {
    prompt: String,
    scope: Option<String>,
    thoroughness: Option<String>,
}

#[derive(Debug, Clone)]
struct SubagentExecution {
    status: ToolStatus,
    summary: String,
    status_label: &'static str,
    error: Option<String>,
    metrics: TurnMetrics,
    supporting_receipts: Vec<Value>,
    model: String,
}

async fn handle_subagent_call(
    context: &ToolExecutionContext<'_>,
    call: &ToolCall,
    kind: SubagentKind,
    broker: &mut CostBroker,
) -> ToolResult {
    broker.metrics.subagent_calls += 1;
    if !context.config.subagents.enabled
        || (kind == SubagentKind::Explore && !context.config.subagents.explore_enabled)
    {
        broker.metrics.subagent_failures += 1;
        return subagent_control_result(
            call,
            kind,
            SubagentExecution {
                status: ToolStatus::Denied,
                summary: String::new(),
                status_label: "disabled",
                error: Some("subagent is disabled by configuration".to_string()),
                metrics: TurnMetrics::default(),
                supporting_receipts: Vec::new(),
                model: subagent_model_for_kind(context.provider.name(), context.config, kind),
            },
        );
    }
    let request = match parse_subagent_request(call, kind) {
        Ok(request) => request,
        Err(error) => {
            broker.metrics.subagent_failures += 1;
            return subagent_control_result(
                call,
                kind,
                SubagentExecution {
                    status: ToolStatus::Error,
                    summary: String::new(),
                    status_label: "invalid_request",
                    error: Some(error),
                    metrics: TurnMetrics::default(),
                    supporting_receipts: Vec::new(),
                    model: subagent_model_for_kind(context.provider.name(), context.config, kind),
                },
            );
        }
    };
    let started_prompt = context
        .redactor
        .redact(&compact_text(&request.prompt, 240))
        .text;
    log_session_event(
        context.session_log.as_ref(),
        &context.redactor,
        "subagent_started",
        Some(context.turn_id),
        Some(format!("{}: {started_prompt}", kind.as_str())),
        json!({
            "agent": kind.as_str(),
            "scope": request.scope,
            "thoroughness": request.thoroughness,
        }),
    );
    let _ = context
        .tx
        .send(AgentEvent::SubagentStarted {
            turn_id: context.turn_id,
            agent: kind.as_str().to_string(),
            prompt: started_prompt,
        })
        .await;

    let execution = run_subagent(context, kind, request).await;
    broker
        .metrics
        .merge_subagent_tool_metrics(&execution.metrics);
    if execution.status != ToolStatus::Success {
        broker.metrics.subagent_failures += 1;
    }
    let event_payload = json!({
        "agent": kind.as_str(),
        "status": execution.status_label,
        "model": execution.model,
        "metrics": execution.metrics.clone(),
        "supporting_receipts": execution.supporting_receipts.clone(),
    });
    match execution.status {
        ToolStatus::Success => {
            log_session_event(
                context.session_log.as_ref(),
                &context.redactor,
                "subagent_completed",
                Some(context.turn_id),
                Some(format!(
                    "{} completed: {}",
                    kind.as_str(),
                    compact_text(&execution.summary, 240)
                )),
                event_payload,
            );
            let _ = context
                .tx
                .send(AgentEvent::SubagentCompleted {
                    turn_id: context.turn_id,
                    agent: kind.as_str().to_string(),
                    summary: compact_text(&execution.summary, 320),
                    metrics: execution.metrics.clone(),
                })
                .await;
        }
        _ => {
            let error = execution
                .error
                .clone()
                .unwrap_or_else(|| execution.status_label.to_string());
            log_session_event(
                context.session_log.as_ref(),
                &context.redactor,
                "subagent_failed",
                Some(context.turn_id),
                Some(format!(
                    "{} failed: {}",
                    kind.as_str(),
                    compact_text(&error, 240)
                )),
                event_payload,
            );
            let _ = context
                .tx
                .send(AgentEvent::SubagentFailed {
                    turn_id: context.turn_id,
                    agent: kind.as_str().to_string(),
                    error: compact_text(&error, 320),
                    metrics: execution.metrics.clone(),
                })
                .await;
        }
    }

    subagent_control_result(call, kind, execution)
}

fn parse_subagent_request(call: &ToolCall, kind: SubagentKind) -> Result<SubagentRequest, String> {
    let prompt = call
        .arguments
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing required string field: prompt".to_string())?
        .to_string();
    let scope = match call.arguments.get("scope") {
        Some(Value::Null) | None => None,
        Some(Value::String(value)) if value.trim().is_empty() => None,
        Some(Value::String(value)) => Some(value.trim().to_string()),
        Some(_) => return Err("scope must be a string or null".to_string()),
    };
    let thoroughness = match call.arguments.get("thoroughness") {
        Some(Value::Null) | None => None,
        Some(Value::String(value)) => {
            let value = value.trim().to_ascii_lowercase();
            if value.is_empty() {
                None
            } else if matches!(value.as_str(), "quick" | "medium" | "thorough") {
                Some(value)
            } else {
                return Err("thoroughness must be quick, medium, or thorough".to_string());
            }
        }
        Some(_) => return Err("thoroughness must be a string".to_string()),
    };
    if kind == SubagentKind::Delegate && thoroughness.is_some() {
        return Err("delegate does not accept thoroughness".to_string());
    }
    Ok(SubagentRequest {
        prompt,
        scope,
        thoroughness,
    })
}

async fn run_subagent(
    parent: &ToolExecutionContext<'_>,
    kind: SubagentKind,
    request: SubagentRequest,
) -> SubagentExecution {
    let mut config = parent.config.clone();
    config.session_mode = SessionMode::Plan;
    config.store_responses = false;
    config.max_output_tokens = Some(config.subagents.max_summary_tokens);
    config.max_tool_calls_per_turn = config.subagents.max_tool_calls_per_call;
    config.max_tool_bytes_read_per_turn = config.subagents.max_tool_bytes_read_per_call;
    config.max_search_files_per_turn = config.subagents.max_search_files_per_call;
    config.max_tool_result_bytes_per_round = config.max_tool_result_bytes_per_round.min(24_000);
    let model = subagent_model_for_kind(parent.provider.name(), &config, kind);
    config.model = model.clone();

    let allowed_tools = subagent_allowed_tools(parent.all_tool_specs, kind);
    if allowed_tools.is_empty() {
        return SubagentExecution {
            status: ToolStatus::Error,
            summary: String::new(),
            status_label: "failed",
            error: Some("no read-only tools are available to the subagent".to_string()),
            metrics: TurnMetrics::default(),
            supporting_receipts: Vec::new(),
            model,
        };
    }
    let allowed_tool_names = allowed_tools
        .iter()
        .map(|tool| tool.spec.name.clone())
        .collect::<BTreeSet<_>>();
    let tool_specs = advertised_tool_specs(&allowed_tools, SessionMode::Plan);
    let instructions = subagent_instructions(kind, &request);
    let redacted_instructions = parent.redactor.redact(&instructions);
    let mut broker = CostBroker::new(&config);
    broker.metrics.redactions += redacted_instructions.redactions;
    let mut assistant_stream = StreamRedactor::new(parent.redactor.clone());
    let mut assistant_message = String::new();
    let mut conversation = vec![LlmInputItem::UserText(subagent_user_prompt(&request))];
    let mut supporting_receipts = Vec::new();
    // Subagent tool execution emits ToolCallStarted/ToolCallCompleted/JobUpdated
    // events on the per-call `tx` channel. The parent never surfaces these
    // intermediate events, so we drain them in a background task. Without an
    // active drain a high-fanout round (>~4 parallel tool calls) would fill
    // the channel buffer and the `send().await` inside the tool dispatcher
    // would block forever.
    let (hidden_tx, mut hidden_rx) = mpsc::channel::<AgentEvent>(64);
    let drain_handle = tokio::spawn(async move { while hidden_rx.recv().await.is_some() {} });
    let local_jobs = JobRegistry::new();
    let local_task_state = Arc::new(Mutex::new(None));
    let local_loaded_schemas = Arc::new(Mutex::new(Vec::new()));
    let local_mode = Arc::new(AtomicU8::new(SessionMode::Plan.to_u8()));
    let local_exploration = Arc::new(Mutex::new(ExplorationTurnState::from_plan(None)));
    let mut seen_outputs = SeenToolOutputs::default();

    let execution = run_subagent_loop(
        parent,
        &config,
        &tool_specs,
        &allowed_tools,
        &allowed_tool_names,
        &redacted_instructions.text,
        &hidden_tx,
        &local_jobs,
        &local_task_state,
        &local_loaded_schemas,
        &local_mode,
        &local_exploration,
        &mut seen_outputs,
        &mut broker,
        &mut assistant_stream,
        &mut assistant_message,
        &mut conversation,
        &mut supporting_receipts,
        model,
    )
    .await;

    drop(hidden_tx);
    let _ = drain_handle.await;
    execution
}

#[allow(clippy::too_many_arguments)]
async fn run_subagent_loop(
    parent: &ToolExecutionContext<'_>,
    config: &AppConfig,
    tool_specs: &[LlmToolSpec],
    allowed_tools: &[AdvertisedTool],
    allowed_tool_names: &BTreeSet<String>,
    instructions: &str,
    hidden_tx: &mpsc::Sender<AgentEvent>,
    local_jobs: &JobRegistry,
    local_task_state: &Arc<Mutex<Option<TaskStateSnapshot>>>,
    local_loaded_schemas: &Arc<Mutex<Vec<String>>>,
    local_mode: &Arc<AtomicU8>,
    local_exploration: &Arc<Mutex<ExplorationTurnState>>,
    seen_outputs: &mut SeenToolOutputs,
    broker: &mut CostBroker,
    assistant_stream: &mut StreamRedactor,
    assistant_message: &mut String,
    conversation: &mut Vec<LlmInputItem>,
    supporting_receipts: &mut Vec<Value>,
    model: String,
) -> SubagentExecution {
    for _round in 0..config.subagents.max_model_rounds {
        let llm_input = redact_llm_input_items(conversation, &parent.redactor);
        let request_model = config.model.clone();
        let llm_request = LlmRequest {
            model: request_model.clone(),
            instructions: instructions.to_string(),
            input: llm_input,
            max_output_tokens: config.max_output_tokens,
            response_verbosity: request_response_verbosity(config, parent.provider.name()),
            reasoning_effort: request_reasoning_effort(config, parent.provider.name()),
            previous_response_id: None,
            tools: tool_specs.to_vec(),
            store: false,
        };
        let mut stream = parent
            .provider
            .stream_response(llm_request, parent.cancel.child_token());
        let mut tool_calls = Vec::new();
        let mut completed = false;
        loop {
            let event = match next_llm_stream_event(
                &mut stream,
                &parent.cancel,
                config.stream_idle_timeout,
            )
            .await
            {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    broker.metrics.redactions += assistant_stream.total_redactions();
                    return SubagentExecution {
                        status: ToolStatus::Error,
                        summary: String::new(),
                        status_label: "failed",
                        error: Some(error.to_string()),
                        metrics: broker.metrics.clone(),
                        supporting_receipts: std::mem::take(supporting_receipts),
                        model,
                    };
                }
            };
            match event {
                LlmEvent::Started => {}
                LlmEvent::TextDelta(delta) => {
                    let chunk = assistant_stream.push(&delta);
                    if !chunk.text.is_empty() {
                        assistant_message.push_str(&chunk.text);
                    }
                }
                LlmEvent::ToolCall(tool_call) => {
                    tool_calls.push(ToolCall {
                        call_id: tool_call.call_id,
                        name: tool_call.name,
                        arguments: tool_call.arguments,
                    });
                }
                LlmEvent::Completed { mut cost, .. } => {
                    if cost.estimated_usd_micros.is_none() {
                        cost.estimated_usd_micros =
                            estimate_cost(parent.provider.name(), &request_model, &cost);
                    }
                    broker.metrics.record_provider(&cost);
                    completed = true;
                    break;
                }
                LlmEvent::Cancelled => {
                    broker.metrics.redactions += assistant_stream.total_redactions();
                    return SubagentExecution {
                        status: ToolStatus::Cancelled,
                        summary: String::new(),
                        status_label: "cancelled",
                        error: Some("subagent cancelled".to_string()),
                        metrics: broker.metrics.clone(),
                        supporting_receipts: std::mem::take(supporting_receipts),
                        model,
                    };
                }
            }
        }

        if !completed {
            let chunk = assistant_stream.finish();
            if !chunk.text.is_empty() {
                assistant_message.push_str(&chunk.text);
            }
            broker.metrics.redactions += assistant_stream.total_redactions();
            return successful_subagent_execution(
                std::mem::take(assistant_message),
                broker.metrics.clone(),
                std::mem::take(supporting_receipts),
                model,
                config,
            );
        }

        if tool_calls.is_empty() {
            let chunk = assistant_stream.finish();
            if !chunk.text.is_empty() {
                assistant_message.push_str(&chunk.text);
            }
            broker.metrics.redactions += assistant_stream.total_redactions();
            return successful_subagent_execution(
                std::mem::take(assistant_message),
                broker.metrics.clone(),
                std::mem::take(supporting_receipts),
                model,
                config,
            );
        }

        let rejected = tool_calls
            .iter()
            .filter(|call| !allowed_tool_names.contains(&call.name))
            .map(|call| ToolResult::denied(call, "tool is not available to this subagent"))
            .collect::<Vec<_>>();
        let approved = tool_calls
            .iter()
            .filter(|call| allowed_tool_names.contains(&call.name))
            .cloned()
            .collect::<Vec<_>>();
        let mut results = rejected;
        if !approved.is_empty() {
            results.extend(
                execute_tool_calls(
                    approved,
                    ToolExecutionContext {
                        turn_id: parent.turn_id,
                        provider: parent.provider.clone(),
                        tools: parent.tools,
                        jobs: local_jobs,
                        config,
                        telemetry: parent.telemetry.clone(),
                        redactor: parent.redactor.clone(),
                        tx: hidden_tx.clone(),
                        cancel: parent.cancel.child_token(),
                        approval_ids: parent.approval_ids.clone(),
                        session_rules: parent.session_rules.clone(),
                        session_mode: local_mode.clone(),
                        session_log: None,
                        task_state: local_task_state.clone(),
                        all_tool_specs: allowed_tools,
                        loaded_tool_schemas: local_loaded_schemas.clone(),
                        exploration_state: local_exploration.clone(),
                    },
                    broker,
                )
                .await,
            );
        }
        let results = seen_outputs.prepare_results(results);
        let results = pack_tool_results(results, config.max_tool_result_bytes_per_round);
        for pending in &results {
            broker.record_model_result(&pending.result);
            if supporting_receipts.len() < 12 {
                supporting_receipts.push(subagent_supporting_receipt(&pending.result));
            }
        }
        conversation.extend(
            tool_calls
                .iter()
                .cloned()
                .map(|call| llm_function_call_item(call, &parent.redactor)),
        );
        conversation.extend(results.into_iter().map(|pending| {
            let output = pending.result.model_output();
            LlmInputItem::FunctionCallOutput {
                call_id: pending.result.call_id,
                output,
            }
        }));
    }

    broker.metrics.redactions += assistant_stream.total_redactions();
    SubagentExecution {
        status: ToolStatus::Error,
        summary: String::new(),
        status_label: "max_rounds_exceeded",
        error: Some(format!(
            "subagent stopped after {} model rounds",
            config.subagents.max_model_rounds
        )),
        metrics: broker.metrics.clone(),
        supporting_receipts: std::mem::take(supporting_receipts),
        model,
    }
}

fn successful_subagent_execution(
    summary: String,
    metrics: TurnMetrics,
    supporting_receipts: Vec<Value>,
    model: String,
    config: &AppConfig,
) -> SubagentExecution {
    let max_chars = (config.subagents.max_summary_tokens as usize)
        .saturating_mul(SUBAGENT_SUMMARY_CHARS_PER_TOKEN)
        .max(256);
    SubagentExecution {
        status: ToolStatus::Success,
        summary: compact_text(&summary, max_chars),
        status_label: "completed",
        error: None,
        metrics,
        supporting_receipts,
        model,
    }
}

fn subagent_control_result(
    call: &ToolCall,
    kind: SubagentKind,
    execution: SubagentExecution,
) -> ToolResult {
    let mut content = json!({
        "ok": execution.status == ToolStatus::Success,
        "agent": kind.as_str(),
        "status": execution.status_label,
        "summary": execution.summary,
        "model": execution.model,
        "supporting_receipts": execution.supporting_receipts,
        "cost": execution.metrics.provider,
        "metrics": {
            "tool_calls": execution.metrics.tool_calls,
            "tool_successes": execution.metrics.tool_successes,
            "tool_errors": execution.metrics.tool_errors,
            "tool_denials": execution.metrics.tool_denials,
            "tool_cancellations": execution.metrics.tool_cancellations,
            "files_scanned": execution.metrics.files_scanned,
            "bytes_read": execution.metrics.bytes_read,
            "model_output_bytes": execution.metrics.model_output_bytes,
            "budget_denials": execution.metrics.budget_denials,
            "redactions": execution.metrics.redactions,
        }
    });
    if let Some(error) = execution.error {
        content["error"] = json!(error);
    }
    control_tool_result(call, execution.status, content)
}

fn subagent_supporting_receipt(result: &ToolResult) -> Value {
    json!({
        "tool": result.tool_name,
        "status": tool_status_label(result.status),
        "output_sha256": result.receipt.output_sha256,
        "content_sha256": result.receipt.content_sha256,
        "output_bytes": result.cost_hint.output_bytes,
        "truncated": result.cost_hint.truncated,
    })
}

fn subagent_user_prompt(request: &SubagentRequest) -> String {
    let mut prompt = format!("Task:\n{}", request.prompt);
    if let Some(scope) = &request.scope {
        prompt.push_str(&format!("\n\nScope:\n{scope}"));
    }
    if let Some(thoroughness) = &request.thoroughness {
        prompt.push_str(&format!("\n\nThoroughness: {thoroughness}"));
    }
    prompt
}

fn tool_status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Success => "success",
        ToolStatus::Error => "error",
        ToolStatus::Denied => "denied",
        ToolStatus::Stale => "stale",
        ToolStatus::Cancelled => "cancelled",
    }
}

fn subagent_instructions(kind: SubagentKind, request: &SubagentRequest) -> String {
    match kind {
        SubagentKind::Delegate => {
            "You are an isolated Squeezy research subagent. Investigate the requested question with read/search/navigation tools only. Return a concise summary for the parent agent with relevant files, symbols, risks, and next actions. Do not modify files, run commands, ask the user, or include raw tool dumps.".to_string()
        }
        SubagentKind::Explore => {
            let thoroughness = request.thoroughness.as_deref().unwrap_or("medium");
            format!(
                "You are Squeezy's cheap read-only code exploration subagent. Use semantic graph tools first: repo_map, decl_search, definition_search, reference_search, symbol_context, hierarchy, upstream_flow, downstream_flow, and read_slice. Use glob, grep, and read_file only as bounded fallback. Thoroughness: {thoroughness}. Return a compact briefing with relevant files/symbols, architecture notes, implementation hazards, and the minimum next reads/actions the parent needs before planning or editing. Do not modify files, run shell/compiler/network/MCP tools, ask the user, or include raw tool dumps."
            )
        }
    }
}

fn subagent_model_for_kind(provider: &str, config: &AppConfig, kind: SubagentKind) -> String {
    match kind {
        SubagentKind::Delegate => config.model.clone(),
        SubagentKind::Explore => config.subagents.explore_model.clone().unwrap_or_else(|| {
            default_cheap_model_for_provider(provider)
                .unwrap_or(&config.model)
                .to_string()
        }),
    }
}

fn default_cheap_model_for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some(DEFAULT_OPENAI_MODEL),
        "anthropic" => Some(DEFAULT_ANTHROPIC_MODEL),
        "google" => Some(DEFAULT_GOOGLE_MODEL),
        "azure_openai" => Some(DEFAULT_AZURE_OPENAI_MODEL),
        "bedrock" => Some(DEFAULT_BEDROCK_MODEL),
        "ollama" => Some(DEFAULT_OLLAMA_MODEL),
        _ => None,
    }
}

const DELEGATE_SUBAGENT_TOOL_NAMES: &[&str] = &[
    "glob",
    "grep",
    "read_file",
    "read_tool_output",
    "decl_search",
    "definition_search",
    "diff_context",
    "downstream_flow",
    "hierarchy",
    "list_skills",
    "load_skill",
    "plan_patch",
    "read_slice",
    "reference_search",
    "repo_map",
    "symbol_context",
    "upstream_flow",
];

const EXPLORE_SUBAGENT_TOOL_NAMES: &[&str] = &[
    "repo_map",
    "decl_search",
    "definition_search",
    "reference_search",
    "symbol_context",
    "hierarchy",
    "upstream_flow",
    "downstream_flow",
    "read_slice",
    "glob",
    "grep",
    "read_file",
];

fn subagent_allowed_tools(
    all_tool_specs: &[AdvertisedTool],
    kind: SubagentKind,
) -> Vec<AdvertisedTool> {
    let names = match kind {
        SubagentKind::Delegate => DELEGATE_SUBAGENT_TOOL_NAMES,
        SubagentKind::Explore => EXPLORE_SUBAGENT_TOOL_NAMES,
    }
    .iter()
    .copied()
    .collect::<BTreeSet<_>>();
    all_tool_specs
        .iter()
        .filter(|tool| names.contains(tool.spec.name.as_str()))
        .filter(|tool| {
            matches!(
                tool.capability,
                PermissionCapability::Read | PermissionCapability::Search
            )
        })
        .cloned()
        .collect()
}

async fn exploration_read_denial_reason(
    context: &ToolExecutionContext<'_>,
    call: &ToolCall,
) -> Option<&'static str> {
    context
        .exploration_state
        .lock()
        .await
        .read_denial_reason(call)
}

async fn record_exploration_tool_result(context: &ToolExecutionContext<'_>, result: &ToolResult) {
    context
        .exploration_state
        .lock()
        .await
        .record_tool_result(&result.tool_name, result.status == ToolStatus::Success);
}

async fn publish_task_state_update(
    tx: &mpsc::Sender<AgentEvent>,
    session_log: Option<&SessionHandle>,
    redactor: &Redactor,
    task_state: &Arc<Mutex<Option<TaskStateSnapshot>>>,
    turn_id: TurnId,
    snapshot: TaskStateSnapshot,
) {
    let snapshot = redact_task_state(snapshot.normalized(), redactor);
    {
        let mut state = task_state.lock().await;
        *state = Some(snapshot.clone());
    }
    log_session_event(
        session_log,
        redactor,
        "task_state",
        Some(turn_id),
        Some(snapshot.compact_summary()),
        json!({ "snapshot": snapshot }),
    );
    let _ = tx
        .send(AgentEvent::TaskStateUpdated { turn_id, snapshot })
        .await;
}

fn redact_task_state(mut snapshot: TaskStateSnapshot, redactor: &Redactor) -> TaskStateSnapshot {
    snapshot.task = redactor.redact(&snapshot.task).text;
    snapshot.summary = snapshot.summary.map(|value| redactor.redact(&value).text);
    snapshot.blocker = snapshot.blocker.map(|value| redactor.redact(&value).text);
    snapshot.next_action = snapshot
        .next_action
        .map(|value| redactor.redact(&value).text);
    snapshot.replan_reason = snapshot
        .replan_reason
        .map(|value| redactor.redact(&value).text);
    snapshot.steps = snapshot
        .steps
        .into_iter()
        .map(|mut step| {
            step.title = redactor.redact(&step.title).text;
            step.detail = step.detail.map(|value| redactor.redact(&value).text);
            step
        })
        .collect();
    snapshot.recent_changes = snapshot
        .recent_changes
        .into_iter()
        .map(|value| redactor.redact(&value).text)
        .collect();
    snapshot.normalized()
}

fn control_tool_result(call: &ToolCall, status: ToolStatus, content: Value) -> ToolResult {
    let output = serde_json::to_vec(&content).unwrap_or_default();
    ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.name.clone(),
        status,
        content,
        cost_hint: ToolCostHint {
            output_bytes: output.len() as u64,
            ..ToolCostHint::default()
        },
        receipt: ToolReceipt {
            output_sha256: sha256_hex(output),
            content_sha256: None,
        },
        spill_model_output: None,
    }
}

fn has_invalid_tool_arguments(call: &ToolCall) -> bool {
    call.arguments
        .get(INVALID_TOOL_ARGUMENTS_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn invalid_tool_arguments_result(call: &ToolCall) -> ToolResult {
    let parse_error = call
        .arguments
        .get(INVALID_TOOL_ARGUMENTS_ERROR_KEY)
        .and_then(Value::as_str)
        .unwrap_or("invalid JSON");
    let raw = call
        .arguments
        .get(INVALID_TOOL_ARGUMENTS_RAW_KEY)
        .and_then(Value::as_str)
        .unwrap_or_default();
    control_tool_result(
        call,
        ToolStatus::Error,
        json!({
            "ok": false,
            "error": "invalid tool arguments from model",
            "parse_error": parse_error,
            "raw_arguments_preview": compact_text(raw, 240),
            "retry": "call the same tool again with complete valid JSON arguments",
        }),
    )
}

fn tool_round_failure_summary(results: &[ToolResult]) -> Option<String> {
    let mut invalid_counts = BTreeMap::<String, usize>::new();
    let mut last_error = None;
    for result in results {
        if result.status != ToolStatus::Error && result.status != ToolStatus::Stale {
            continue;
        }
        let error = tool_failure_detail(result);
        if error.contains("invalid tool arguments") {
            *invalid_counts.entry(result.tool_name.clone()).or_default() += 1;
        }
        last_error = Some(format!("last {} failure: {error}", result.tool_name));
    }
    invalid_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(tool, count)| {
            if count > 1 {
                format!("repeated invalid {tool} arguments ({count}x)")
            } else {
                format!("invalid {tool} arguments")
            }
        })
        .or(last_error)
}

#[derive(Default)]
struct ToolLoopGuard {
    control_only_rounds: usize,
    failure_counts: BTreeMap<String, usize>,
}

impl ToolLoopGuard {
    fn observe_round(&mut self, calls: &[ToolCall], results: &[ToolResult]) -> Option<String> {
        if !calls.is_empty() && calls.iter().all(|call| is_control_tool_name(&call.name)) {
            self.control_only_rounds += 1;
            if self.control_only_rounds > MAX_CONTROL_ONLY_TOOL_ROUNDS {
                return Some(
                    "agent only updated internal task state; stopping before burning more tool rounds"
                        .to_string(),
                );
            }
        } else {
            self.control_only_rounds = 0;
        }

        for result in results {
            let Some(key) = repeated_tool_failure_key(result) else {
                continue;
            };
            let count = self.failure_counts.entry(key).or_default();
            *count += 1;
            if *count >= 2 {
                return Some(format!(
                    "repeated {} failure: {}; stopping before burning more tool rounds",
                    result.tool_name,
                    tool_failure_detail(result)
                ));
            }
        }
        None
    }
}

fn repeated_tool_failure_key(result: &ToolResult) -> Option<String> {
    if result.status != ToolStatus::Error && result.status != ToolStatus::Stale {
        return None;
    }
    let path = result
        .content
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(format!(
        "{}:{:?}:{path}:{}",
        result.tool_name,
        result.status,
        tool_failure_detail(result)
    ))
}

fn is_control_tool_name(name: &str) -> bool {
    matches!(name, TASK_STATE_TOOL_NAME | LOAD_TOOL_SCHEMA_TOOL_NAME)
}

fn tool_failure_detail(result: &ToolResult) -> String {
    if let Some(error) = result
        .content
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| result.content.get("parse_error").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return compact_text(error, 180);
    }
    if let Some(code) = result.content.get("exit_code").and_then(Value::as_i64) {
        return format!("exit {code}");
    }
    if let Some(signal) = result.content.get("signal").and_then(Value::as_i64) {
        return format!("signal {signal}");
    }
    for key in ["stderr", "stdout"] {
        if let Some(line) = result
            .content
            .get(key)
            .and_then(Value::as_str)
            .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        {
            return compact_text(line, 180);
        }
    }
    "tool failed".to_string()
}

async fn execute_tool_calls(
    calls: Vec<ToolCall>,
    context: ToolExecutionContext<'_>,
    broker: &mut CostBroker,
) -> Vec<ToolResult> {
    let _mcp_elicitation_handler = install_mcp_elicitation_handler(&context);
    let mut approved = Vec::new();
    let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
    let mut recorded = vec![false; calls.len()];

    for (index, call) in calls.iter().enumerate() {
        if context.cancel.is_cancelled() {
            let result = ToolResult::cancelled(call);
            broker.record_executed_result(&result);
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            return collect_recorded_results(
                results,
                recorded,
                broker,
                context.config,
                &context.telemetry,
            );
        }
        if call.name == TASK_STATE_TOOL_NAME {
            results[index] = Some(handle_task_state_call(&context, call).await);
            recorded[index] = true;
            continue;
        }
        if call.name == LOAD_TOOL_SCHEMA_TOOL_NAME {
            results[index] = Some(handle_load_tool_schema_call(&context, call).await);
            recorded[index] = true;
            continue;
        }
        if has_invalid_tool_arguments(call) {
            let result = invalid_tool_arguments_result(call);
            broker.record_executed_result(&result);
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            continue;
        }
        if call.name == DELEGATE_TOOL_NAME {
            results[index] = Some(
                Box::pin(handle_subagent_call(
                    &context,
                    call,
                    SubagentKind::Delegate,
                    broker,
                ))
                .await,
            );
            recorded[index] = true;
            continue;
        }
        if call.name == EXPLORE_TOOL_NAME {
            results[index] = Some(
                Box::pin(handle_subagent_call(
                    &context,
                    call,
                    SubagentKind::Explore,
                    broker,
                ))
                .await,
            );
            recorded[index] = true;
            continue;
        }

        let tool_sequence = match broker.reserve_call() {
            Ok(tool_sequence) => tool_sequence,
            Err((tool_sequence, reason)) => {
                let result = budget_denied_result(call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    call,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
        };

        if let Some(reason) = exploration_read_denial_reason(&context, call).await {
            let result = ToolResult::denied(call, reason);
            broker.metrics.planner_refusals += 1;
            emit_tool_telemetry(
                context.config,
                &context.telemetry,
                context.turn_id,
                tool_sequence,
                call,
                &result,
                Duration::ZERO,
            );
            broker.record_executed_result(&result);
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            continue;
        }

        match permission_decision(call, &context).await {
            ApprovalDecision::Approved => approved.push((index, call.clone(), tool_sequence)),
            ApprovalDecision::Denied(reason) => {
                let result = ToolResult::denied(call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    call,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                recorded[index] = true;
            }
            ApprovalDecision::Cancelled => {
                let result = ToolResult::cancelled(call);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    call,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                recorded[index] = true;
                return collect_recorded_results(
                    results,
                    recorded,
                    broker,
                    context.config,
                    &context.telemetry,
                );
            }
        }
    }

    let mut parallel_batch = Vec::new();
    for (index, call, tool_sequence) in approved {
        if context.cancel.is_cancelled() {
            let result = ToolResult::cancelled(&call);
            emit_tool_telemetry(
                context.config,
                &context.telemetry,
                context.turn_id,
                tool_sequence,
                &call,
                &result,
                Duration::ZERO,
            );
            broker.record_executed_result(&result);
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
            recorded[index] = true;
            break;
        }
        if context.tools.is_parallel_safe(&call) {
            if let Some(reason) = broker.deny_reason() {
                let result = budget_denied_result(&call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &call,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
            parallel_batch.push((index, call, tool_sequence));
        } else {
            flush_parallel_batch(&context, broker, &mut results, &mut parallel_batch).await;
            if let Some(reason) = broker.deny_reason() {
                let result = budget_denied_result(&call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &call,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
            let result = run_one_tool(context.clone(), tool_sequence, call).await;
            broker.record_executed_result(&result);
            results[index] = Some(result);
            recorded[index] = true;
        }
    }
    flush_parallel_batch(&context, broker, &mut results, &mut parallel_batch).await;

    collect_recorded_results(
        results,
        recorded,
        broker,
        context.config,
        &context.telemetry,
    )
}

async fn replay_tool_calls(
    replay: &ReplayRuntime,
    calls: Vec<ToolCall>,
    turn_id: TurnId,
    tx: mpsc::Sender<AgentEvent>,
    broker: &mut CostBroker,
) -> squeezy_core::Result<Vec<ToolResult>> {
    let results = replay.replay_tool_results(&calls)?;
    for (call, result) in calls.iter().zip(results.iter()) {
        let _ = tx
            .send(AgentEvent::ToolCallStarted {
                turn_id,
                call: call.clone(),
            })
            .await;
        broker.record_executed_result(result);
        let _ = tx
            .send(AgentEvent::ToolCallCompleted {
                turn_id,
                result: result.clone(),
            })
            .await;
    }
    Ok(results)
}

fn collect_recorded_results(
    results: Vec<Option<ToolResult>>,
    _recorded: Vec<bool>,
    _broker: &mut CostBroker,
    _config: &AppConfig,
    _telemetry: &TelemetryClient,
) -> Vec<ToolResult> {
    results.into_iter().flatten().collect()
}

fn cancelled_tool_result(result: &ToolResult) -> bool {
    result.status == ToolStatus::Cancelled
}

async fn flush_parallel_batch(
    context: &ToolExecutionContext<'_>,
    broker: &mut CostBroker,
    results: &mut [Option<ToolResult>],
    batch: &mut Vec<(usize, ToolCall, u64)>,
) {
    if batch.is_empty() {
        return;
    }

    let calls = std::mem::take(batch);
    if context.cancel.is_cancelled() {
        for (index, call, tool_sequence) in calls {
            let result = ToolResult::cancelled(&call);
            emit_tool_telemetry(
                context.config,
                &context.telemetry,
                context.turn_id,
                tool_sequence,
                &call,
                &result,
                Duration::ZERO,
            );
            broker.record_executed_result(&result);
            let _ = context
                .tx
                .send(AgentEvent::ToolCallCompleted {
                    turn_id: context.turn_id,
                    result: result.clone(),
                })
                .await;
            results[index] = Some(result);
        }
        return;
    }
    if broker.enforces_result_budgets() {
        for (index, call, tool_sequence) in calls {
            if let Some(reason) = broker.deny_reason() {
                let result = budget_denied_result(&call, reason);
                emit_tool_telemetry(
                    context.config,
                    &context.telemetry,
                    context.turn_id,
                    tool_sequence,
                    &call,
                    &result,
                    Duration::ZERO,
                );
                broker.record_executed_result(&result);
                let _ = context
                    .tx
                    .send(AgentEvent::ToolCallCompleted {
                        turn_id: context.turn_id,
                        result: result.clone(),
                    })
                    .await;
                results[index] = Some(result);
                continue;
            }
            let result = run_one_tool(context.clone(), tool_sequence, call).await;
            broker.record_executed_result(&result);
            results[index] = Some(result);
        }
        return;
    }

    let completions =
        futures_util::stream::iter(calls.into_iter().map(|(index, call, tool_sequence)| {
            let context = context.clone();
            async move {
                let result = run_one_tool(context, tool_sequence, call).await;
                (index, result)
            }
        }))
        .buffer_unordered(context.config.max_parallel_tools.max(1))
        .collect::<Vec<_>>()
        .await;

    for (index, result) in completions {
        broker.record_executed_result(&result);
        results[index] = Some(result);
    }
}

async fn run_one_tool(
    context: ToolExecutionContext<'_>,
    tool_sequence: u64,
    call: ToolCall,
) -> ToolResult {
    if context.cancel.is_cancelled() {
        let result = ToolResult::cancelled(&call);
        emit_tool_telemetry(
            context.config,
            &context.telemetry,
            context.turn_id,
            tool_sequence,
            &call,
            &result,
            Duration::ZERO,
        );
        let _ = context
            .tx
            .send(AgentEvent::ToolCallCompleted {
                turn_id: context.turn_id,
                result: result.clone(),
            })
            .await;
        return result;
    }
    let tracked_job = job_kind_for_tool(&call.name).map(|kind| {
        let cancel = context.cancel.child_token();
        let snapshot = context.jobs.create(
            kind,
            context.tools.describe_call(&call),
            Some(context.turn_id),
            Some(call.name.clone()),
            Some(call.call_id.clone()),
            cancel.clone(),
        );
        log_job_lifecycle(
            context.session_log.as_ref(),
            &context.redactor,
            "job_queued",
            &snapshot,
        );
        (snapshot.id, cancel)
    });
    if let Some((job_id, _)) = &tracked_job
        && let Some(started) = context.jobs.start(*job_id)
    {
        log_job_lifecycle(
            context.session_log.as_ref(),
            &context.redactor,
            "job_started",
            &started,
        );
        let _ = context
            .tx
            .send(AgentEvent::JobUpdated { job: started })
            .await;
    }
    let _ = context
        .tx
        .send(AgentEvent::ToolCallStarted {
            turn_id: context.turn_id,
            call: redact_tool_call(call.clone(), &context.redactor),
        })
        .await;
    let started = Instant::now();
    // Capture a borrow-able snapshot of the call before it moves into the
    // tool registry, so paired-SHA telemetry (F06) can hash its arguments
    // when emitting the completion event.
    let call_for_telemetry = call.clone();
    let result = context
        .tools
        .execute_for_group(
            call,
            tracked_job
                .as_ref()
                .map(|(_, cancel)| cancel.clone())
                .unwrap_or_else(|| context.cancel.clone()),
            context.turn_id.to_string(),
        )
        .await;
    record_exploration_tool_result(&context, &result).await;
    if let Some((job_id, _)) = tracked_job {
        let status = job_status_for_tool_status(result.status);
        let summary = tool_result_summary(&result);
        let output_handle = tool_result_output_handle(&result);
        if let Some(done) = context.jobs.finish(job_id, status, summary, output_handle) {
            log_job_lifecycle(
                context.session_log.as_ref(),
                &context.redactor,
                "job_finished",
                &done,
            );
            let _ = context
                .tx
                .send(AgentEvent::JobUpdated { job: done.clone() })
                .await;
            if let Some(notification) = context
                .jobs
                .notifications()
                .into_iter()
                .rev()
                .find(|notification| notification.job_id == done.id)
            {
                let _ = context
                    .tx
                    .send(AgentEvent::JobNotification { notification })
                    .await;
            }
        }
    }
    emit_tool_telemetry(
        context.config,
        &context.telemetry,
        context.turn_id,
        tool_sequence,
        &call_for_telemetry,
        &result,
        started.elapsed(),
    );
    let _ = context
        .tx
        .send(AgentEvent::ToolCallCompleted {
            turn_id: context.turn_id,
            result: result.clone(),
        })
        .await;
    result
}

#[derive(Debug)]
struct CostBroker {
    max_tool_calls: u64,
    max_bytes_read: u64,
    max_search_files: u64,
    metrics: TurnMetrics,
}

impl CostBroker {
    fn new(config: &AppConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls_per_turn,
            max_bytes_read: config.max_tool_bytes_read_per_turn,
            max_search_files: config.max_search_files_per_turn,
            metrics: TurnMetrics::default(),
        }
    }

    fn reserve_call(&mut self) -> Result<u64, (u64, String)> {
        self.metrics.tool_calls += 1;
        let tool_sequence = self.metrics.tool_calls;
        if tool_sequence > self.max_tool_calls {
            Err((
                tool_sequence,
                format!(
                    "per-turn tool-call budget exceeded: limit={}",
                    self.max_tool_calls
                ),
            ))
        } else {
            Ok(tool_sequence)
        }
    }

    fn deny_reason(&self) -> Option<String> {
        if self.metrics.bytes_read >= self.max_bytes_read {
            Some(format!(
                "per-turn tool byte-read budget exceeded: limit={}",
                self.max_bytes_read
            ))
        } else if self.metrics.files_scanned >= self.max_search_files {
            Some(format!(
                "per-turn search file-scan budget exceeded: limit={}",
                self.max_search_files
            ))
        } else {
            None
        }
    }

    fn enforces_result_budgets(&self) -> bool {
        self.max_bytes_read < u64::MAX || self.max_search_files < u64::MAX
    }

    fn record_executed_result(&mut self, result: &ToolResult) {
        match result.status {
            ToolStatus::Success => self.metrics.tool_successes += 1,
            ToolStatus::Error | ToolStatus::Stale => self.metrics.tool_errors += 1,
            ToolStatus::Denied => self.metrics.tool_denials += 1,
            ToolStatus::Cancelled => self.metrics.tool_cancellations += 1,
        }
        self.metrics.files_scanned += result.cost_hint.files_scanned;
        self.metrics.bytes_read += result.cost_hint.bytes_read;
        self.metrics.matches_returned += result.cost_hint.matches_returned;
        self.metrics.redactions += result.cost_hint.redactions;
        if result.content.get("spilled").and_then(Value::as_bool) == Some(true) {
            self.metrics.spill_writes += 1;
        }
        if result.tool_name == "read_tool_output" && result.status == ToolStatus::Success {
            self.metrics.spill_reads += 1;
        }
        if is_budget_denied(result) {
            self.metrics.budget_denials += 1;
        }
    }

    fn record_model_result(&mut self, result: &ToolResult) {
        self.metrics.model_output_bytes += result.model_output().len() as u64;
        if result.content.get("receipt_stub").and_then(Value::as_bool) == Some(true) {
            self.metrics.receipt_stub_hits += 1;
        }
        if result
            .content
            .get("negative_receipt_stub")
            .and_then(Value::as_bool)
            == Some(true)
        {
            self.metrics.negative_receipt_hits += 1;
        }
        if is_budget_denied(result) {
            self.metrics.budget_denials += 1;
        }
    }
}

fn budget_denied_result(call: &ToolCall, reason: String) -> ToolResult {
    let content = json!({
        "error": reason,
        "budget_denied": true,
    });
    let output_bytes = serde_json::to_vec(&content).unwrap_or_default();
    ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.name.clone(),
        status: ToolStatus::Denied,
        content,
        cost_hint: ToolCostHint {
            output_bytes: output_bytes.len() as u64,
            truncated: true,
            ..ToolCostHint::default()
        },
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: None,
        },
        spill_model_output: None,
    }
}

fn emit_tool_telemetry(
    config: &AppConfig,
    telemetry: &TelemetryClient,
    turn_id: TurnId,
    tool_sequence: u64,
    call: &ToolCall,
    result: &ToolResult,
    duration: Duration,
) {
    let args_sha256 = tool_call_args_sha256(call);
    telemetry.spawn(TelemetryEvent::tool_completed(ToolTelemetryReport {
        provider: &config.provider,
        model: &config.model,
        turn_index: turn_id.get(),
        tool_sequence,
        tool_name: &result.tool_name,
        status: telemetry_tool_status(result.status),
        duration,
        cost: ToolCostProperties {
            files_scanned: result.cost_hint.files_scanned,
            bytes_read: result.cost_hint.bytes_read,
            matches_returned: result.cost_hint.matches_returned,
            output_bytes: result.cost_hint.output_bytes,
        },
        args_sha256: args_sha256.as_deref(),
        output_sha256: Some(result.receipt.output_sha256.as_str()),
        content_sha256: result.receipt.content_sha256.as_deref(),
    }));
}

/// SHA-256 of the canonical JSON arguments the model sent for a tool call.
/// Used to pair with `output_sha256` in telemetry (F06).
fn tool_call_args_sha256(call: &ToolCall) -> Option<String> {
    serde_json::to_vec(&call.arguments)
        .ok()
        .map(|bytes| squeezy_tools::sha256_hex(&bytes))
}

fn telemetry_tool_status(status: ToolStatus) -> TelemetryToolStatusKind {
    match status {
        ToolStatus::Success => TelemetryToolStatusKind::Success,
        ToolStatus::Error => TelemetryToolStatusKind::Error,
        ToolStatus::Denied => TelemetryToolStatusKind::Denied,
        ToolStatus::Stale => TelemetryToolStatusKind::Stale,
        ToolStatus::Cancelled => TelemetryToolStatusKind::Cancelled,
    }
}

fn is_budget_denied(result: &ToolResult) -> bool {
    result.content.get("budget_denied").and_then(Value::as_bool) == Some(true)
}

fn error_kind(error: &SqueezyError) -> ErrorKind {
    match error {
        SqueezyError::ProviderNotConfigured(_)
        | SqueezyError::ProviderRequest(_)
        | SqueezyError::ProviderStream(_) => ErrorKind::Provider,
        SqueezyError::Tool(_) => ErrorKind::Tool,
        SqueezyError::Permission(_) => ErrorKind::Permission,
        SqueezyError::Graph(_) => ErrorKind::Graph,
        SqueezyError::Io(_) => ErrorKind::Io,
        SqueezyError::Config(_) => ErrorKind::Config,
        SqueezyError::Agent(_)
        | SqueezyError::Terminal(_)
        | SqueezyError::Workspace(_)
        | SqueezyError::Parse(_) => ErrorKind::Unknown,
    }
}

async fn permission_decision(
    call: &ToolCall,
    context: &ToolExecutionContext<'_>,
) -> ApprovalDecision {
    if is_direct_user_shell_call(call) {
        return ApprovalDecision::Approved;
    }
    let request = context.tools.permission_request(call);
    let active_mode = load_session_mode(&context.session_mode);
    if let Some(verdict) = mode_permission_verdict(active_mode, &request) {
        log_permission_verdict(&request, &verdict);
        return ApprovalDecision::Denied(context.redactor.redact(&verdict.reason).text);
    }
    let session_rules = snapshot_session_rules(&context.session_rules);
    let mut verdict = context
        .config
        .permissions
        .evaluate_with_extra(&request, &session_rules);
    if should_classify_shell(context.config, context.provider.name(), &request, &verdict)
        && let Some(classifier) = classify_ambiguous_shell(
            context.provider.clone(),
            context.config,
            &request,
            context.cancel.clone(),
        )
        .await
    {
        verdict = classifier;
    }
    log_permission_verdict(&request, &verdict);
    match verdict.action {
        PermissionAction::Allow => ApprovalDecision::Approved,
        PermissionAction::Deny => {
            ApprovalDecision::Denied(context.redactor.redact(&verdict.reason).text)
        }
        PermissionAction::Ask => {
            let (decision_tx, decision_rx) = oneshot::channel();
            let approval_request = ToolApprovalRequest {
                id: context.approval_ids.fetch_add(1, Ordering::Relaxed),
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                scope: legacy_scope_for_capability(request.capability),
                permission: redact_permission_request(request.clone(), &context.redactor),
                matched_rule: verdict.matched_rule,
                reason: context.redactor.redact(&verdict.reason).text,
            };
            log_session_event(
                context.session_log.as_ref(),
                &context.redactor,
                "approval_requested",
                Some(context.turn_id),
                Some(call.name.clone()),
                json!({
                    "tool": call.name,
                    "call_id": call.call_id,
                    "permission": approval_request.permission,
                    "reason": approval_request.reason,
                }),
            );
            let send_approval = context.tx.send(AgentEvent::ApprovalRequested {
                turn_id: context.turn_id,
                request: approval_request,
                decision_tx,
            });
            let send_result = tokio::select! {
                _ = context.cancel.cancelled() => return ApprovalDecision::Cancelled,
                result = send_approval => result,
            };
            if send_result.is_err() {
                return ApprovalDecision::Denied("approval channel closed".to_string());
            }
            let decision = tokio::select! {
                _ = context.cancel.cancelled() => return ApprovalDecision::Cancelled,
                decision = decision_rx => decision,
            };
            log_session_event(
                context.session_log.as_ref(),
                &context.redactor,
                "approval_decided",
                Some(context.turn_id),
                Some(format!("{decision:?}")),
                json!({ "decision": format!("{decision:?}") }),
            );
            match decision {
                Ok(ToolApprovalDecision::Approved | ToolApprovalDecision::AllowOnce) => {
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AllowRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Allow,
                    );
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AllowRuleProject) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Project,
                        PermissionAction::Allow,
                    );
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AskRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Ask,
                    );
                    ApprovalDecision::Denied(
                        "user asked to require approval for future matching calls".to_string(),
                    )
                }
                Ok(ToolApprovalDecision::AskRuleProject) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Project,
                        PermissionAction::Ask,
                    );
                    ApprovalDecision::Denied(
                        "user asked to require approval for future matching calls".to_string(),
                    )
                }
                Ok(ToolApprovalDecision::Denied | ToolApprovalDecision::DenyOnce) => {
                    ApprovalDecision::Denied(permission_denied_reason(
                        &request,
                        "user denied tool call",
                    ))
                }
                Ok(ToolApprovalDecision::DenyRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Deny,
                    );
                    ApprovalDecision::Denied(permission_denied_reason(
                        &request,
                        "user denied and persisted a user rule",
                    ))
                }
                Ok(ToolApprovalDecision::DenyRuleProject) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Project,
                        PermissionAction::Deny,
                    );
                    ApprovalDecision::Denied(permission_denied_reason(
                        &request,
                        "user denied and persisted a project rule",
                    ))
                }
                Ok(ToolApprovalDecision::Cancelled) => ApprovalDecision::Cancelled,
                Err(_) => ApprovalDecision::Denied("approval was not answered".to_string()),
            }
        }
    }
}

fn is_direct_user_shell_call(call: &ToolCall) -> bool {
    call.name == "shell"
        && call.call_id.starts_with("local-shell-")
        && call
            .arguments
            .get("direct_user_shell")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// Lock-free read of the active session mode. Defaults to `Build` if the
/// stored byte is corrupted, but that path is unreachable in normal flow
/// because every writer goes through `SessionMode::to_u8`.
fn load_session_mode(session_mode: &Arc<AtomicU8>) -> SessionMode {
    let raw = session_mode.load(Ordering::Acquire);
    SessionMode::from_u8(raw).unwrap_or_else(|| {
        tracing::warn!(
            target: "squeezy::permissions",
            discriminant = raw,
            "unexpected session mode discriminant; defaulting to build",
        );
        SessionMode::Build
    })
}

pub(crate) fn mode_permission_verdict(
    mode: SessionMode,
    request: &PermissionRequest,
) -> Option<PermissionVerdict> {
    if !mode_refuses_capability(mode, request.capability) {
        return None;
    }
    Some(PermissionVerdict {
        action: PermissionAction::Deny,
        matched_rule: None,
        reason: format!(
            "{} mode refuses {}",
            mode.as_str(),
            request.capability.as_str()
        ),
    })
}

/// Single source of truth for whether a session mode forbids a capability.
/// Plan mode allows only Read and Search; Build mode allows everything (the
/// configured `PermissionPolicy` still applies). The capability list is
/// intentionally exhaustive (`match`) so adding a new capability is a
/// compile-time prompt to decide whether plan mode admits it.
fn mode_refuses_capability(mode: SessionMode, capability: PermissionCapability) -> bool {
    if mode == SessionMode::Build {
        return false;
    }
    match capability {
        PermissionCapability::Read | PermissionCapability::Search => false,
        PermissionCapability::Edit
        | PermissionCapability::Shell
        | PermissionCapability::Git
        | PermissionCapability::Network
        | PermissionCapability::Mcp
        | PermissionCapability::Compiler
        | PermissionCapability::Destructive => true,
    }
}

fn snapshot_session_rules(session_rules: &Arc<RwLock<Vec<PermissionRule>>>) -> Vec<PermissionRule> {
    session_rules
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_else(|err| {
            tracing::warn!(
                target: "squeezy::permissions",
                error = %err,
                "session permission rule lock was poisoned; treating session rules as empty",
            );
            Vec::new()
        })
}

fn log_session_mode_transition(from_mode: SessionMode, to_mode: SessionMode, source: &'static str) {
    tracing::info!(
        target: "squeezy::permissions",
        from_mode = %from_mode.as_str(),
        to_mode = %to_mode.as_str(),
        source,
        "session mode transition",
    );
}

fn log_permission_verdict(request: &PermissionRequest, verdict: &PermissionVerdict) {
    let matched_source = verdict
        .matched_rule
        .as_ref()
        .map(|rule| rule.source.as_str());
    let matched_target = verdict
        .matched_rule
        .as_ref()
        .map(|rule| rule.target.as_str());
    tracing::info!(
        target: "squeezy::permissions",
        tool = %request.tool_name,
        capability = %request.capability.as_str(),
        target = %request.target,
        risk = %request.risk.as_str(),
        action = %verdict.action.as_str(),
        matched_source,
        matched_target,
        reason = %verdict.reason,
        "permission verdict",
    );
}

fn should_classify_shell(
    config: &AppConfig,
    provider_name: &str,
    request: &PermissionRequest,
    verdict: &PermissionVerdict,
) -> bool {
    config.permissions.shell_classifier
        && provider_name != "scripted"
        && request.tool_name == "shell"
        && request.capability == PermissionCapability::Shell
        && verdict.action == PermissionAction::Ask
}

async fn classify_ambiguous_shell(
    provider: Arc<dyn LlmProvider>,
    config: &AppConfig,
    request: &PermissionRequest,
    cancel: CancellationToken,
) -> Option<PermissionVerdict> {
    let command = request.metadata.get("command")?;
    let prompt = format!(
        "Classify this shell command for a coding agent permission gate.\n\
Return compact JSON with fields {{\"action\": \"ask\"|\"deny\", \"reason\": string}}.\n\
Never return allow.\n\
Command: {command:?}\n\
Working target: {:?}",
        request.target
    );
    let llm_request = LlmRequest {
        model: config.model.clone(),
        instructions: "You classify shell-command risk for a local coding agent. Return JSON only."
            .to_string(),
        input: vec![LlmInputItem::UserText(prompt)],
        max_output_tokens: Some(80),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
    };
    let mut stream = provider.stream_response(llm_request, cancel.clone());
    let mut text = String::new();
    while let Some(event) = next_llm_stream_event(&mut stream, &cancel, config.stream_idle_timeout)
        .await
        .ok()?
    {
        match event {
            LlmEvent::TextDelta(delta) => text.push_str(&delta),
            LlmEvent::Completed { .. } => break,
            LlmEvent::Cancelled => return None,
            LlmEvent::Started | LlmEvent::ToolCall(_) => {}
        }
    }
    Some(parse_classifier_verdict(&text))
}

/// Parse the classifier's textual response into a verdict. Only `deny` can
/// flip the verdict; missing or unparseable output leaves the call as `ask`.
/// Made `pub(crate)` so tests can exercise the JSON parsing rules.
pub(crate) fn parse_classifier_verdict(text: &str) -> PermissionVerdict {
    let trimmed = text.trim();
    let action = extract_json_action(trimmed)
        .or_else(|| extract_loose_action(trimmed))
        .unwrap_or(PermissionAction::Ask);
    let reason_excerpt = compact_reason(trimmed);
    match action {
        PermissionAction::Deny => PermissionVerdict {
            action: PermissionAction::Deny,
            matched_rule: None,
            reason: format!("shell classifier denied command: {reason_excerpt}"),
        },
        // Allow from the classifier is intentionally disallowed - we keep the
        // verdict at Ask so a human still confirms.
        _ => PermissionVerdict {
            action: PermissionAction::Ask,
            matched_rule: None,
            reason: format!("shell classifier requires approval: {reason_excerpt}"),
        },
    }
}

fn extract_json_action(text: &str) -> Option<PermissionAction> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let candidate = &text[start..=end];
    let value: serde_json::Value = serde_json::from_str(candidate).ok()?;
    let action = value.get("action")?.as_str()?;
    match action.trim().to_ascii_lowercase().as_str() {
        "deny" | "denied" | "refuse" => Some(PermissionAction::Deny),
        "ask" | "prompt" | "confirm" => Some(PermissionAction::Ask),
        _ => None,
    }
}

fn extract_loose_action(text: &str) -> Option<PermissionAction> {
    // Defensive fallback when the model returns "action: deny" or similar
    // without strict JSON. Look for a colon-bound "action" field and read the
    // next bare word.
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("action")?;
    let after = &lower[idx + "action".len()..];
    let after = after.trim_start_matches(|c: char| !c.is_alphanumeric());
    if after.starts_with("deny") {
        Some(PermissionAction::Deny)
    } else if after.starts_with("ask") {
        Some(PermissionAction::Ask)
    } else {
        None
    }
}

fn compact_reason(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn legacy_scope_for_capability(capability: PermissionCapability) -> PermissionScope {
    match capability {
        PermissionCapability::Read | PermissionCapability::Search => PermissionScope::Read,
        PermissionCapability::Edit => PermissionScope::Edit,
        PermissionCapability::Network => PermissionScope::Web,
        PermissionCapability::Mcp => PermissionScope::Mcp,
        PermissionCapability::Shell
        | PermissionCapability::Git
        | PermissionCapability::Compiler
        | PermissionCapability::Destructive => PermissionScope::Shell,
    }
}

fn permission_denied_reason(request: &PermissionRequest, reason: &str) -> String {
    format!(
        "{reason}; capability={} target={} risk={}",
        request.capability.as_str(),
        request.target,
        request.risk.as_str()
    )
}

/// Install a user/project rule both into the in-memory session list and (best
/// effort) on disk. Returns immediately when the rule cannot be persisted; the
/// failure is logged but never bubbled to the caller, since the current call
/// has already been resolved by the approval response.
fn install_persistent_rule(
    context: &ToolExecutionContext<'_>,
    request: &PermissionRequest,
    source: PermissionRuleSource,
    action: PermissionAction,
) {
    let Some(rule) = permission_rule_for_persistence(request, source, action) else {
        tracing::warn!(
            target: "squeezy::permissions",
            capability = %request.capability.as_str(),
            target = %request.target,
            action = %action.as_str(),
            "refused to install permission rule (e.g. Allow on destructive capability)",
        );
        return;
    };

    match context.session_rules.write() {
        Ok(mut guard) => guard.push(rule.clone()),
        Err(err) => {
            tracing::warn!(
                target: "squeezy::permissions",
                error = %err,
                "could not install session permission rule",
            );
        }
    }

    let path = match persistence_path_for(context.config, source) {
        Some(path) => path,
        None => return,
    };
    if let Err(err) = write_permission_rule(&path, &rule) {
        tracing::warn!(
            target: "squeezy::permissions",
            path = %path.display(),
            error = %err,
            "failed to persist permission rule",
        );
    } else {
        tracing::info!(
            target: "squeezy::permissions",
            path = %path.display(),
            capability = %rule.capability,
            target = %rule.target,
            action = %rule.action.as_str(),
            source = %rule.source.as_str(),
            "persisted permission rule",
        );
    }
}

fn persistence_path_for(config: &AppConfig, source: PermissionRuleSource) -> Option<PathBuf> {
    match source {
        PermissionRuleSource::User => Some(default_settings_path()),
        PermissionRuleSource::Project => Some(config.workspace_root.join(PROJECT_SETTINGS_FILE)),
        PermissionRuleSource::Builtin | PermissionRuleSource::Session => None,
    }
}

fn write_permission_rule(path: &std::path::Path, rule: &PermissionRule) -> io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let reason = rule
        .reason
        .clone()
        .unwrap_or_else(|| "added from approval prompt".to_string());
    let mut text = String::new();
    text.push_str("\n[[permissions.rules]]\n");
    text.push_str(&format!(
        "capability = {}\n",
        escape_toml_basic_string(&rule.capability)
    ));
    text.push_str(&format!(
        "target = {}\n",
        escape_toml_basic_string(&rule.target)
    ));
    text.push_str(&format!(
        "action = {}\n",
        escape_toml_basic_string(rule.action.as_str())
    ));
    text.push_str(&format!(
        "source = {}\n",
        escape_toml_basic_string(rule.source.as_str())
    ));
    text.push_str(&format!("reason = {}\n", escape_toml_basic_string(&reason)));
    file.write_all(text.as_bytes())
}

/// Pick a rule shape to persist for this approval. Refuses Allow on any
/// destructive capability (regardless of target), and refuses Allow rules that
/// would broadly match all paths/commands via a `*` target.
pub(crate) fn permission_rule_for_persistence(
    request: &PermissionRequest,
    source: PermissionRuleSource,
    action: PermissionAction,
) -> Option<PermissionRule> {
    let mut rule = request.suggested_rules.first().cloned().unwrap_or_else(|| {
        PermissionRule::new(
            request.capability.as_str(),
            request.target.clone(),
            action,
            source,
            Some("added from approval prompt".to_string()),
        )
    });
    rule.action = action;
    rule.source = source;
    if action == PermissionAction::Allow {
        if request.capability == PermissionCapability::Destructive {
            return None;
        }
        if rule.capability == "destructive" {
            return None;
        }
        if squeezy_core::target_is_effectively_wildcard(&rule.target) {
            return None;
        }
    }
    Some(rule)
}

/// Pair of an LLM-facing tool spec and the capability used to decide whether
/// the tool is advertised in a given session mode. Carrying the capability
/// alongside the spec keeps the advertisement filter in lock-step with the
/// per-call permission decision: both consult the same enum, and the source
/// of truth lives in `squeezy-tools` next to each tool's builder.
#[derive(Clone)]
pub(crate) struct AdvertisedTool {
    spec: LlmToolSpec,
    capability: PermissionCapability,
}

pub(crate) fn advertised_tool(spec: ToolSpec) -> AdvertisedTool {
    AdvertisedTool {
        capability: spec.capability,
        spec: LlmToolSpec {
            name: spec.name,
            description: spec.description,
            parameters: spec.parameters,
            strict: false,
        },
    }
}

/// Synthetic control tools that are advertised to the model on every
/// request. Progress/task state is intentionally not model-visible: the
/// runtime derives visible working state from turn and tool lifecycle events,
/// so simple prompts cannot burn full model rounds on bookkeeping-only calls.
/// `delegate` and `explore` are gated on [`SubagentConfig::enabled`] /
/// `explore_enabled` so we don't spend prompt tokens advertising tools the
/// agent would refuse on every call.
fn core_control_tools(subagents: &SubagentConfig) -> Vec<AdvertisedTool> {
    let mut tools = Vec::new();
    if subagents.enabled {
        tools.push(delegate_advertised_tool());
        if subagents.explore_enabled {
            tools.push(explore_advertised_tool());
        }
    }
    tools
}

/// Synthetic control tool that promotes a discoverable tool's full schema
/// into the request `tools` array. It is intentionally **not** routed through
/// the `permissions.rules` engine: lazy loading is a model-facing UX
/// affordance, and the capability is `Read` so it stays available whenever
/// lazy loading itself is enabled and the session mode does not refuse read
/// capabilities.
fn load_tool_schema_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: LlmToolSpec {
            name: LOAD_TOOL_SCHEMA_TOOL_NAME.to_string(),
            description: "Attach the full JSON schema for a discoverable tool before using it."
                .to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the discoverable tool whose schema should be attached."
                    }
                },
                "required": ["name"]
            }),
            strict: false,
        },
    }
}

fn delegate_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: LlmToolSpec {
            name: DELEGATE_TOOL_NAME.to_string(),
            description: "Delegate broad research to an isolated subagent. The parent receives only a structured summary, supporting receipts, and separate spend metrics.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Natural language research task for the subagent."
                    },
                    "scope": {
                        "type": ["string", "null"],
                        "description": "Optional bounded scope such as paths, modules, symbols, or exclusions."
                    }
                },
                "required": ["prompt"]
            }),
            strict: false,
        },
    }
}

fn explore_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: LlmToolSpec {
            name: EXPLORE_TOOL_NAME.to_string(),
            description: "Ask a cheaper read-only exploration subagent to scan the codebase with Squeezy semantic tools and return a compact briefing before planning or executing.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Codebase question or task context to investigate."
                    },
                    "scope": {
                        "type": ["string", "null"],
                        "description": "Optional paths, crates, modules, symbols, or file patterns to focus on."
                    },
                    "thoroughness": {
                        "type": "string",
                        "enum": ["quick", "medium", "thorough"],
                        "description": "How broadly to scan. Default is medium."
                    }
                },
                "required": ["prompt"]
            }),
            strict: false,
        },
    }
}

fn advertised_tool_specs(tools: &[AdvertisedTool], mode: SessionMode) -> Vec<LlmToolSpec> {
    tools
        .iter()
        .filter(|tool| !mode_refuses_capability(mode, tool.capability))
        .map(|tool| tool.spec.clone())
        .collect()
}

fn synthetic_tool_by_name(name: &str) -> Option<AdvertisedTool> {
    match name {
        DELEGATE_TOOL_NAME => Some(delegate_advertised_tool()),
        EXPLORE_TOOL_NAME => Some(explore_advertised_tool()),
        LOAD_TOOL_SCHEMA_TOOL_NAME => Some(load_tool_schema_advertised_tool()),
        _ => None,
    }
}

fn transcript_shape(transcript: &[TranscriptItem]) -> TranscriptShape {
    let mut shape = TranscriptShape {
        items: transcript.len(),
        ..TranscriptShape::default()
    };
    for item in transcript {
        shape.bytes += item.content.len();
        match item.role {
            Role::User => shape.user += 1,
            Role::Assistant => shape.assistant += 1,
            Role::System => shape.system += 1,
        }
    }
    shape
}

fn conversation_shape(conversation: &[LlmInputItem]) -> ConversationShape {
    let mut shape = ConversationShape {
        items: conversation.len(),
        ..ConversationShape::default()
    };
    for item in conversation {
        match item {
            LlmInputItem::UserText(text) => {
                shape.user_text += 1;
                shape.text_bytes += text.len();
            }
            LlmInputItem::AssistantText(text) => {
                shape.assistant_text += 1;
                shape.text_bytes += text.len();
            }
            LlmInputItem::FunctionCall { arguments, .. } => {
                shape.function_calls += 1;
                shape.text_bytes += arguments.to_string().len();
            }
            LlmInputItem::FunctionCallOutput { output, .. } => {
                shape.function_outputs += 1;
                shape.tool_output_bytes += output.len();
            }
        }
    }
    shape
}

fn attachment_shape(attachments: &[ContextAttachment]) -> AttachmentShape {
    let mut shape = AttachmentShape {
        total: attachments.len(),
        ..AttachmentShape::default()
    };
    for attachment in attachments {
        shape.stored_bytes += attachment.stored_bytes;
        shape.redactions += attachment.redactions;
        match attachment.status {
            ContextAttachmentStatus::Attached => shape.active += 1,
            ContextAttachmentStatus::Removed => shape.removed += 1,
            ContextAttachmentStatus::Unsupported => shape.unsupported += 1,
        }
    }
    shape
}

fn request_tool_specs(
    tools: &[AdvertisedTool],
    mode: SessionMode,
    schema_config: &ToolSchemaConfig,
    loaded_tool_schemas: &[String],
) -> Vec<LlmToolSpec> {
    if !schema_config.lazy_schema_loading {
        return advertised_tool_specs(tools, mode);
    }

    // Per-round `LlmToolSpec` clones are intentional and bounded by the size
    // of the advertised tool set (~20 first-party tools today). If the spec
    // set grows materially — for example once MCP brings in many external
    // tools — switching `AdvertisedTool::spec` to `Arc<LlmToolSpec>` or
    // emitting `Vec<Arc<LlmToolSpec>>` from this function would zero the
    // per-round clone cost.
    let mut specs = Vec::new();
    let mut seen = BTreeSet::new();
    let advertised_names: BTreeSet<&str> =
        tools.iter().map(|tool| tool.spec.name.as_str()).collect();
    let synthetic_order = [
        DELEGATE_TOOL_NAME,
        EXPLORE_TOOL_NAME,
        LOAD_TOOL_SCHEMA_TOOL_NAME,
    ];
    for name in synthetic_order
        .into_iter()
        .filter(|name| {
            // Synthetic control tools may have been filtered out of
            // `core_control_tools` (e.g. subagents disabled). In that case
            // don't push them back into the request via name lookup.
            *name == LOAD_TOOL_SCHEMA_TOOL_NAME || advertised_names.contains(name)
        })
        .chain(schema_config.core.iter().map(String::as_str))
    {
        push_tool_spec_by_name(tools, name, mode, &mut specs, &mut seen);
    }
    for name in loaded_tool_schemas {
        push_tool_spec_by_name(tools, name, mode, &mut specs, &mut seen);
    }
    specs
}

fn push_tool_spec_by_name(
    tools: &[AdvertisedTool],
    name: &str,
    mode: SessionMode,
    specs: &mut Vec<LlmToolSpec>,
    seen: &mut BTreeSet<String>,
) {
    if !seen.insert(name.to_string()) {
        return;
    }
    if let Some(tool) = synthetic_tool_by_name(name) {
        if !mode_refuses_capability(mode, tool.capability) {
            specs.push(tool.spec);
        }
        return;
    }
    let Some(tool) = tools.iter().find(|tool| tool.spec.name == name) else {
        // Misconfigured `[tools].core` / `[tools].discoverable` entries (typos
        // or names that no longer exist in the registry) are surfaced once at
        // session start by `warn_unknown_tool_schema_names`. Silently skipping
        // here keeps the hot path allocation-free.
        return;
    };
    if !mode_refuses_capability(mode, tool.capability) {
        specs.push(tool.spec.clone());
    }
}

/// Emit `tracing::warn!` for any name in `[tools].core` or
/// `[tools].discoverable` that does not refer to a known tool. This is run
/// once at session start (when `all_tool_specs` is built) so a typo like
/// `core = ["webfectch"]` surfaces as an actionable warning instead of
/// disappearing silently in the hot path. Synthetic tools are always
/// considered known.
fn warn_unknown_tool_schema_names(
    all_tool_specs: &[AdvertisedTool],
    schema_config: &ToolSchemaConfig,
) {
    let mut known: BTreeSet<&str> = all_tool_specs
        .iter()
        .map(|tool| tool.spec.name.as_str())
        .collect();
    known.insert(DELEGATE_TOOL_NAME);
    known.insert(EXPLORE_TOOL_NAME);
    known.insert(LOAD_TOOL_SCHEMA_TOOL_NAME);
    for name in schema_config
        .core
        .iter()
        .chain(schema_config.discoverable.iter())
    {
        if !known.contains(name.as_str()) {
            tracing::warn!(
                target: "squeezy::tools",
                tool = %name,
                "[tools] entry references unknown tool; entry will be ignored"
            );
        }
    }
}

const TOOLS_INDEX_OPENER: &str = "<tools_index>\nDiscoverable tools are listed below with compact metadata. Use load_tool_schema before calling one of these tools.\n";
const TOOLS_INDEX_CLOSER: &str = "\n</tools_index>";

fn tool_schema_index(
    tools: &[AdvertisedTool],
    mode: SessionMode,
    schema_config: &ToolSchemaConfig,
) -> Option<String> {
    if !schema_config.lazy_schema_loading {
        return None;
    }
    let mut rows = tools
        .iter()
        .filter(|tool| {
            !mode_refuses_capability(mode, tool.capability)
                && !tool_is_core_schema(tool, schema_config)
        })
        .map(|tool| {
            format!(
                "- {} | capability={} | {}",
                tool.spec.name,
                tool.capability.as_str(),
                first_line_of_description(&tool.spec.description)
            )
        })
        .collect::<Vec<_>>();
    // Alphabetic ordering (not first-load order like `request_tool_specs`)
    // keeps the rendered `<tools_index>` byte-stable across rounds even if
    // the registry's iteration order shifts, which matters for provider-side
    // prompt-prefix caching.
    rows.sort();
    if rows.is_empty() {
        return None;
    }
    let mut index = String::with_capacity(
        TOOLS_INDEX_OPENER.len()
            + TOOLS_INDEX_CLOSER.len()
            + rows.iter().map(String::len).sum::<usize>()
            + rows.len(),
    );
    index.push_str(TOOLS_INDEX_OPENER);
    index.push_str(&rows.join("\n"));
    index.push_str(TOOLS_INDEX_CLOSER);
    Some(index)
}

fn instructions_with_tool_index(
    base: &str,
    tools: &[AdvertisedTool],
    mode: SessionMode,
    schema_config: &ToolSchemaConfig,
) -> String {
    match tool_schema_index(tools, mode, schema_config) {
        Some(index) => format!("{base}\n\n{index}"),
        None => base.to_string(),
    }
}

fn first_line_of_description(description: &str) -> String {
    description
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Returns `true` when `tool`'s full JSON schema must be sent on every
/// request (no lazy `load_tool_schema` hop). Tools fall into one of three
/// buckets:
///   * synthetic control tools (`delegate`, `explore`, `load_tool_schema`)
///     and every tool when lazy loading is disabled — always-core,
///   * names listed in `[tools].core` — explicit core,
///   * everything else (including names listed in `[tools].discoverable`
///     and any unknown name) — discoverable.
///
/// Returning `false` for the implicit-discoverable case is intentional: a
/// tool that is neither configured-core nor configured-discoverable should
/// default to discoverable so the cache prefix stays compact.
fn tool_is_core_schema(tool: &AdvertisedTool, schema_config: &ToolSchemaConfig) -> bool {
    let name = tool.spec.name.as_str();
    if matches!(
        name,
        DELEGATE_TOOL_NAME | EXPLORE_TOOL_NAME | LOAD_TOOL_SCHEMA_TOOL_NAME
    ) {
        return true;
    }
    if !schema_config.lazy_schema_loading {
        return true;
    }
    schema_config.core_contains(name)
}

fn llm_function_call_item(call: ToolCall, redactor: &Redactor) -> LlmInputItem {
    LlmInputItem::FunctionCall {
        call_id: call.call_id,
        name: call.name,
        arguments: redact_json_value(call.arguments, redactor),
    }
}

fn redact_llm_input_items(input: &[LlmInputItem], redactor: &Redactor) -> Vec<LlmInputItem> {
    input
        .iter()
        .cloned()
        .map(|item| match item {
            LlmInputItem::UserText(text) => LlmInputItem::UserText(redactor.redact(&text).text),
            LlmInputItem::AssistantText(text) => {
                LlmInputItem::AssistantText(redactor.redact(&text).text)
            }
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments: redact_json_value(arguments, redactor),
            },
            LlmInputItem::FunctionCallOutput { call_id, output } => {
                LlmInputItem::FunctionCallOutput {
                    call_id,
                    output: redactor.redact(&output).text,
                }
            }
        })
        .collect()
}

/// Scrub the user/UI-facing surfaces of a `PermissionRequest` so an approval
/// prompt cannot leak a secret that appeared in a shell command, file path,
/// or rule metadata. Capability and risk are enum-only and need no redaction.
fn redact_permission_request(
    mut request: PermissionRequest,
    redactor: &Redactor,
) -> PermissionRequest {
    request.target = redactor.redact(&request.target).text;
    request.summary = redactor.redact(&request.summary).text;
    request.metadata = request
        .metadata
        .into_iter()
        .map(|(key, value)| (key, redactor.redact(&value).text))
        .collect();
    request
}

fn redact_tool_call(mut call: ToolCall, redactor: &Redactor) -> ToolCall {
    call.arguments = redact_json_value(call.arguments, redactor);
    call
}

fn redact_json_value(value: Value, redactor: &Redactor) -> Value {
    match value {
        Value::String(text) => Value::String(redactor.redact(&text).text),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_value(item, redactor))
                .collect(),
        ),
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, redact_json_value(value, redactor)))
                .collect(),
        ),
        value => value,
    }
}

fn redact_error(error: SqueezyError, redactor: &Redactor) -> SqueezyError {
    match error {
        SqueezyError::Config(message) => SqueezyError::Config(redactor.redact(&message).text),
        SqueezyError::ProviderNotConfigured(message) => {
            SqueezyError::ProviderNotConfigured(redactor.redact(&message).text)
        }
        SqueezyError::ProviderRequest(message) => {
            SqueezyError::ProviderRequest(redactor.redact(&message).text)
        }
        SqueezyError::ProviderStream(message) => {
            SqueezyError::ProviderStream(redactor.redact(&message).text)
        }
        SqueezyError::Terminal(message) => SqueezyError::Terminal(redactor.redact(&message).text),
        SqueezyError::Agent(message) => SqueezyError::Agent(redactor.redact(&message).text),
        SqueezyError::Workspace(message) => SqueezyError::Workspace(redactor.redact(&message).text),
        SqueezyError::Parse(message) => SqueezyError::Parse(redactor.redact(&message).text),
        SqueezyError::Graph(message) => SqueezyError::Graph(redactor.redact(&message).text),
        SqueezyError::Tool(message) => SqueezyError::Tool(redactor.redact(&message).text),
        SqueezyError::Permission(message) => {
            SqueezyError::Permission(redactor.redact(&message).text)
        }
        SqueezyError::Io(error) => SqueezyError::Io(error),
    }
}

fn merge_cost(total: &mut CostSnapshot, next: &CostSnapshot) {
    total.input_tokens = add_optional(total.input_tokens, next.input_tokens);
    total.output_tokens = add_optional(total.output_tokens, next.output_tokens);
    total.reasoning_output_tokens =
        add_optional(total.reasoning_output_tokens, next.reasoning_output_tokens);
    total.cached_input_tokens = add_optional(total.cached_input_tokens, next.cached_input_tokens);
    total.cache_write_input_tokens = add_optional(
        total.cache_write_input_tokens,
        next.cache_write_input_tokens,
    );
    total.estimated_usd_micros =
        add_optional(total.estimated_usd_micros, next.estimated_usd_micros);
}

fn start_session_log(config: &AppConfig, provider: &str) -> Option<SessionHandle> {
    let store = SessionStore::open(config);
    let metadata = SessionMetadata::new(config, provider);
    match store.start_session(metadata) {
        Ok(handle) => {
            let _ = handle.append_event(SessionEvent::new(
                "session_started",
                None,
                Some("session started".to_string()),
                json!({}),
            ));
            Some(handle)
        }
        Err(error) => {
            tracing::warn!(
                target: "squeezy::sessions",
                %error,
                "session logging disabled for this run",
            );
            None
        }
    }
}

fn next_attachment_counter(attachments: &[ContextAttachment]) -> u64 {
    attachments
        .iter()
        .filter_map(|attachment| attachment.id.strip_prefix("att-"))
        .filter_map(|suffix| suffix.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

fn format_user_text_with_context(input: &str, attachments: &[ContextAttachment]) -> String {
    if attachments.is_empty() {
        return input.to_string();
    }
    let mut output = input.to_string();
    output.push_str("\n\nAttached context references:\n");
    for attachment in attachments {
        output.push_str(&format!(
            "- {reference} id={id} source={source} kind={kind} label={label:?} bytes={bytes} stored_bytes={stored_bytes} truncated={truncated}\n",
            reference = attachment.reference(),
            id = attachment.id,
            source = attachment.source.as_str(),
            kind = attachment.kind.as_str(),
            label = attachment.label,
            bytes = attachment.original_bytes,
            stored_bytes = attachment.stored_bytes,
            truncated = attachment.truncated,
        ));
        if let Some(path) = &attachment.path {
            output.push_str(&format!("  path={path:?}\n"));
        }
        if !attachment.preview.is_empty() {
            output.push_str("  redacted_preview:\n");
            for line in attachment.preview.lines().take(20) {
                output.push_str("    ");
                output.push_str(line);
                output.push('\n');
            }
        }
    }
    output
}

fn redact_json_payload(payload: Value, redactor: &Redactor) -> Value {
    match payload {
        Value::String(text) => Value::String(redactor.redact(&text).text),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_payload(item, redactor))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_json_payload(value, redactor)))
                .collect(),
        ),
        // Numbers, booleans, and null cannot contain redactable text and we
        // intentionally do not touch JSON object keys so the resulting value
        // keeps a stable shape for callers that index into the payload.
        other => other,
    }
}

fn log_session_event(
    session: Option<&SessionHandle>,
    redactor: &Redactor,
    kind: &str,
    turn_id: Option<TurnId>,
    summary: Option<String>,
    payload: Value,
) {
    let Some(session) = session else {
        return;
    };
    let summary = summary.map(|value| redactor.redact(&value).text);
    let payload = redact_json_payload(payload, redactor);
    let _ = session.append_event(SessionEvent::new(
        kind,
        turn_id.map(|value| value.to_string()),
        summary,
        payload,
    ));
}

fn log_job_lifecycle(
    session: Option<&SessionHandle>,
    redactor: &Redactor,
    kind: &str,
    job: &JobSnapshot,
) {
    log_session_event(
        session,
        redactor,
        kind,
        job.turn_id,
        Some(format!(
            "job {} {} {}",
            job.id,
            job.status.as_str(),
            job.title
        )),
        json!({
            "job": job_snapshot_json(job),
        }),
    );
}

fn job_snapshot_json(job: &JobSnapshot) -> Value {
    json!({
        "id": job.id,
        "kind": job.kind.as_str(),
        "status": job.status.as_str(),
        "title": &job.title,
        "progress": job.progress.as_ref().map(|progress| json!({
            "completed": progress.completed,
            "total": progress.total,
            "message": &progress.message,
        })),
        "result_summary": job.result_summary.as_ref(),
        "output_handle": job.output_handle.as_ref(),
        "turn_id": job.turn_id.map(|turn_id| turn_id.to_string()),
        "tool_name": job.tool_name.as_ref(),
        "call_id": job.call_id.as_ref(),
        "created_at_ms": job.created_at_ms,
        "updated_at_ms": job.updated_at_ms,
        "ended_at_ms": job.ended_at_ms,
    })
}

fn job_kind_for_tool(name: &str) -> Option<JobKind> {
    match name {
        "shell" => Some(JobKind::Shell),
        "verify" => Some(JobKind::Verify),
        "symbol_context" | "diff_context" => Some(JobKind::Indexing),
        _ => None,
    }
}

fn job_status_for_tool_status(status: ToolStatus) -> JobStatus {
    match status {
        ToolStatus::Success => JobStatus::Completed,
        ToolStatus::Cancelled => JobStatus::Cancelled,
        ToolStatus::Error | ToolStatus::Denied | ToolStatus::Stale => JobStatus::Failed,
    }
}

fn implicit_skill_names(
    results: &[ToolResult],
    active_skill_names: &BTreeSet<String>,
) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut names = Vec::new();
    for result in results {
        let Some(name) = result
            .content
            .get("implicit_skill_activation")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if active_skill_names.contains(name) || !seen.insert(name.to_string()) {
            continue;
        }
        names.push(name.to_string());
    }
    names
}

fn tool_result_summary(result: &ToolResult) -> String {
    let mut parts = vec![format!("{} {:?}", result.tool_name, result.status)];
    if let Some(name) = result
        .content
        .get("implicit_skill_activation")
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
    {
        parts.push(format!("implicit_skill={name}"));
    }
    if let Some(exit_code) = result.content.get("exit_code").and_then(Value::as_i64) {
        parts.push(format!("exit={exit_code}"));
    }
    if let Some(error) = result.content.get("error").and_then(Value::as_str)
        && !error.trim().is_empty()
    {
        parts.push(format!("error={}", collapse_status_text(error)));
    }
    if let Some(handle) = tool_result_output_handle(result) {
        parts.push(format!("handle={handle}"));
    }
    if result.cost_hint.output_bytes > 0 {
        parts.push(format!("output={}B", result.cost_hint.output_bytes));
    }
    if result.cost_hint.truncated {
        parts.push("truncated".to_string());
    }
    truncate_chars(&parts.join(" "), JOB_SUMMARY_MAX_CHARS)
}

fn tool_result_output_handle(result: &ToolResult) -> Option<String> {
    result
        .content
        .get("handle")
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .content
                .get("output")
                .and_then(|output| output.get("handle"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

fn collapse_status_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut output = text
        .chars()
        .take(max_chars.saturating_sub(13))
        .collect::<String>();
    output.push_str(" [truncated]");
    output
}

fn unix_timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn replay_hash(value: &impl Serialize) -> String {
    sha256_hex(serde_json::to_vec(value).unwrap_or_default())
}

fn replay_user_inputs(tape: &SessionReplayTape) -> Vec<String> {
    tape.events
        .iter()
        .filter(|event| event.kind == SessionReplayEventKind::UserMessage)
        .filter_map(|event| {
            event
                .payload
                .get("input")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect()
}

fn replay_provider_name(provider: &str) -> &'static str {
    match provider {
        "openai" => "openai",
        "anthropic" => "anthropic",
        "google" => "google",
        "azure_openai" => "azure_openai",
        "ollama" => "ollama",
        "bedrock" => "bedrock",
        "mock-openai" => "mock-openai",
        "mock-anthropic" => "mock-anthropic",
        "planner-probe" => "planner-probe",
        other if other.contains("anthropic") => "mock-anthropic",
        _ => "mock-openai",
    }
}

fn user_item_summary(item: &LlmInputItem) -> Option<String> {
    match item {
        LlmInputItem::UserText(text) => Some(text.clone()),
        _ => None,
    }
}

fn tool_output_summary(item: &LlmInputItem) -> Option<String> {
    match item {
        LlmInputItem::FunctionCallOutput { call_id, .. } => Some(format!("tool output {call_id}")),
        _ => None,
    }
}

fn llm_input_to_resume_item(item: LlmInputItem) -> ResumeItem {
    match item {
        LlmInputItem::UserText(text) => ResumeItem::UserText { text },
        LlmInputItem::AssistantText(text) => ResumeItem::AssistantText { text },
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => ResumeItem::FunctionCall {
            call_id,
            name,
            arguments,
        },
        LlmInputItem::FunctionCallOutput { call_id, output } => {
            ResumeItem::FunctionCallOutput { call_id, output }
        }
    }
}

fn resume_item_for_json(item: LlmInputItem) -> Value {
    serde_json::to_value(llm_input_to_resume_item(item))
        .unwrap_or_else(|_| json!({"error": "resume item serialization failed"}))
}

fn resume_item_to_llm_input(item: ResumeItem) -> LlmInputItem {
    match item {
        ResumeItem::UserText { text } => LlmInputItem::UserText(text),
        ResumeItem::AssistantText { text } => LlmInputItem::AssistantText(text),
        ResumeItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        },
        ResumeItem::FunctionCallOutput { call_id, output } => {
            LlmInputItem::FunctionCallOutput { call_id, output }
        }
    }
}

fn maybe_compact_conversation(
    conversation: &mut Vec<LlmInputItem>,
    state: &mut ContextCompactionState,
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
    trigger: ContextCompactionTrigger,
) -> Option<ContextCompactionReport> {
    if !config.context_compaction.enabled {
        return None;
    }
    let estimate = estimate_context(conversation);
    if estimate.items < config.context_compaction.min_items
        || estimate.estimated_tokens < config.context_compaction.estimated_tokens
    {
        return None;
    }
    compact_conversation(
        conversation,
        state,
        attachments,
        store,
        config,
        trigger,
        false,
    )
}

fn compact_conversation(
    conversation: &mut Vec<LlmInputItem>,
    state: &mut ContextCompactionState,
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
    trigger: ContextCompactionTrigger,
    force: bool,
) -> Option<ContextCompactionReport> {
    let before = estimate_context(conversation);
    let keep = config.context_compaction.recent_items.max(1);
    if !force && before.items <= keep {
        return None;
    }
    let initial_split = conversation.len().saturating_sub(keep);
    if initial_split == 0 {
        return None;
    }
    // Tool calls and their outputs are pushed as contiguous pairs in the
    // turn loop. If the naive split falls between a `FunctionCall` and
    // its matching `FunctionCallOutput`, the recent slice would start
    // with an orphan output whose `call_id` is no longer declared on the
    // wire — the OpenAI Responses provider rejects that input. Snap the
    // boundary forward so any leading `FunctionCallOutput` in `recent`
    // whose `FunctionCall` lives in `older` is absorbed back into older.
    let split = snap_compaction_split(conversation, initial_split);
    if split == 0 || split >= conversation.len() {
        return None;
    }

    let older = conversation[..split].to_vec();
    let recent = conversation[split..].to_vec();
    let generation = state.generation.saturating_add(1);
    let summary = build_compaction_summary(generation, state, &older, attachments, store, config);
    let mut compacted = Vec::with_capacity(recent.len() + 1);
    compacted.push(LlmInputItem::UserText(summary.clone()));
    compacted.extend(recent);
    let after = estimate_context(&compacted);
    if !force && after.bytes >= before.bytes {
        return None;
    }
    *conversation = compacted;

    let record = ContextCompactionRecord {
        generation,
        trigger,
        compacted_at_ms: unix_timestamp_millis(),
        before,
        after,
        dropped_items: split,
        summary_bytes: summary.len(),
    };
    state.generation = generation;
    state.summary = Some(summary.clone());
    state.last = Some(record.clone());
    state.history.push(record.clone());
    if state.history.len() > COMPACTION_MAX_HISTORY {
        let excess = state.history.len() - COMPACTION_MAX_HISTORY;
        state.history.drain(0..excess);
    }
    Some(ContextCompactionReport { record, summary })
}

/// Adjusts a proposed compaction split point so `recent` does not start
/// with a `FunctionCallOutput` whose declaring `FunctionCall` has been
/// dropped into `older`. The OpenAI Responses provider serializes each
/// `function_call_output` with a bare `call_id` and the API rejects any
/// payload where a `call_id` is not also present as a `function_call`.
///
/// The strategy is to scan forward from `initial_split` and skip past
/// any `FunctionCallOutput` items whose `call_id` was already declared
/// by a `FunctionCall` in the older slice. We stop once the next item
/// in the recent slice is either a non-tool item (text) or a fresh
/// `FunctionCall` that begins a new pair. The split may grow up to
/// `conversation.len()`; the caller treats `>= conversation.len()` as
/// "nothing left to compact" and bails out without bumping generation.
fn snap_compaction_split(conversation: &[LlmInputItem], initial_split: usize) -> usize {
    let mut split = initial_split;
    while split < conversation.len() {
        match &conversation[split] {
            LlmInputItem::FunctionCallOutput { call_id, .. } => {
                let declared_in_older = conversation[..split].iter().any(|item| match item {
                    LlmInputItem::FunctionCall {
                        call_id: declared, ..
                    } => declared == call_id,
                    _ => false,
                });
                if declared_in_older {
                    split += 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    split
}

fn estimate_context(conversation: &[LlmInputItem]) -> ContextEstimate {
    let bytes = conversation
        .iter()
        .map(llm_item_estimated_bytes)
        .fold(0usize, usize::saturating_add);
    ContextEstimate {
        bytes,
        estimated_tokens: estimated_tokens(bytes),
        items: conversation.len(),
    }
}

fn estimated_tokens(bytes: usize) -> u64 {
    bytes.saturating_add(3).saturating_div(4) as u64
}

fn llm_item_estimated_bytes(item: &LlmInputItem) -> usize {
    match item {
        LlmInputItem::UserText(text) | LlmInputItem::AssistantText(text) => text.len(),
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => call_id.len() + name.len() + arguments.to_string().len(),
        LlmInputItem::FunctionCallOutput { call_id, output } => call_id.len() + output.len(),
    }
}

fn build_compaction_summary(
    generation: u64,
    state: &ContextCompactionState,
    older: &[LlmInputItem],
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Squeezy compacted conversation context (generation {generation})."
    ));
    lines.push(
        "Preserve these durable facts, decisions, pinned entries, seen-file receipts, and unresolved questions; do not ask for raw output already summarized here unless it is needed again."
            .to_string(),
    );
    if let Some(summary) = &state.summary {
        lines.push(format!(
            "Previous compacted summary: {}",
            compact_text(summary, COMPACTION_PREVIOUS_SUMMARY_MAX_CHARS)
        ));
    }
    if !state.pinned.is_empty() {
        lines.push("Pinned context:".to_string());
        for pin in &state.pinned {
            lines.push(format!(
                "- {} {}: {}",
                pin.id,
                pin.label,
                compact_text(&pin.summary, COMPACTION_PIN_SUMMARY_MAX_CHARS)
            ));
        }
    }
    let decisions = durable_context_lines(older);
    if !decisions.is_empty() {
        lines.push("Durable conversation facts and decisions:".to_string());
        lines.extend(decisions);
    }
    let unresolved = unresolved_question_lines(older);
    if !unresolved.is_empty() {
        lines.push("Unresolved questions:".to_string());
        lines.extend(unresolved);
    }
    let active_attachments = attachments
        .iter()
        .filter(|attachment| attachment.is_active())
        .collect::<Vec<_>>();
    if !active_attachments.is_empty() {
        lines.push("Active attached context:".to_string());
        for attachment in active_attachments {
            lines.push(format!(
                "- {} {} {}B preview={}",
                attachment.id,
                attachment.kind.as_str(),
                attachment.original_bytes,
                compact_text(
                    &collapse_status_text(&attachment.preview),
                    COMPACTION_ATTACHMENT_PREVIEW_MAX_CHARS
                )
            ));
        }
    }
    if let Some(receipts) = receipt_summary_lines(store) {
        lines.push("Tool/file output receipts already seen:".to_string());
        lines.extend(receipts);
    }
    lines.push(format!(
        "Compacted {} older model-visible item(s); the most recent context remains verbatim after this summary.",
        older.len()
    ));
    let summary = lines.join("\n");
    context_attachment_preview(&summary, config.context_compaction.max_summary_bytes).0
}

fn durable_context_lines(items: &[LlmInputItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::UserText(text) => {
                let compact = compact_text(text, COMPACTION_DURABLE_LINE_MAX_CHARS);
                (!compact.is_empty()).then(|| format!("- user: {compact}"))
            }
            LlmInputItem::AssistantText(text) => {
                let compact = compact_text(text, COMPACTION_DURABLE_LINE_MAX_CHARS);
                let lower = compact.to_ascii_lowercase();
                (lower.contains("decision")
                    || lower.contains("decided")
                    || lower.contains("plan")
                    || lower.contains("assumption")
                    || lower.contains("must")
                    || lower.contains("should"))
                .then(|| format!("- assistant: {compact}"))
            }
            LlmInputItem::FunctionCall {
                name, arguments, ..
            } => Some(format!(
                "- tool call {name} args={}",
                compact_text(&arguments.to_string(), COMPACTION_TOOL_ARGS_MAX_CHARS)
            )),
            LlmInputItem::FunctionCallOutput { call_id, output } => Some(format!(
                "- tool output {call_id}: {}",
                compact_text(output, COMPACTION_TOOL_OUTPUT_MAX_CHARS)
            )),
        })
        .take(COMPACTION_DURABLE_LINES_LIMIT)
        .collect()
}

fn unresolved_question_lines(items: &[LlmInputItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::UserText(text) | LlmInputItem::AssistantText(text) => Some(text),
            _ => None,
        })
        .flat_map(|text| text.lines())
        .filter(|line| line.contains('?'))
        .map(|line| {
            format!(
                "- {}",
                compact_text(&collapse_status_text(line), COMPACTION_UNRESOLVED_MAX_CHARS)
            )
        })
        .take(COMPACTION_UNRESOLVED_LINES_LIMIT)
        .collect()
}

fn receipt_summary_lines(store: Option<&SqueezyStore>) -> Option<Vec<String>> {
    let store = store?;
    let mut receipts = store.tool_receipts().ok()?;
    if receipts.is_empty() {
        return None;
    }
    receipts.sort_by_key(|receipt| std::cmp::Reverse(receipt.created_unix_millis));
    let lines = receipts
        .into_iter()
        .take(COMPACTION_RECEIPT_LINES_LIMIT)
        .map(|receipt| {
            let summary = receipt.summary.unwrap_or_else(|| {
                format!(
                    "{} output {}B sha={}",
                    receipt.tool_name, receipt.model_output_bytes, receipt.stable_output_sha256
                )
            });
            format!("- {}", compact_text(&summary, COMPACTION_RECEIPT_MAX_CHARS))
        })
        .collect::<Vec<_>>();
    Some(lines)
}

fn next_context_pin_id(pins: &[ContextPin]) -> String {
    let next = pins
        .iter()
        .filter_map(|pin| pin.id.strip_prefix("pin-"))
        .filter_map(|raw| raw.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    format!("pin-{next:04}")
}

fn compact_text(text: &str, max_chars: usize) -> String {
    truncate_chars(&collapse_status_text(text), max_chars)
}

#[derive(Debug, Clone)]
struct SeenToolOutput {
    call_id: String,
    tool_name: String,
    stable_output_sha256: String,
    content_sha256: Option<String>,
    model_output_bytes: usize,
    summary: Option<String>,
}

impl SeenToolOutput {
    fn from_result(result: &ToolResult) -> Self {
        Self {
            call_id: result.call_id.clone(),
            tool_name: result.tool_name.clone(),
            stable_output_sha256: stable_output_sha256(result),
            content_sha256: result.receipt.content_sha256.clone(),
            model_output_bytes: result.model_output().len(),
            summary: Some(tool_result_summary(result)),
        }
    }
}

#[derive(Debug, Clone)]
struct PendingToolResult {
    result: ToolResult,
    remember: Option<SeenToolOutput>,
    same_as_current_call_id: Option<String>,
}

#[derive(Debug, Default)]
struct SeenToolOutputs {
    by_tool_output: BTreeMap<(String, String), SeenToolOutput>,
    store: Option<Arc<SqueezyStore>>,
}

impl SeenToolOutputs {
    fn from_store(store: Option<Arc<SqueezyStore>>) -> Self {
        let mut outputs = Self {
            by_tool_output: BTreeMap::new(),
            store,
        };
        if let Some(store) = outputs.store.as_deref()
            && let Ok(receipts) = store.tool_receipts()
        {
            for receipt in receipts {
                let seen = SeenToolOutput {
                    call_id: receipt.call_id,
                    tool_name: receipt.tool_name,
                    stable_output_sha256: receipt.stable_output_sha256,
                    content_sha256: receipt.content_sha256,
                    model_output_bytes: receipt.model_output_bytes,
                    summary: receipt.summary,
                };
                outputs
                    .by_tool_output
                    .entry((seen.tool_name.clone(), seen.stable_output_sha256.clone()))
                    .or_insert(seen);
            }
        }
        outputs
    }

    fn prepare_results(&self, results: Vec<ToolResult>) -> Vec<PendingToolResult> {
        let mut prepared = Vec::with_capacity(results.len());
        let mut seen = self
            .by_tool_output
            .iter()
            .map(|(key, seen)| {
                (
                    key.clone(),
                    RoundSeenToolOutput {
                        output: seen.clone(),
                        current_round: false,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        for result in results {
            prepared.push(Self::prepare_result(result, &mut seen));
        }
        prepared
    }

    fn prepare_result(
        result: ToolResult,
        seen: &mut BTreeMap<(String, String), RoundSeenToolOutput>,
    ) -> PendingToolResult {
        if !is_receipt_stub_candidate(&result) {
            return PendingToolResult {
                result,
                remember: None,
                same_as_current_call_id: None,
            };
        }

        let key = (result.tool_name.clone(), stable_output_sha256(&result));
        if let Some(seen) = seen.get(&key) {
            return PendingToolResult {
                result: receipt_stub_result(result, &seen.output),
                remember: None,
                same_as_current_call_id: seen.current_round.then(|| seen.output.call_id.clone()),
            };
        }

        let output = SeenToolOutput::from_result(&result);
        seen.insert(
            key,
            RoundSeenToolOutput {
                output: output.clone(),
                current_round: true,
            },
        );
        PendingToolResult {
            remember: Some(output),
            result,
            same_as_current_call_id: None,
        }
    }

    fn remember_results(&mut self, results: &[PendingToolResult]) {
        for result in results {
            if let Some(seen) = result.remember.clone() {
                self.by_tool_output
                    .entry((seen.tool_name.clone(), seen.stable_output_sha256.clone()))
                    .or_insert(seen.clone());
                if let Some(store) = self.store.as_deref() {
                    let _ = store.put_tool_receipt(&StoredToolReceipt {
                        tool_name: seen.tool_name.clone(),
                        stable_output_sha256: seen.stable_output_sha256.clone(),
                        call_id: seen.call_id.clone(),
                        content_sha256: seen.content_sha256.clone(),
                        model_output_bytes: seen.model_output_bytes,
                        created_unix_millis: unix_millis(),
                        summary: seen.summary.clone(),
                    });
                    if let Some(snapshot) = read_snapshot_from_result(&result.result, &seen) {
                        let _ = store.put_read_snapshot(&snapshot);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RoundSeenToolOutput {
    output: SeenToolOutput,
    current_round: bool,
}

fn is_receipt_stub_candidate(result: &ToolResult) -> bool {
    result.status == ToolStatus::Success
        && matches!(
            result.tool_name.as_str(),
            "decl_search"
                | "definition_search"
                | "downstream_flow"
                | "glob"
                | "grep"
                | "hierarchy"
                | "read_file"
                | "read_slice"
                | "read_tool_output"
                | "reference_search"
                | "repo_map"
                | "symbol_context"
                | "upstream_flow"
                | "webfetch"
                | "websearch"
        )
}

fn stable_output_sha256(result: &ToolResult) -> String {
    result
        .content
        .get("cache_receipt")
        .and_then(|value| value.get("stable_output_sha256"))
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .content
                .get("original_output_sha256")
                .and_then(Value::as_str)
        })
        .unwrap_or(&result.receipt.output_sha256)
        .to_string()
}

fn read_snapshot_from_result(
    result: &ToolResult,
    seen: &SeenToolOutput,
) -> Option<StoredReadSnapshot> {
    if !matches!(result.tool_name.as_str(), "read_file" | "read_slice") {
        return None;
    }
    if result.content.get("read_mode").and_then(Value::as_str) == Some("diff") {
        return None;
    }
    let path = result.content.get("path")?.as_str()?.to_string();
    let content = result.content.get("content")?.as_str()?.to_string();
    let start_byte = result
        .content
        .get("offset")
        .and_then(Value::as_u64)
        .or_else(|| result.content.get("start_byte").and_then(Value::as_u64))
        .unwrap_or(0);
    let bytes_returned = result.content.get("bytes_returned")?.as_u64()?;
    Some(StoredReadSnapshot {
        path,
        tool_name: seen.tool_name.clone(),
        call_id: seen.call_id.clone(),
        stable_output_sha256: seen.stable_output_sha256.clone(),
        content_sha256: seen.content_sha256.clone(),
        start_byte,
        end_byte: start_byte.saturating_add(bytes_returned),
        content,
        model_output_bytes: seen.model_output_bytes,
        created_unix_millis: unix_millis(),
    })
}

fn receipt_stub_result(result: ToolResult, seen: &SeenToolOutput) -> ToolResult {
    let negative_receipt_stub = is_negative_receipt_result(&result);
    let content = json!({
        "receipt_stub": true,
        "negative_receipt_stub": negative_receipt_stub,
        "message": "identical tool output already sent to the model in this turn",
        "same_as_call_id": &seen.call_id,
        "same_as_tool_name": &seen.tool_name,
        "original_output_sha256": &seen.stable_output_sha256,
        "original_content_sha256": &seen.content_sha256,
        "original_model_output_bytes": seen.model_output_bytes,
    });
    let output_bytes = serde_json::to_vec(&content).unwrap_or_default();
    let mut cost_hint = result.cost_hint;
    cost_hint.output_bytes = output_bytes.len() as u64;
    cost_hint.truncated = true;

    ToolResult {
        call_id: result.call_id,
        tool_name: result.tool_name,
        status: result.status,
        content,
        cost_hint,
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: result.receipt.content_sha256,
        },
        spill_model_output: None,
    }
}

fn is_negative_receipt_result(result: &ToolResult) -> bool {
    match result.tool_name.as_str() {
        "grep" => {
            result
                .content
                .get("matches")
                .and_then(Value::as_array)
                .is_some_and(|items| items.is_empty())
                || result
                    .content
                    .get("paths")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.is_empty())
                || result.content.get("count").and_then(Value::as_u64) == Some(0)
        }
        "glob" => result
            .content
            .get("paths")
            .and_then(Value::as_array)
            .is_some_and(|items| items.is_empty()),
        _ => false,
    }
}

fn pack_tool_results(
    results: Vec<PendingToolResult>,
    budget_bytes: usize,
) -> Vec<PendingToolResult> {
    if budget_bytes == 0 {
        return results;
    }

    let mut used = 0usize;
    let mut visible_current_call_ids = BTreeSet::new();
    results
        .into_iter()
        .map(|mut pending| {
            if pending
                .same_as_current_call_id
                .as_ref()
                .is_some_and(|call_id| !visible_current_call_ids.contains(call_id))
            {
                pending.result = receipt_stub_reference_omitted(pending.result);
                pending.remember = None;
                pending.same_as_current_call_id = None;
            }

            let bytes = pending.result.model_output().len();
            if used.saturating_add(bytes) <= budget_bytes {
                used += bytes;
                if pending.remember.is_some() {
                    visible_current_call_ids.insert(pending.result.call_id.clone());
                }
                pending
            } else {
                let compact = pending
                    .result
                    .aggregate_budget_exceeded(budget_bytes, bytes);
                used = used.saturating_add(compact.model_output().len());
                PendingToolResult {
                    result: compact,
                    remember: None,
                    same_as_current_call_id: None,
                }
            }
        })
        .collect()
}

fn receipt_stub_reference_omitted(result: ToolResult) -> ToolResult {
    let content = json!({
        "error": "tool result omitted because the identical result it references was omitted by the aggregate tool-result budget",
    });
    let output_bytes = serde_json::to_vec(&content).unwrap_or_default();

    ToolResult {
        call_id: result.call_id,
        tool_name: result.tool_name,
        status: ToolStatus::Error,
        content,
        cost_hint: ToolCostHint {
            output_bytes: output_bytes.len() as u64,
            truncated: true,
            ..Default::default()
        },
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: result.receipt.content_sha256,
        },
        spill_model_output: None,
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolApprovalRequest {
    pub id: u64,
    pub call_id: String,
    pub tool_name: String,
    pub scope: PermissionScope,
    pub permission: PermissionRequest,
    pub matched_rule: Option<PermissionRule>,
    pub reason: String,
}

impl ToolApprovalRequest {
    pub fn summary(&self) -> &str {
        &self.permission.summary
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    Approved,
    Denied,
    AllowOnce,
    AllowRuleUser,
    AllowRuleProject,
    AskRuleUser,
    AskRuleProject,
    DenyOnce,
    DenyRuleUser,
    DenyRuleProject,
    Cancelled,
}

enum ApprovalDecision {
    Approved,
    Denied(String),
    Cancelled,
}

#[derive(Debug)]
pub enum AgentEvent {
    UserMessage {
        turn_id: TurnId,
        message: TranscriptItem,
    },
    Started {
        turn_id: TurnId,
    },
    AssistantDelta {
        turn_id: TurnId,
        delta: String,
    },
    ToolCallQueued {
        turn_id: TurnId,
        call: ToolCall,
    },
    ToolCallStarted {
        turn_id: TurnId,
        call: ToolCall,
    },
    ToolCallCompleted {
        turn_id: TurnId,
        result: ToolResult,
    },
    TaskStateUpdated {
        turn_id: TurnId,
        snapshot: TaskStateSnapshot,
    },
    McpStatusUpdated {
        turn_id: TurnId,
        snapshot: McpStatusSnapshot,
    },
    McpElicitationRequested {
        turn_id: TurnId,
        request: McpElicitationRequest,
        response_tx: oneshot::Sender<McpElicitationResponse>,
    },
    JobUpdated {
        job: JobSnapshot,
    },
    JobNotification {
        notification: JobNotification,
    },
    ContextCompacted {
        turn_id: TurnId,
        report: ContextCompactionReport,
    },
    SubagentStarted {
        turn_id: TurnId,
        agent: String,
        prompt: String,
    },
    SubagentCompleted {
        turn_id: TurnId,
        agent: String,
        summary: String,
        metrics: TurnMetrics,
    },
    SubagentFailed {
        turn_id: TurnId,
        agent: String,
        error: String,
        metrics: TurnMetrics,
    },
    ApprovalRequested {
        turn_id: TurnId,
        request: ToolApprovalRequest,
        decision_tx: oneshot::Sender<ToolApprovalDecision>,
    },
    Completed {
        turn_id: TurnId,
        message: TranscriptItem,
        response_id: Option<String>,
        cost: CostSnapshot,
        metrics: TurnMetrics,
    },
    Cancelled {
        turn_id: TurnId,
    },
    Failed {
        turn_id: TurnId,
        error: SqueezyError,
    },
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
