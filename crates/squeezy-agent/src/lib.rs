use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, RwLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
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
};
use squeezy_hooks::{HookEvent, HookRegistry};
use squeezy_llm::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, LlmToolSpec,
    ReasoningPayload, ReasoningSnapshot, RequestTokenEstimate, capabilities_for, estimate_cost,
    estimate_request_context_calibrated, fetch_ollama_context_window,
};
use squeezy_skills::{
    BundledDoc, HelpAnswer, HelpStatus, SqueezyHelp, bundled_docs, matches_squeezy_help_input,
};
use squeezy_store::{
    BugReportBundle, BugReportOptions, CleanupReport, ResumeItem, SessionEvent, SessionEventKind,
    SessionHandle, SessionMetadata, SessionQuery, SessionRecord, SessionReplayEvent,
    SessionReplayEventKind, SessionReplayTape, SessionResumeState, SessionStatus, SessionStore,
    SqueezyStore,
};
use squeezy_telemetry::{
    ErrorKind, FeedbackClient, FeedbackSubmitResult, PreparedFeedback, ReportUpload,
    TelemetryClient, TelemetryEvent, ToolCostProperties, ToolStatusKind as TelemetryToolStatusKind,
    ToolTelemetryReport, prepare_feedback,
};
use squeezy_tools::{
    McpElicitationHandler, McpElicitationRequest, McpElicitationResponse, McpStatusSnapshot,
    ShellAskApprover, ShellAskDecision, ShellAskRequest, ShellBestEffortFallback, ToolCall,
    ToolCostHint, ToolExecutionOptions, ToolOutputConfig, ToolReceipt, ToolRegistry,
    ToolRegistryRuntime, ToolResult, ToolRuntimeConfig, ToolSpec, ToolStatus, WebToolConfig,
    sha256_hex, shell_best_effort_fallback_from_result,
};
use tokio::{
    sync::{Mutex, Notify, broadcast, mpsc, oneshot},
    task::AbortHandle,
};
use tokio_util::sync::CancellationToken;

mod ai_reviewer;
mod cancel;
mod context_compaction;
mod cost_broker;
mod exploration_compiler;
mod permission_persist;
mod plan_mode;
mod roles;

use cancel::{CancelErr, OrCancelExt};
use context_compaction::{
    PendingToolResult, SeenToolOutputs, compact_conversation_with_strategy,
    drop_orphan_function_call_outputs, estimate_context, maybe_compact_conversation,
    maybe_compact_mid_turn, next_context_pin_id, pack_tool_results,
};
#[cfg(test)]
use context_compaction::{build_compaction_summary, compact_conversation};
use cost_broker::{CostBroker, format_cap_reached_reason, llm_request_input_bytes};
use exploration_compiler::{ExplorationTurnState, compile_exploration_plan};
use permission_persist::persist_permission_rule;
use roles::{RoleModelPolicy, SubagentRole, role_config};

pub use ai_reviewer::{ReviewerAuditEntry, ReviewerAuditVerdict};
pub use context_compaction::ContextCompactionReport;
pub use cost_broker::CostCapStatus;
pub use plan_mode::{PROPOSED_PLAN_CLOSE_TAG, PROPOSED_PLAN_OPEN_TAG, strip_proposed_plan_blocks};

// Emergency belt on tool rounds per turn — codex and opencode loop
// unbounded; CC only caps explicit-purpose subagents (its
// `forkSubagent` uses 200). 200 keeps a true safety ceiling without
// truncating legitimate long-running exploration.
const MAX_TOOL_ROUNDS: usize = 200;
const MAX_CONTROL_ONLY_TOOL_ROUNDS: usize = 2;
const LOCAL_SHELL_TIMEOUT_MS: u64 = 10_000;
const LOCAL_SHELL_OUTPUT_BYTE_CAP: usize = 32_000;
const TASK_STATE_TOOL_NAME: &str = "update_task_state";
const LOAD_TOOL_SCHEMA_TOOL_NAME: &str = "load_tool_schema";
const DELEGATE_TOOL_NAME: &str = "delegate";
const EXPLORE_TOOL_NAME: &str = "explore";
const DELEGATE_PLAN_TOOL_NAME: &str = "delegate_plan";
const DELEGATE_REVIEW_TOOL_NAME: &str = "delegate_review";
const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";
pub const MAX_JOB_NOTIFICATIONS: usize = 20;
pub const MAX_JOBS_RETAINED: usize = 200;
const JOB_CANCEL_GRACE: Duration = Duration::from_millis(250);
const JOB_SUMMARY_MAX_CHARS: usize = 320;
const SUBAGENT_SUMMARY_CHARS_PER_TOKEN: usize = 4;
/// Deterministic-keys contract for Plan and Review subagents. The parser
/// reads the JSON object from the tail of the final assistant message so
/// the parent agent can iterate findings as structured data. Free-text
/// preambles before the JSON are preserved in `summary` and silently
/// ignored by the parser.
const SUBAGENT_JSON_TAIL_INSTRUCTION: &str = "Output contract: end your final assistant message with a single JSON object on its own line of the form `{\"findings\": [{\"finding\": \"...\", \"recommendation\": \"...\", \"priority\": \"blocker|warning|info\"}], \"summary\": \"...\"}`. Add no prose after the JSON object. If you have nothing to report, emit `{\"findings\": [], \"summary\": \"...\"}`.";
/// Maximum number of subagents that may be active at once for a single
/// parent Agent. The registry rejects further `start()` calls until an
/// in-flight subagent finishes (lease drops). Keeps fanout flat and
/// predictable rather than letting a model spawn an unbounded swarm.
const SUBAGENT_MAX_CONCURRENT: usize = 4;
// Compaction summary truncation budgets. These are character (not byte)
// caps because they pass through `compact_text` → `truncate_chars`. They
// stay collocated so a future audit can read the total summary growth
// in one place rather than chasing literals across `build_compaction_summary`.
pub(crate) const COMPACTION_PIN_SUMMARY_MAX_CHARS: usize = 400;

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
    token_calibration: squeezy_llm::TokenCalibration,
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
            token_calibration: metadata.token_calibration.clone(),
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
        let actual = replay_hash(&replay_request_view(request));
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
    pub reasoning_items: usize,
    pub text_bytes: usize,
    pub tool_output_bytes: usize,
    pub reasoning_bytes: usize,
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

pub type JobId = u64;
pub type SubagentId = u64;

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
    pub subagent_id: Option<SubagentId>,
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

#[derive(Debug)]
struct JobRecord {
    snapshot: JobSnapshot,
    cancel: CancellationToken,
    abort: Option<AbortHandle>,
    done: Option<Arc<Notify>>,
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
            subagent_id: None,
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
        self.finish_impl(id, status, summary, output_handle, false)
    }

    fn finish_if_active(
        &self,
        id: JobId,
        status: JobStatus,
        summary: impl Into<String>,
        output_handle: Option<String>,
    ) -> Option<JobSnapshot> {
        self.finish_impl(id, status, summary, output_handle, true)
    }

    fn finish_impl(
        &self,
        id: JobId,
        status: JobStatus,
        summary: impl Into<String>,
        output_handle: Option<String>,
        only_active: bool,
    ) -> Option<JobSnapshot> {
        let summary = truncate_chars(&summary.into(), JOB_SUMMARY_MAX_CHARS);
        let (snapshot, notification) = {
            let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            let record = state.jobs.get_mut(&id)?;
            if only_active && !record.snapshot.status.is_active() {
                return None;
            }
            record.snapshot.status = status;
            record.snapshot.result_summary = Some(summary);
            record.snapshot.output_handle = output_handle;
            record.snapshot.progress = Some(JobProgress {
                completed: Some(1),
                total: Some(1),
                message: status.as_str().to_string(),
            });
            record.snapshot.ended_at_ms = Some(unix_timestamp_millis());
            record.snapshot.updated_at_ms = unix_timestamp_millis();
            let snapshot = record.snapshot.clone();
            push_job_notification(&mut state, &snapshot);
            let notification = state.notifications.back().cloned();
            prune_completed_jobs(&mut state);
            (snapshot, notification)
        };
        let _ = self.tx.send(JobEvent::Updated(snapshot.clone()));
        if let Some(notification) = notification {
            let _ = self.tx.send(JobEvent::Notification(notification));
        }
        Some(snapshot)
    }

    fn attach_handle(&self, id: JobId, abort: AbortHandle, done: Arc<Notify>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        let Some(record) = state.jobs.get_mut(&id) else {
            return false;
        };
        if !record.snapshot.status.is_active() {
            return false;
        }
        record.abort = Some(abort);
        record.done = Some(done);
        true
    }

    pub fn cancel(&self, id: JobId) -> bool {
        let (cancel, abort, done) = {
            let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            let Some(record) = state.jobs.get(&id) else {
                return false;
            };
            if !record.snapshot.status.is_active() {
                return false;
            }
            (
                record.cancel.clone(),
                record.abort.clone(),
                record.done.clone(),
            )
        };
        cancel.cancel();
        let _ = self.progress(id, None, None, "cancellation requested");
        if let (Some(abort), Some(done)) = (abort, done) {
            let jobs = self.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = done.notified() => {}
                    _ = tokio::time::sleep(JOB_CANCEL_GRACE) => {
                        abort.abort();
                        if jobs
                            .finish_if_active(
                                id,
                                JobStatus::Cancelled,
                                "cancelled after grace window",
                                None,
                            )
                            .is_some()
                        {
                            done.notify_waiters();
                        }
                    }
                }
            });
        }
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
                abort: None,
                done: None,
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

#[derive(Clone, Debug)]
struct SubagentRegistry {
    state: Arc<StdMutex<BTreeMap<SubagentId, SubagentMetadata>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self {
            state: Arc::new(StdMutex::new(BTreeMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

// `id`, `role`, `started_at_ms`, and `last_status_message` are recorded so
// future code (UI surfaces, telemetry, /subagents introspection) can read
// the live registry without a second source of truth. They're written by
// `start` / `update_status` and read via `snapshot` from tests today.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct SubagentMetadata {
    id: SubagentId,
    role: SubagentRole,
    started_at_ms: u64,
    cancel: CancellationToken,
    last_status_message: Option<String>,
}

#[derive(Debug)]
struct SubagentLease {
    id: SubagentId,
    registry: SubagentRegistry,
}

impl Drop for SubagentLease {
    fn drop(&mut self) {
        self.registry.finish(self.id);
    }
}

impl SubagentRegistry {
    fn start(
        &self,
        role: SubagentRole,
        cancel: CancellationToken,
        max_concurrent: usize,
        status: impl Into<String>,
    ) -> Result<SubagentLease, String> {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        let active = state
            .values()
            .filter(|metadata| !metadata.cancel.is_cancelled())
            .count();
        if active >= max_concurrent.max(1) {
            return Err(format!(
                "subagent concurrency limit reached ({})",
                max_concurrent.max(1)
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        state.insert(
            id,
            SubagentMetadata {
                id,
                role,
                started_at_ms: unix_timestamp_millis(),
                cancel,
                last_status_message: Some(status.into()),
            },
        );
        Ok(SubagentLease {
            id,
            registry: self.clone(),
        })
    }

    #[allow(dead_code)]
    fn update_status(&self, id: SubagentId, status: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(metadata) = state.get_mut(&id) {
            metadata.last_status_message = Some(status.into());
        }
    }

    fn finish(&self, id: SubagentId) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.remove(&id);
    }

    #[allow(dead_code)]
    fn snapshot(&self) -> Vec<SubagentMetadata> {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.values().cloned().collect()
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

fn spawn_observed_job<F>(
    jobs: JobRegistry,
    job_id: JobId,
    done: Arc<Notify>,
    future: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(future).catch_unwind().await;
        if outcome.is_err() {
            let _ = jobs.finish_if_active(job_id, JobStatus::Failed, "job panicked", None);
        }
        done.notify_waiters();
    })
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
    ai_reviewer_state: Arc<StdMutex<ai_reviewer::AiReviewerState>>,
    next_turn_id: Arc<AtomicU64>,
    next_approval_id: Arc<AtomicU64>,
    next_attachment_id: Arc<AtomicU64>,
    subagents: SubagentRegistry,
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
    /// Optional registry of lifecycle hook handlers. Skills and other
    /// extensions register here; the per-turn LLM call site dispatches
    /// `HookEvent::PreTurn` before issuing the request when this is
    /// `Some`. Defaults to `None` for backwards compatibility — callers
    /// that need hooks install a registry via `set_hooks`.
    hooks: Option<Arc<HookRegistry>>,
    /// Config save queued from the config screen. Drained before each
    /// `start_turn` so the running turn (if any) finishes on the old config
    /// and the next turn picks up the new one.
    pending_swap: Option<PendingConfigSwap>,
}

/// A configuration change that has been written to disk but is waiting for
/// the next turn boundary to take effect. The provider is optional because
/// many fields (verbosity, permissions, telemetry endpoint) reuse the
/// existing LLM client.
#[derive(Clone)]
pub struct PendingConfigSwap {
    pub config: AppConfig,
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub display_note: Option<String>,
}

impl Agent {
    pub fn new(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Self {
        let session_log = start_session_log(&config, provider.name());
        // Fresh sessions inherit the most-recent cross-session calibration so
        // the first round's estimator isn't stuck on per-provider defaults.
        // Missing or malformed files fall back to `TokenCalibration::default()`,
        // which is what `ConversationState::default()` would carry anyway.
        let conversation_state = ConversationState {
            token_calibration: SessionStore::open(&config).load_global_calibration(),
            ..ConversationState::default()
        };
        Self::build(config, provider, session_log, conversation_state, None)
    }

    pub fn resume(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_id: &str,
    ) -> squeezy_core::Result<(Self, Vec<TranscriptItem>)> {
        let store = SessionStore::open(&config);
        let handle = store.open_session(session_id.to_string());
        // Prefer the durable snapshot, but fall back to replaying
        // events.jsonl when resume_state.json is missing, corrupt, or
        // marks the session non-resumable. The event log is appended on
        // every turn, so it survives a crash that ate the snapshot.
        let resume_state = match handle.read_resume_state() {
            Ok(state) if state.resume_available => state,
            _ => match handle.replay_resume_state() {
                Ok(state) => state,
                Err(_) => {
                    return Err(SqueezyError::Agent(format!(
                        "session {session_id} is not resumable"
                    )));
                }
            },
        };
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
        let _ = handle.append_typed_event(
            SessionEventKind::SessionResumed,
            None,
            Some("session resumed".to_string()),
        );
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
        // Replay must produce byte-identical model requests against the
        // recorded tape. Workspace-specific ingestion (AGENTS.md and
        // user memory) would change `config.instructions` based on the
        // host environment, breaking the hash check. Disable it here.
        config.context_compaction.repo_doc_max_bytes = 0;
        config.context_compaction.user_memory_max_bytes = 0;
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
        if let Some(store) = store.as_deref() {
            let now: u128 = unix_timestamp_millis() as u128;
            let ttl_ms: u128 = (squeezy_store::DEFAULT_COMPACTION_CHECKPOINT_RETENTION_DAYS
                as u128)
                * 24
                * 60
                * 60
                * 1_000;
            let cutoff = now.saturating_sub(ttl_ms);
            if let Err(err) = store.prune_compaction_checkpoints(cutoff) {
                tracing::warn!(
                    target: "squeezy::store",
                    error = %err,
                    "failed to prune compaction_checkpoints; old entries may persist",
                );
            }
        }
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
        if let Some(repo_doc) = ingest_agents_md(
            &config.workspace_root,
            config.context_compaction.repo_doc_max_bytes,
        ) {
            log_session_event(
                session_log.as_ref(),
                &redactor,
                "agents_md_ingested",
                None,
                Some(format!("{} bytes ingested from AGENTS.md", repo_doc.len())),
                json!({ "bytes": repo_doc.len() }),
            );
            config.instructions = format!(
                "{}\n\nProject conventions from AGENTS.md:\n{}",
                config.instructions, repo_doc
            );
        }
        if let Some(user_memory) =
            ingest_user_memory(config.context_compaction.user_memory_max_bytes)
        {
            log_session_event(
                session_log.as_ref(),
                &redactor,
                "user_memory_ingested",
                None,
                Some(format!(
                    "{} bytes ingested from ~/.squeezy/MEMORY.md",
                    user_memory.len()
                )),
                json!({ "bytes": user_memory.len() }),
            );
            config.instructions = format!(
                "{}\n\nUser-level memory (~/.squeezy/MEMORY.md):\n{}",
                config.instructions, user_memory
            );
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
            ai_reviewer_state: Arc::new(StdMutex::new(ai_reviewer::AiReviewerState::default())),
            next_turn_id: Arc::new(AtomicU64::new(1)),
            next_approval_id: Arc::new(AtomicU64::new(1)),
            next_attachment_id: Arc::new(AtomicU64::new(next_attachment_id)),
            subagents: SubagentRegistry::default(),
            session_rules: Arc::new(RwLock::new(Vec::new())),
            session_mode: Arc::new(AtomicU8::new(initial_session_mode.to_u8())),
            loaded_tool_schemas: Arc::new(Mutex::new(Vec::new())),
            store,
            replay,
            hooks: None,
            pending_swap: None,
        }
    }

    /// Borrow the current effective config.
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Clone the current effective config — used by the config screen to
    /// initialize its editing buffer.
    pub fn config_snapshot(&self) -> AppConfig {
        self.config.clone()
    }

    /// Replace the in-process config immediately. Use for Immediate-tier
    /// saves: verbosity, permissions, telemetry on/off — fields that are
    /// consulted fresh on each operation. Fields baked into derived state at
    /// build time (tools/MCP/redactor) are NOT rebuilt; pair this with the
    /// "restart required" badge in the UI for those.
    pub fn replace_config(&mut self, next: AppConfig) {
        self.config = next;
    }

    /// Replace the LLM client. The in-flight turn (if any) holds a clone of
    /// the old `Arc` so it finishes against the old client; subsequent turns
    /// pick up the new one.
    pub fn replace_provider(&mut self, next: Arc<dyn LlmProvider>, model: String) {
        self.provider = next;
        self.config.model = model;
    }

    /// Queue a NextPrompt-tier swap. Drained by `drain_pending_swap()` at the
    /// top of the next user turn so the running turn is undisturbed.
    pub fn arm_config_swap(&mut self, swap: PendingConfigSwap) {
        self.pending_swap = Some(swap);
    }

    pub fn pending_config_swap(&self) -> Option<&PendingConfigSwap> {
        self.pending_swap.as_ref()
    }

    /// Apply the queued swap (if any) and return it for telemetry / display.
    /// Call this from the TUI immediately before `start_turn()` so the new
    /// config takes effect for the very next request.
    pub fn drain_pending_swap(&mut self) -> Option<PendingConfigSwap> {
        let swap = self.pending_swap.take()?;
        self.config = swap.config.clone();
        if let Some(provider) = swap.provider.clone() {
            self.provider = provider;
        }
        Some(swap)
    }

    /// Install a hook registry. Handlers registered here observe
    /// `HookEvent::PreTurn` before each turn's LLM request and
    /// `HookEvent::{PreCompact, PostCompact}` around each compaction
    /// pass (pre- and mid-turn). Remaining variants are reserved
    /// (variant-only) for follow-up wiring. Passing `None` clears any
    /// previously-installed registry. Wrapped in `Arc` so cloned
    /// `TurnRuntime`s share the same handler set without paying for
    /// re-registration on every turn.
    pub fn set_hooks(&mut self, hooks: Option<Arc<HookRegistry>>) {
        self.hooks = hooks;
    }

    /// Borrow the currently-installed hook registry, if any. Returns
    /// `None` when hooks are disabled (default) so the caller can skip
    /// dispatch entirely.
    pub fn hooks(&self) -> Option<&Arc<HookRegistry>> {
        self.hooks.as_ref()
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

    pub fn reviewer_audit_snapshot(&self) -> Vec<ReviewerAuditEntry> {
        self.ai_reviewer_state
            .lock()
            .map(|guard| guard.recent_decisions())
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
        let done = Arc::new(Notify::new());
        let handle = spawn_observed_job(self.jobs.clone(), job_id, done.clone(), async move {
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
        self.jobs.attach_handle(job_id, handle.abort_handle(), done);
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

    fn session_prompt_cache_key(&self) -> Option<String> {
        self.session_id().map(|id| format!("squeezy::{id}"))
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
            transmitted_request: estimate_request_context_calibrated(
                self.provider.name(),
                &self.config.model,
                &transmitted_request,
                context_window_override,
                Some(&state.token_calibration),
            ),
            full_history_request: estimate_request_context_calibrated(
                self.provider.name(),
                &self.config.model,
                &full_history_request,
                context_window_override,
                Some(&state.token_calibration),
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
        let mut all_tool_specs = core_control_tools(&self.config.subagents, mode);
        all_tool_specs.extend(self.tools.specs().iter().cloned().map(advertised_tool));
        let session_id_for_plan_mode = self.session_id();
        let plan_edit_allowed = plan_mode::plan_edit_allowed_in_workspace(
            mode,
            &self.config.workspace_root,
            session_id_for_plan_mode.as_deref(),
        );
        LlmRequest {
            model: Arc::from(self.config.model.as_str()),
            instructions: Arc::from(instructions_with_tool_index(
                &request_instructions,
                &all_tool_specs,
                mode,
                &self.config.tools,
                plan_edit_allowed,
            )),
            input: Arc::from(input),
            max_output_tokens: self.config.max_output_tokens,
            response_verbosity: request_response_verbosity(&self.config, self.provider.name()),
            reasoning_effort: request_reasoning_effort(&self.config, self.provider.name()),
            previous_response_id: if include_response_state {
                previous_response_id
            } else {
                None
            },
            cache_key: self.session_prompt_cache_key(),
            tools: Arc::from(request_tool_specs(
                &all_tool_specs,
                mode,
                &self.config.tools,
                loaded_tool_schemas,
                plan_edit_allowed,
            )),
            store,
            output_schema: None,
        }
    }

    pub fn list_sessions(
        &self,
        query: &SessionQuery,
    ) -> squeezy_core::Result<Vec<SessionMetadata>> {
        self.flush_active_session_log();
        SessionStore::open(&self.config).list(query)
    }

    pub fn show_session(&self, session_id: &str) -> squeezy_core::Result<SessionRecord> {
        self.flush_session_log_if_current(session_id);
        SessionStore::open(&self.config).show(session_id)
    }

    pub fn export_session(&self, session_id: &str) -> squeezy_core::Result<Value> {
        self.flush_session_log_if_current(session_id);
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

    fn flush_active_session_log(&self) {
        if let Some(session) = &self.session_log {
            let _ = session.flush_events();
        }
    }

    fn flush_session_log_if_current(&self, session_id: &str) {
        if let Some(session) = &self.session_log
            && session.session_id() == session_id
        {
            let _ = session.flush_events();
        }
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
        let report = compact_conversation_with_strategy(
            &mut conversation,
            &mut context_compaction,
            &attachments,
            self.store.as_deref(),
            &self.provider,
            self.session_log.as_ref(),
            &self.redactor,
            &self.config,
            ContextCompactionTrigger::Manual,
            true,
        )
        .await
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

    /// Dispatch an agent-side slash command (e.g. from a non-TUI
    /// driver such as `squeezy-eval`). Only commands whose behavior
    /// lives wholly inside `Agent` are handled here — TUI-only
    /// commands (overlays, help text) return `Unsupported`.
    ///
    /// The structured outcome is suitable for embedding in JSON
    /// transcripts; the TUI is free to render its own variant on top.
    pub async fn dispatch_command(&self, name: &str, _args: &str) -> CommandOutcome {
        let normalized = name.trim().trim_start_matches('/').to_ascii_lowercase();
        match normalized.as_str() {
            "compact" => match self.compact_context_manual().await {
                Ok(_) => CommandOutcome::Compacted,
                Err(err) => CommandOutcome::Error {
                    command: normalized,
                    message: format!("{err}"),
                },
            },
            "plan" => {
                let changed = self.set_session_mode(SessionMode::Plan, "dispatch_command");
                CommandOutcome::ModeChanged {
                    mode: "plan".into(),
                    changed,
                }
            }
            "build" => {
                let changed = self.set_session_mode(SessionMode::Build, "dispatch_command");
                CommandOutcome::ModeChanged {
                    mode: "build".into(),
                    changed,
                }
            }
            "cost" => {
                let snap = self.session_accounting_snapshot().await;
                CommandOutcome::CostSnapshot {
                    debug: format!("{:?}", snap),
                }
            }
            "jobs" => {
                let jobs = self.jobs_snapshot();
                CommandOutcome::JobsList { count: jobs.len() }
            }
            "permissions" => {
                let rules = self.session_rules_snapshot();
                CommandOutcome::PermissionsList { count: rules.len() }
            }
            other => CommandOutcome::Unsupported {
                command: other.to_string(),
            },
        }
    }

    /// Append an extra user message to the conversation transcript
    /// without starting a new turn. Use to script "interrupting user"
    /// behavior from drivers like `squeezy-eval`.
    pub async fn queue_user_message(&self, text: String) {
        let mut state = self.conversation_state.lock().await;
        state.conversation.push(LlmInputItem::UserText(text));
    }

    /// Restore the most recent compaction checkpoint, undoing the last
    /// `compact_context_manual` (or auto-compaction). Returns the restored
    /// record on success, or `Ok(None)` when there is nothing to undo
    /// (no compaction history, or the checkpoint expired / was never
    /// persisted because the agent had no store handle).
    pub async fn compact_context_undo(
        &self,
    ) -> squeezy_core::Result<Option<ContextCompactionRecord>> {
        let mut state = self.conversation_state.lock().await;
        let Some(last) = state.context_compaction.last.clone() else {
            return Ok(None);
        };
        let Some(replacement_id) = last.replacement_id.clone() else {
            return Ok(None);
        };
        let Some(store) = self.store.as_deref() else {
            return Ok(None);
        };
        let Some(checkpoint) = store.get_compaction_checkpoint(&replacement_id)? else {
            return Ok(None);
        };
        // The synthetic summary head occupies index 0 of `conversation`.
        // Drop it and prepend the restored items so the conversation now
        // matches the pre-compaction shape (plus any items added after
        // the compaction event, which stay verbatim).
        if !matches!(state.conversation.first(), Some(LlmInputItem::UserText(_))) {
            return Err(SqueezyError::Agent(
                "cannot undo compaction: conversation head is not a synthetic summary".to_string(),
            ));
        }
        let mut restored: Vec<LlmInputItem> = checkpoint
            .items
            .into_iter()
            .map(resume_item_to_llm_input)
            .collect();
        let tail = state.conversation.split_off(1);
        restored.extend(tail);
        state.conversation = restored;
        state.context_compaction.generation = state.context_compaction.generation.saturating_sub(1);
        state.context_compaction.history.pop();
        state.context_compaction.last = state.context_compaction.history.last().cloned();
        state.context_compaction.summary = state
            .context_compaction
            .last
            .as_ref()
            .and_then(|_| state.context_compaction.summary.clone());
        state.previous_response_id = None;
        if let Some(session) = &self.session_log {
            session.write_resume_state(&state.to_resume_state())?;
        }
        drop(state);
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "context_compaction_undone",
            None,
            Some(format!(
                "undid compaction gen={} via {}",
                last.generation, replacement_id,
            )),
            json!({ "record": last.clone(), "replacement_id": replacement_id }),
        );
        Ok(Some(last))
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
                "replacement_id": report.record.replacement_id,
                "conversation": report.dropped,
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
        let ai_reviewer_state = self.ai_reviewer_state.clone();
        let store = self.store.clone();
        let task_state = Arc::new(Mutex::new(None));
        let loaded_tool_schemas = self.loaded_tool_schemas.clone();
        let replay = self.replay.clone();
        let subagents = self.subagents.clone();
        let hooks = self.hooks.clone();

        let turn_done = Arc::new(Notify::new());
        let panic_tx = tx.clone();
        let panic_session_log = session_log.clone();
        let panic_redactor = redactor.clone();
        let panic_telemetry = telemetry.clone();
        let monitor_tx = tx.clone();
        let monitor_session_log = session_log.clone();
        let monitor_redactor = redactor.clone();
        let monitor_cancel = cancel.clone();
        let turn_handle = spawn_observed_turn(
            turn_id,
            turn_done.clone(),
            panic_tx,
            panic_session_log,
            panic_redactor,
            panic_telemetry,
            async move {
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
                            ai_reviewer_state: ai_reviewer_state.clone(),
                            loaded_tool_schemas: loaded_tool_schemas.clone(),
                            subagents: subagents.clone(),
                            hooks: hooks.clone(),
                        },
                    )
                    .await;
                    return;
                }
                // Cheap pre-check first so unrelated coding turns do not pay for a
                // full `inspect_redacted()` rendering on every turn.
                if matches_squeezy_help_input(&task_title) {
                    let outcome = resolve_help_turn(
                        &task_title,
                        &HelpResolutionDeps {
                            provider: provider.clone(),
                            tools: tools.clone(),
                            telemetry: telemetry.clone(),
                            config: config.clone(),
                            redactor: redactor.clone(),
                            cancel: cancel.clone(),
                            approval_ids: approval_ids.clone(),
                            session_rules: session_rules.clone(),
                            ai_reviewer_state: ai_reviewer_state.clone(),
                            session_mode: session_mode.clone(),
                            subagents: subagents.clone(),
                            hooks: hooks.clone(),
                        },
                    )
                    .await;
                    complete_squeezy_help_turn(
                        turn_id,
                        task_title,
                        outcome,
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
                let mut all_tool_specs =
                    core_control_tools(&config.subagents, load_session_mode(&session_mode));
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
                    ai_reviewer_state,
                    session_mode,
                    session_log,
                    conversation_state,
                    store,
                    task_state: task_state.clone(),
                    loaded_tool_schemas,
                    replay,
                    subagents,
                    hooks,
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
            },
        );
        spawn_turn_cancel_monitor(
            turn_id,
            monitor_cancel,
            turn_done,
            turn_handle.abort_handle(),
            monitor_tx.downgrade(),
            monitor_session_log,
            monitor_redactor,
        );

        rx
    }
}

fn spawn_observed_turn<F>(
    turn_id: TurnId,
    done: Arc<Notify>,
    tx: mpsc::Sender<AgentEvent>,
    session_log: Option<SessionHandle>,
    redactor: Arc<Redactor>,
    telemetry: TelemetryClient,
    future: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(future).catch_unwind().await;
        if outcome.is_err() {
            let error = SqueezyError::Agent("agent turn panicked".to_string());
            log_session_event(
                session_log.as_ref(),
                &redactor,
                "failed",
                Some(turn_id),
                Some(error.to_string()),
                json!({ "error": error.to_string(), "panic": true }),
            );
            if let Some(session) = &session_log {
                let _ = session.update_metadata(|metadata| {
                    metadata.status = SessionStatus::Failed;
                    metadata.latest_summary = Some(error.to_string());
                });
            }
            telemetry.spawn(TelemetryEvent::failure_seen(error_kind(&error)));
            let _ = tx.send(AgentEvent::Failed { turn_id, error }).await;
        }
        done.notify_waiters();
    })
}

fn spawn_turn_cancel_monitor(
    turn_id: TurnId,
    cancel: CancellationToken,
    done: Arc<Notify>,
    abort: AbortHandle,
    tx: mpsc::WeakSender<AgentEvent>,
    session_log: Option<SessionHandle>,
    redactor: Arc<Redactor>,
) {
    tokio::spawn(async move {
        cancel.cancelled().await;
        tokio::select! {
            _ = done.notified() => {}
            _ = tokio::time::sleep(JOB_CANCEL_GRACE) => {
                abort.abort();
                log_session_event(
                    session_log.as_ref(),
                    &redactor,
                    "cancelled",
                    Some(turn_id),
                    Some("turn cancelled after grace window".to_string()),
                    json!({ "reason": "cancelled after grace window" }),
                );
                if let Some(session) = &session_log {
                    let _ = session.update_metadata(|metadata| {
                        metadata.status = SessionStatus::Cancelled;
                        metadata.latest_summary =
                            Some("turn cancelled after grace window".to_string());
                    });
                }
                if let Some(tx) = tx.upgrade() {
                    let _ = tx.send(AgentEvent::Cancelled { turn_id }).await;
                }
                done.notify_waiters();
            }
        }
    });
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

#[derive(Debug, Clone)]
struct HelpTurnOutcome {
    answer: HelpAnswer,
    metrics: TurnMetrics,
    cost: CostSnapshot,
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
    ai_reviewer_state: Arc<StdMutex<ai_reviewer::AiReviewerState>>,
    loaded_tool_schemas: Arc<Mutex<Vec<String>>>,
    subagents: SubagentRegistry,
    hooks: Option<Arc<HookRegistry>>,
}

async fn resolve_help_turn(task_title: &str, deps: &HelpResolutionDeps) -> HelpTurnOutcome {
    let config_inspect = deps.config.inspect_redacted();
    let curated = SqueezyHelp::new(config_inspect).answer_for_input(task_title);

    // Curated topics always beat the subagent: they have hand-written summaries,
    // citation paths, and extracted config sections that the model can only
    // approximate. We only escalate to the subagent when the curated layer
    // returns `Unsupported` (or returns nothing for a borderline question).
    if let Some(answer) = curated.as_ref()
        && answer.status == HelpStatus::Answered
    {
        return HelpTurnOutcome {
            answer: answer.clone(),
            metrics: TurnMetrics::default(),
            cost: CostSnapshot::default(),
        };
    }

    let subagent = run_doc_help_subagent(task_title, deps).await;

    if let Some(answer) = subagent.answer {
        return HelpTurnOutcome {
            answer,
            metrics: subagent.metrics,
            cost: subagent.cost,
        };
    }

    let answer =
        curated.unwrap_or_else(|| SqueezyHelp::new(deps.config.inspect_redacted()).topic_index());
    HelpTurnOutcome {
        answer,
        metrics: subagent.metrics,
        cost: subagent.cost,
    }
}

struct DocHelpResolution {
    answer: Option<HelpAnswer>,
    metrics: TurnMetrics,
    cost: CostSnapshot,
}

impl DocHelpResolution {
    fn skipped() -> Self {
        Self {
            answer: None,
            metrics: TurnMetrics::default(),
            cost: CostSnapshot::default(),
        }
    }
}

struct HelpResolutionDeps {
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    telemetry: TelemetryClient,
    config: AppConfig,
    redactor: Arc<Redactor>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    ai_reviewer_state: Arc<StdMutex<ai_reviewer::AiReviewerState>>,
    session_mode: Arc<AtomicU8>,
    subagents: SubagentRegistry,
    hooks: Option<Arc<HookRegistry>>,
}

async fn run_doc_help_subagent(task_title: &str, deps: &HelpResolutionDeps) -> DocHelpResolution {
    if !deps.config.subagents.enabled {
        return DocHelpResolution::skipped();
    }
    let config_inspect = deps.config.inspect_redacted();
    let prompt = doc_help_subagent_prompt(task_title, &config_inspect, &bundled_docs());
    let request = SubagentRequest {
        prompt,
        scope: Some(
            "Inlined bundled docs (originally under docs/external) and the inlined redacted config inspect output."
                .to_string(),
        ),
        thoroughness: None,
    };
    let mut all_tool_specs = core_control_tools(
        &deps.config.subagents,
        load_session_mode(&deps.session_mode),
    );
    all_tool_specs.extend(deps.tools.specs().iter().cloned().map(advertised_tool));
    let jobs = JobRegistry::new();
    let parent = ToolExecutionContext {
        turn_id: TurnId::new(0),
        origin: ToolOrigin::Subagent,
        provider: deps.provider.clone(),
        tools: &deps.tools,
        jobs: &jobs,
        config: &deps.config,
        telemetry: deps.telemetry.clone(),
        redactor: deps.redactor.clone(),
        tx: mpsc::channel(1).0,
        cancel: deps.cancel.clone(),
        approval_ids: deps.approval_ids.clone(),
        session_rules: deps.session_rules.clone(),
        ai_reviewer_state: deps.ai_reviewer_state.clone(),
        session_mode: deps.session_mode.clone(),
        session_log: None,
        conversation_state: None,
        task_state: Arc::new(Mutex::new(None)),
        all_tool_specs: &all_tool_specs,
        loaded_tool_schemas: Arc::new(Mutex::new(Vec::new())),
        exploration_state: Arc::new(Mutex::new(ExplorationTurnState::from_plan(None))),
        subagents: deps.subagents.clone(),
        hooks: deps.hooks.clone(),
    };
    let execution = run_subagent(&parent, SubagentKind::DocHelp, request).await;

    let mut metrics = TurnMetrics::default();
    metrics.merge_subagent_tool_metrics(&execution.metrics);
    metrics.subagent_calls = 1;
    if execution.status != ToolStatus::Success {
        metrics.subagent_failures = 1;
    }
    let cost = execution.metrics.provider.clone();

    let answer = if execution.status == ToolStatus::Success && !execution.summary.trim().is_empty()
    {
        Some(HelpAnswer {
            topic: "doc-help".to_string(),
            status: HelpStatus::Answered,
            body: execution.summary,
            citations: Vec::new(),
            config_sections: Vec::new(),
        })
    } else {
        None
    };

    DocHelpResolution {
        answer,
        metrics,
        cost,
    }
}

fn doc_help_subagent_prompt(task_title: &str, config_inspect: &str, docs: &[BundledDoc]) -> String {
    // Inlining the bundled docs is what makes this subagent actually work at
    // runtime: end users run Squeezy outside the source tree, so docs/external
    // does not exist on disk for filesystem tools to find. The doc corpus is
    // ~120KB total; that is acceptable for a help turn the user explicitly
    // invoked.
    let mut prompt = String::with_capacity(config_inspect.len() + 4096 + docs_total_len(docs));
    prompt.push_str("User help request:\n");
    prompt.push_str(task_title.trim());
    prompt.push_str("\n\nRedacted config inspect:\n```toml\n");
    prompt.push_str(config_inspect.trim());
    prompt.push_str("\n```\n\nBundled docs corpus (each section is the full content of one bundled doc; cite by the listed path):\n");
    for doc in docs {
        prompt.push_str("\n---\nPATH: ");
        prompt.push_str(doc.path);
        prompt.push_str("\n\n");
        prompt.push_str(doc.content.trim_end());
        prompt.push('\n');
    }
    prompt
}

fn docs_total_len(docs: &[BundledDoc]) -> usize {
    docs.iter()
        .map(|doc| doc.content.len() + doc.path.len() + 16)
        .sum()
}

async fn complete_squeezy_help_turn(
    turn_id: TurnId,
    task_title: String,
    outcome: HelpTurnOutcome,
    seed_redactions: u64,
    deps: HelpTurnDeps,
) {
    let HelpTurnOutcome {
        answer,
        mut metrics,
        cost,
    } = outcome;
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
    metrics.redactions += seed_redactions + rendered.redactions;

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
            Some("Squeezy help".to_string()),
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
    let context_estimate = {
        let state = conversation_state.lock().await;
        estimate_context(&state.conversation)
    };
    let _ = tx
        .send(AgentEvent::Completed {
            turn_id,
            message,
            response_id: None,
            cost,
            metrics,
            context_estimate,
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
        ai_reviewer_state,
        loaded_tool_schemas,
        subagents,
        hooks,
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
            origin: ToolOrigin::Model,
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
            ai_reviewer_state,
            session_mode: session_mode.clone(),
            session_log: session_log.clone(),
            conversation_state: None,
            task_state: task_state.clone(),
            all_tool_specs: &all_tool_specs,
            loaded_tool_schemas,
            exploration_state,
            subagents,
            hooks,
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
    let context_estimate = {
        let state = conversation_state.lock().await;
        estimate_context(&state.conversation)
    };
    let _ = tx
        .send(AgentEvent::Completed {
            turn_id,
            message,
            response_id: None,
            cost,
            metrics,
            context_estimate,
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
    ai_reviewer_state: Arc<StdMutex<ai_reviewer::AiReviewerState>>,
    session_mode: Arc<AtomicU8>,
    session_log: Option<SessionHandle>,
    conversation_state: Arc<Mutex<ConversationState>>,
    store: Option<Arc<SqueezyStore>>,
    task_state: Arc<Mutex<Option<TaskStateSnapshot>>>,
    loaded_tool_schemas: Arc<Mutex<Vec<String>>>,
    replay: Option<Arc<ReplayRuntime>>,
    subagents: SubagentRegistry,
    /// Hook registry shared with `Agent`. `None` when no hooks are
    /// installed — the per-round LLM call site checks this before
    /// building a `HookContext`.
    hooks: Option<Arc<HookRegistry>>,
}

impl TurnRuntime {
    /// Session id derived from the session log handle, used by plan-mode
    /// path-scoped write exception (issue 17). `None` when the session
    /// has not yet been assigned an id (pre-first-turn window) or has
    /// no log handle (replay/test scenarios).
    fn session_id(&self) -> Option<String> {
        self.session_log
            .as_ref()
            .map(|handle| handle.session_id().to_string())
    }
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
/// Walk from `cwd` up to the nearest `.git` directory and concatenate every
/// `AGENTS.md` found from root downward, capped at `max_bytes` of UTF-8.
/// Returns `None` when ingestion is disabled (`max_bytes == 0`), no
/// `AGENTS.md` exists in the walked range, or every read fails.
fn ingest_agents_md(cwd: &std::path::Path, max_bytes: usize) -> Option<String> {
    if max_bytes == 0 {
        return None;
    }
    let canonical_cwd = fs::canonicalize(cwd)
        .ok()
        .unwrap_or_else(|| cwd.to_path_buf());
    let mut root: Option<std::path::PathBuf> = None;
    for ancestor in canonical_cwd.ancestors() {
        if ancestor.join(".git").exists() {
            root = Some(ancestor.to_path_buf());
            break;
        }
    }
    let root = root.unwrap_or_else(|| canonical_cwd.clone());
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut current = canonical_cwd.as_path();
    loop {
        dirs.push(current.to_path_buf());
        if current == root {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    dirs.reverse(); // root-first
    let mut combined = String::new();
    let mut remaining = max_bytes;
    for dir in dirs {
        let candidate = dir.join("AGENTS.md");
        let Ok(body) = fs::read_to_string(&candidate) else {
            continue;
        };
        if body.is_empty() {
            continue;
        }
        let header = format!("--- {} ---\n", candidate.display());
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        let header_bytes = header.len().min(remaining);
        combined.push_str(&header[..header_bytes]);
        remaining = remaining.saturating_sub(header_bytes);
        if remaining == 0 {
            combined.push_str("[truncated]");
            break;
        }
        let take = body.len().min(remaining);
        let mut end = take;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        combined.push_str(&body[..end]);
        remaining = remaining.saturating_sub(end);
        if body.len() > end {
            combined.push_str("\n[truncated]");
            break;
        }
    }
    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}

/// Read `~/.squeezy/MEMORY.md` (preferred) or `~/.squeezy/memory.md` and
/// return its contents truncated to `max_bytes`. Returns `None` when
/// ingestion is disabled, `HOME` is unset, or neither file is present /
/// readable. Errors are silent on purpose: this is a best-effort enrichment,
/// never load-bearing. Uppercase first mirrors the project's `AGENTS.md`
/// casing so users converging on the canonical name see it picked up.
fn ingest_user_memory(max_bytes: usize) -> Option<String> {
    if max_bytes == 0 {
        return None;
    }
    let home = env::var_os("HOME")?;
    let dir = std::path::PathBuf::from(home).join(".squeezy");
    let body = fs::read_to_string(dir.join("MEMORY.md"))
        .or_else(|_| fs::read_to_string(dir.join("memory.md")))
        .ok()?;
    if body.is_empty() {
        return None;
    }
    if body.len() <= max_bytes {
        return Some(body);
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::with_capacity(end + 16);
    truncated.push_str(&body[..end]);
    truncated.push_str("\n[truncated]");
    Some(truncated)
}

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
    fn session_prompt_cache_key(&self) -> Option<String> {
        self.session_log
            .as_ref()
            .map(|handle| format!("squeezy::{}", handle.session_id()))
    }

    /// Fan out a `HookEvent::PreTurn` to every registered handler when
    /// a hook registry is installed. Mutation replies are logged but
    /// not yet applied — that wiring is deferred to a follow-up
    /// commit so this first hooks foundation stays minimal and
    /// strictly observational. Returns immediately when no registry
    /// is configured so the no-hooks path costs zero allocations.
    fn dispatch_pre_turn(&self) {
        let Some(registry) = self.hooks.as_ref() else {
            return;
        };
        if registry.is_empty() {
            return;
        }
        let payload = json!({ "turn_index": self.turn_id.to_string() });
        let results = registry.dispatch(HookEvent::PreTurn, payload);
        for (idx, result) in results.iter().enumerate() {
            if let Some(mutate) = result.mutate.as_ref() {
                tracing::debug!(
                    target: "squeezy::hooks",
                    turn_id = %self.turn_id,
                    handler_index = idx,
                    %mutate,
                    "PreTurn handler proposed a mutation (not yet applied)"
                );
            }
            if !result.allow {
                tracing::debug!(
                    target: "squeezy::hooks",
                    turn_id = %self.turn_id,
                    handler_index = idx,
                    message = result.message.as_deref().unwrap_or(""),
                    "PreTurn handler returned allow=false (not yet enforced)"
                );
            }
        }
    }

    /// Fan out a `HookEvent::PreCompact` to every registered handler
    /// when a hook registry is installed. `before_tokens` is the
    /// pre-compaction estimate so handlers can decide whether to log,
    /// veto (advisory today; not yet enforced), or react. The hook is
    /// skipped entirely when no registry is configured so the no-hooks
    /// path stays allocation-free.
    fn dispatch_pre_compact(&self, before_tokens: u64) {
        let Some(registry) = self.hooks.as_ref() else {
            return;
        };
        if registry.is_empty() {
            return;
        }
        let payload = json!({
            "turn_index": self.turn_id.to_string(),
            "before_tokens": before_tokens,
        });
        let results = registry.dispatch(HookEvent::PreCompact, payload);
        for (idx, result) in results.iter().enumerate() {
            if let Some(mutate) = result.mutate.as_ref() {
                tracing::debug!(
                    target: "squeezy::hooks",
                    turn_id = %self.turn_id,
                    handler_index = idx,
                    %mutate,
                    "PreCompact handler proposed a mutation (not yet applied)"
                );
            }
            if !result.allow {
                tracing::debug!(
                    target: "squeezy::hooks",
                    turn_id = %self.turn_id,
                    handler_index = idx,
                    message = result.message.as_deref().unwrap_or(""),
                    "PreCompact handler returned allow=false (not yet enforced)"
                );
            }
        }
    }

    /// Fan out a `HookEvent::PostCompact` carrying the before/after
    /// token counts so handlers can observe how much the rewrite
    /// shrank the conversation. Mirrors `dispatch_pre_compact` in
    /// every other respect.
    fn dispatch_post_compact(&self, before_tokens: u64, after_tokens: u64) {
        let Some(registry) = self.hooks.as_ref() else {
            return;
        };
        if registry.is_empty() {
            return;
        }
        let payload = json!({
            "turn_index": self.turn_id.to_string(),
            "before_tokens": before_tokens,
            "after_tokens": after_tokens,
        });
        let results = registry.dispatch(HookEvent::PostCompact, payload);
        for (idx, result) in results.iter().enumerate() {
            if let Some(mutate) = result.mutate.as_ref() {
                tracing::debug!(
                    target: "squeezy::hooks",
                    turn_id = %self.turn_id,
                    handler_index = idx,
                    %mutate,
                    "PostCompact handler proposed a mutation (not yet applied)"
                );
            }
            if !result.allow {
                tracing::debug!(
                    target: "squeezy::hooks",
                    turn_id = %self.turn_id,
                    handler_index = idx,
                    message = result.message.as_deref().unwrap_or(""),
                    "PostCompact handler returned allow=false (not yet enforced)"
                );
            }
        }
    }

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
        // Plan mode is enforced by tool-filtering elsewhere; the overlay
        // here tells the model *why* its toolbox shrank and what the
        // expected output contract (`<proposed_plan>`) looks like.
        let active_mode = load_session_mode(&self.session_mode);
        let session_id_for_plan_mode = self.session_id();
        let mode_instructions = plan_mode::instructions_for_mode(
            &verbosity_instructions,
            active_mode,
            &self.config.workspace_root,
            session_id_for_plan_mode.as_deref(),
        );
        let mut prior_state = self.conversation_state.lock().await.clone();
        // Pinned context must reach the model on every turn, not only
        // after a compaction has occurred. Inline it into the per-turn
        // instructions so a `/pin` is immediately visible to the model
        // even on sessions that never cross the compaction threshold.
        let raw_instructions = instructions_with_pinned_context(
            &mode_instructions,
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
        // Redact at insertion time so the conversation upholds the
        // "already redacted" invariant. The per-round LLM request build
        // then sends `next_input` straight through without rebuilding
        // the vector via `redact_llm_input_items`.
        let user_item = redact_input_item(
            LlmInputItem::UserText(format_user_text_with_context(
                &activation.task_input,
                &active_attachments,
            )),
            &self.redactor,
        );
        // Upgrade any legacy conversation items resumed from disk so the
        // invariant holds for the rest of this turn. Idempotent and
        // cheap for items already in redacted form.
        let mut conversation =
            redact_llm_input_items(prior_state.conversation.clone(), &self.redactor);
        conversation.push(user_item.clone());
        let mut context_compaction = prior_state.context_compaction.clone();
        // PreCompact hook fires only when the auto trigger's
        // thresholds are crossed so handlers don't see a hook on every
        // turn — only when compaction will actually run. PostCompact
        // mirrors the report's before/after counts so observers can
        // measure the rewrite. The no-hook path stays allocation-free.
        let pre_compaction_estimate = estimate_context(&conversation);
        let compaction_likely = self.config.context_compaction.enabled
            && pre_compaction_estimate.items >= self.config.context_compaction.min_items
            && pre_compaction_estimate.estimated_tokens
                >= self.config.context_compaction.estimated_tokens;
        if compaction_likely {
            self.dispatch_pre_compact(pre_compaction_estimate.estimated_tokens);
        }
        if let Some(report) = maybe_compact_conversation(
            &mut conversation,
            &mut context_compaction,
            &active_attachments,
            self.store.as_deref(),
            &self.config,
            ContextCompactionTrigger::Auto,
        ) {
            self.dispatch_post_compact(
                report.record.before.estimated_tokens,
                report.record.after.estimated_tokens,
            );
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
                    "replacement_id": report.record.replacement_id,
                    "conversation": report.dropped,
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
        broker.seed_session(
            prior_state.cost.estimated_usd_micros.unwrap_or(0),
            prior_state.token_calibration.clone(),
        );
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
            self.finish_cancelled_turn(
                &task_title,
                &total_cost,
                &broker.metrics,
                &broker.calibration,
            )
            .await;
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
                        origin: ToolOrigin::Planner,
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
                        ai_reviewer_state: self.ai_reviewer_state.clone(),
                        session_mode: self.session_mode.clone(),
                        session_log: self.session_log.clone(),
                        conversation_state: Some(self.conversation_state.clone()),
                        task_state: self.task_state.clone(),
                        all_tool_specs: &self.all_tool_specs,
                        loaded_tool_schemas: self.loaded_tool_schemas.clone(),
                        exploration_state: exploration_state.clone(),
                        subagents: self.subagents.clone(),
                        hooks: self.hooks.clone(),
                    },
                    &mut broker,
                )
                .await
            };
            if self.cancel.is_cancelled() || results.iter().any(cancelled_tool_result) {
                self.finish_cancelled_turn(
                    &task_title,
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                )
                .await;
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
                    let output = self.redactor.redact(&pending.result.model_output()).text;
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
        // Per-turn cache of `<instructions> + <tool_index>` keyed by
        // session mode. `request_instructions`, `self.all_tool_specs`,
        // and `self.config.tools` are turn-stable; only `active_mode`
        // (which the TUI can flip mid-turn) varies, and the rare implicit
        // skill append below invalidates this on a revision boundary.
        let mut instructions_cache: [Option<String>; 2] = [None, None];
        // Fire the PreTurn hook once per user turn, immediately before
        // the first round's LLM request is built. Mutation replies are
        // currently observational only — see `dispatch_pre_turn` for
        // the rationale.
        self.dispatch_pre_turn();
        for _round in 0..MAX_TOOL_ROUNDS {
            if self.cancel.is_cancelled() {
                self.finish_cancelled_turn(
                    &task_title,
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                )
                .await;
                return Ok(());
            }
            if let Some(status) = broker.session_cap_reached() {
                self.publish_terminal_task_state(
                    TaskStateStatus::Failed,
                    Some(format_cap_reached_reason(status)),
                    &task_title,
                )
                .await;
                self.persist_turn_accounting(
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                    false,
                )
                .await;
                let _ = self
                    .tx
                    .send(AgentEvent::Failed {
                        turn_id: self.turn_id,
                        error: SqueezyError::Agent(format_cap_reached_reason(status)),
                    })
                    .await;
                self.finish_turn(&broker.metrics).await;
                return Ok(());
            }
            let active_mode = load_session_mode(&self.session_mode);
            let loaded_tool_schemas = self.loaded_tool_schemas.lock().await.clone();
            let plan_edit_allowed = plan_mode::plan_edit_allowed_in_workspace(
                active_mode,
                &self.config.workspace_root,
                self.session_id().as_deref(),
            );
            let mode_slot = active_mode as usize;
            if instructions_cache[mode_slot].is_none() {
                instructions_cache[mode_slot] = Some(instructions_with_tool_index(
                    &request_instructions,
                    &self.all_tool_specs,
                    active_mode,
                    &self.config.tools,
                    plan_edit_allowed,
                ));
            }
            let cached_instructions = instructions_cache[mode_slot]
                .as_ref()
                .expect("instructions cache populated above")
                .clone();
            let request = LlmRequest {
                model: Arc::from(self.config.model.as_str()),
                instructions: Arc::from(cached_instructions),
                input: Arc::from(next_input.as_slice()),
                max_output_tokens: self.config.max_output_tokens,
                response_verbosity: request_response_verbosity(&self.config, self.provider.name()),
                reasoning_effort: request_reasoning_effort(&self.config, self.provider.name()),
                previous_response_id: previous_response_id.clone(),
                cache_key: self.session_prompt_cache_key(),
                tools: Arc::from(request_tool_specs(
                    &self.all_tool_specs,
                    active_mode,
                    &self.config.tools,
                    &loaded_tool_schemas,
                    plan_edit_allowed,
                )),
                store: self.config.store_responses,
                output_schema: None,
            };
            let request_model = Arc::clone(&request.model);
            let request_input_bytes = llm_request_input_bytes(&request);
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
                    self.finish_cancelled_turn(
                        &task_title,
                        &total_cost,
                        &broker.metrics,
                        &broker.calibration,
                    )
                    .await;
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
                    LlmEvent::ReasoningDelta { text, .. } => {
                        if self
                            .tx
                            .send(AgentEvent::ReasoningDelta {
                                turn_id: self.turn_id,
                                delta: text,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    LlmEvent::ReasoningDone(payload) => {
                        let snapshot = ReasoningSnapshot::from_payload(payload.clone());
                        // Push the opaque blob into the conversation now so the
                        // model gets it back on every subsequent provider call
                        // in this turn (tool result → next model call → ...),
                        // not just at the end. Mirrors codex: each reasoning
                        // segment is committed when it closes.
                        conversation.push(redact_input_item(
                            LlmInputItem::Reasoning(payload),
                            &self.redactor,
                        ));
                        if self
                            .tx
                            .send(AgentEvent::ReasoningSegment {
                                turn_id: self.turn_id,
                                snapshot,
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
                        let warning = broker.record_provider_cost(&cost);
                        broker.calibration.record_sample(
                            self.provider.name(),
                            request_input_bytes,
                            cost.input_tokens.unwrap_or(0),
                        );
                        if let Some(status) = warning {
                            let _ = self
                                .tx
                                .send(AgentEvent::CostWarning {
                                    turn_id: self.turn_id,
                                    status,
                                })
                                .await;
                        }
                        merge_cost(&mut total_cost, &cost);
                        completed_cost = cost;
                        response_id = id;
                        completed = true;
                        break;
                    }
                    LlmEvent::Cancelled => {
                        self.finish_cancelled_turn(
                            &task_title,
                            &total_cost,
                            &broker.metrics,
                            &broker.calibration,
                        )
                        .await;
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
                let raw_assistant_text = std::mem::take(&mut assistant_message);
                // Reasoning blobs and segment events have already been pushed
                // by the `LlmEvent::ReasoningDone` arm above; only the
                // assistant text remains.
                //
                // Conversation state keeps the raw text (including any
                // `<proposed_plan>` block) so the model retains its own
                // prior plan when refining next turn. The displayed and
                // persisted transcript drops the block — the structured
                // Plan card is the canonical visualization.
                conversation.push(redact_input_item(
                    LlmInputItem::AssistantText(raw_assistant_text.clone()),
                    &self.redactor,
                ));
                let message = TranscriptItem::assistant(plan_mode::strip_proposed_plan_blocks(
                    &raw_assistant_text,
                ));
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
                    token_calibration: broker.calibration.clone(),
                })
                .await;
                let context_estimate = estimate_context(&conversation);
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message,
                        response_id: None,
                        cost: total_cost,
                        metrics: broker.metrics.clone(),
                        context_estimate,
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
                let raw_assistant_text = std::mem::take(&mut assistant_message);
                conversation.push(redact_input_item(
                    LlmInputItem::AssistantText(raw_assistant_text.clone()),
                    &self.redactor,
                ));
                let message = TranscriptItem::assistant(plan_mode::strip_proposed_plan_blocks(
                    &raw_assistant_text,
                ));
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
                    token_calibration: broker.calibration.clone(),
                })
                .await;
                let context_estimate = estimate_context(&conversation);
                let _ = self
                    .tx
                    .send(AgentEvent::Completed {
                        turn_id: self.turn_id,
                        message,
                        response_id,
                        cost: total_cost,
                        metrics: broker.metrics.clone(),
                        context_estimate,
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
                        origin: ToolOrigin::Model,
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
                        ai_reviewer_state: self.ai_reviewer_state.clone(),
                        session_mode: self.session_mode.clone(),
                        session_log: self.session_log.clone(),
                        conversation_state: Some(self.conversation_state.clone()),
                        task_state: self.task_state.clone(),
                        all_tool_specs: &self.all_tool_specs,
                        loaded_tool_schemas: self.loaded_tool_schemas.clone(),
                        exploration_state: exploration_state.clone(),
                        subagents: self.subagents.clone(),
                        hooks: self.hooks.clone(),
                    },
                    &mut broker,
                )
                .await
            };
            if self.cancel.is_cancelled() || results.iter().any(cancelled_tool_result) {
                self.finish_cancelled_turn(
                    &task_title,
                    &total_cost,
                    &broker.metrics,
                    &broker.calibration,
                )
                .await;
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
            if implicit_instructions_added {
                instructions_cache = [None, None];
            }
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
                    let output = self.redactor.redact(&pending.result.model_output()).text;
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

            // Mid-turn compaction (F75): if the provider reported usage
            // crossing the configured fraction of `model_context_window`,
            // shrink the conversation before the next sample. Bumps the
            // compaction generation, which forces previous_response_id
            // off the next request to keep the provider state consistent
            // with the new history.
            //
            // The PreCompact / PostCompact hook fan-out mirrors the
            // pre-turn path: PreCompact fires only when the mid-turn
            // gate will trip; PostCompact carries the report's
            // before/after counts when the rewrite landed.
            let mid_turn_observed_tokens = total_tokens_from_cost(&completed_cost);
            let mid_turn_compaction_likely = mid_turn_compaction_will_fire(
                &self.config,
                &conversation,
                mid_turn_observed_tokens,
            );
            if mid_turn_compaction_likely {
                let pre_estimate = mid_turn_observed_tokens.unwrap_or_else(|| {
                    estimate_context(&conversation).estimated_tokens
                });
                self.dispatch_pre_compact(pre_estimate);
            }
            let mid_turn_report = maybe_compact_mid_turn(
                &mut conversation,
                &mut context_compaction,
                &active_attachments,
                self.store.as_deref(),
                &self.config,
                mid_turn_observed_tokens,
            );
            let mid_turn_compacted = mid_turn_report.is_some();
            if let Some(report) = mid_turn_report {
                self.dispatch_post_compact(
                    report.record.before.estimated_tokens,
                    report.record.after.estimated_tokens,
                );
                self.log_event(
                    "context_compacted",
                    Some(self.turn_id),
                    Some(format!(
                        "mid-turn compacted gen={} {}->{} estimated tokens",
                        report.record.generation,
                        report.record.before.estimated_tokens,
                        report.record.after.estimated_tokens,
                    )),
                    json!({
                        "record": report.record,
                        "summary": report.summary,
                        "replacement_id": report.record.replacement_id,
                        "conversation": report.dropped,
                        "phase": "mid_turn",
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

            if self.config.store_responses {
                previous_response_id = if implicit_instructions_added || mid_turn_compacted {
                    None
                } else {
                    response_id
                };
                next_input = if mid_turn_compacted {
                    conversation.clone()
                } else {
                    outputs
                };
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
            token_calibration,
        } = input;
        {
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
        }
        self.persist_turn_accounting(cost, metrics, &token_calibration, true)
            .await;
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

    /// Merge per-turn cost/metrics/redactions and the latest token
    /// calibration into the conversation state and mirror them into the
    /// session metadata file.
    ///
    /// `mark_resume_available` is only `true` on the success path: a
    /// cancelled or failed turn must not flip the resume flag, since the
    /// conversation slice was not advanced. `previous_response_id` is left
    /// alone for the same reason — the provider-side response chain must
    /// not jump past a turn we never persisted.
    async fn persist_turn_accounting(
        &self,
        cost: &CostSnapshot,
        metrics: &TurnMetrics,
        token_calibration: &squeezy_llm::TokenCalibration,
        mark_resume_available: bool,
    ) {
        let calibration_for_global = {
            let mut state = self.conversation_state.lock().await;
            merge_cost(&mut state.cost, cost);
            state.metrics.merge_turn(metrics);
            state.redactions += metrics.redactions;
            state.token_calibration = token_calibration.clone();
            if let Some(session) = &self.session_log {
                let _ = session.write_resume_state(&state.to_resume_state());
                let calibration_for_metadata = state.token_calibration.clone();
                let _ = session.update_metadata(|metadata| {
                    metadata.cost = state.cost.clone();
                    metadata.metrics = state.metrics.clone();
                    metadata.redactions = state.redactions;
                    if mark_resume_available {
                        metadata.resume_available = true;
                    }
                    metadata.mode = load_session_mode(&self.session_mode);
                    metadata.token_calibration = calibration_for_metadata;
                });
            }
            state.token_calibration.clone()
        };
        // Mirror the calibration into the cross-session file so brand-new
        // sessions (no resume metadata yet) seed off a recent ratio rather
        // than the per-provider defaults. Failures are silent — the global
        // file is a warm-start cache, not a source of truth.
        let _ = SessionStore::open(&self.config).save_global_calibration(&calibration_for_global);
    }

    async fn finish_cancelled_turn(
        &self,
        task_title: &str,
        cost: &CostSnapshot,
        metrics: &TurnMetrics,
        token_calibration: &squeezy_llm::TokenCalibration,
    ) {
        self.persist_turn_accounting(cost, metrics, token_calibration, false)
            .await;
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
                "hash": replay_hash(&replay_request_view(request)),
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
    token_calibration: squeezy_llm::TokenCalibration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentKind {
    Delegate,
    Explore,
    DocHelp,
    Plan,
    Review,
}

impl SubagentKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delegate => "delegate",
            Self::Explore => "explore",
            Self::DocHelp => "doc_help",
            Self::Plan => "plan",
            Self::Review => "review",
        }
    }

    /// Role-catalog overlay for the subagent kind, when one applies.
    ///
    /// `Delegate` keeps its existing broad-research behavior — the Worker
    /// role is roadmap, and mapping delegate to Explorer would strip its
    /// access to `plan_patch` and skill discovery — so it returns `None`.
    fn role(self) -> Option<SubagentRole> {
        match self {
            Self::Delegate => None,
            Self::Explore => Some(SubagentRole::Explorer),
            Self::DocHelp => None,
            Self::Plan => Some(SubagentRole::Planner),
            Self::Review => Some(SubagentRole::Reviewer),
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

/// Who initiated a tool call. Surfaced on `AgentEvent::ToolCallStarted`
/// so the TUI and `squeezy-eval` can render distinct icons (planner
/// preflight vs. the model's own dispatch) and so legibility rules
/// like `redundant_graph_lookup` can attribute hits correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolOrigin {
    /// Pre-LLM exploration plan executed before the model sees the
    /// prompt. The user never asked for these directly; we ran them to
    /// seed receipts.
    Planner,
    /// Tools the model itself requested during its response.
    #[default]
    Model,
    /// Tools executed inside a subagent. Currently emitted only for
    /// completeness — the parent surfaces a `SubagentStarted` event for
    /// the actual dispatch.
    Subagent,
}

#[derive(Clone)]
struct ToolExecutionContext<'a> {
    turn_id: TurnId,
    origin: ToolOrigin,
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
    ai_reviewer_state: Arc<StdMutex<ai_reviewer::AiReviewerState>>,
    session_mode: Arc<AtomicU8>,
    session_log: Option<SessionHandle>,
    conversation_state: Option<Arc<Mutex<ConversationState>>>,
    task_state: Arc<Mutex<Option<TaskStateSnapshot>>>,
    subagents: SubagentRegistry,
    all_tool_specs: &'a [AdvertisedTool],
    loaded_tool_schemas: Arc<Mutex<Vec<String>>>,
    exploration_state: Arc<Mutex<ExplorationTurnState>>,
    /// Hook registry shared with the parent `Agent` / `TurnRuntime`.
    /// `None` when no hooks are installed — `run_one_tool` checks this
    /// before building a `HookContext` so the no-hooks path costs zero
    /// allocations.
    hooks: Option<Arc<HookRegistry>>,
}

impl ToolExecutionContext<'_> {
    /// Session id derived from the session log handle, used by plan-mode
    /// path-scoped write exception (issue 17). `None` when the session
    /// has not yet been assigned an id (pre-first-turn window) or has no
    /// log handle (in-memory test scenarios).
    fn session_id_for_plan_mode(&self) -> Option<String> {
        self.session_log
            .as_ref()
            .map(|handle| handle.session_id().to_string())
    }
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
            let send_result = match send_request.or_cancel(&cancel).await {
                Ok(result) => result,
                Err(CancelErr::Cancelled) => return McpElicitationResponse::cancel(),
            };
            if send_result.is_err() {
                return McpElicitationResponse::decline();
            }
            match response_rx.or_cancel(&cancel).await {
                Ok(response) => response.unwrap_or_else(|_| McpElicitationResponse::decline()),
                Err(CancelErr::Cancelled) => McpElicitationResponse::cancel(),
            }
        })
    });
    context.tools.set_mcp_elicitation_handler(Some(handler));
    McpElicitationHandlerScope {
        tools: context.tools,
    }
}

#[derive(Clone)]
struct PermissionDecisionContext {
    turn_id: TurnId,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    config: AppConfig,
    redactor: Arc<Redactor>,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    approval_ids: Arc<AtomicU64>,
    session_rules: Arc<RwLock<Vec<PermissionRule>>>,
    ai_reviewer_state: Arc<StdMutex<ai_reviewer::AiReviewerState>>,
    session_mode: Arc<AtomicU8>,
    session_log: Option<SessionHandle>,
    conversation_state: Option<Arc<Mutex<ConversationState>>>,
    telemetry: TelemetryClient,
}

impl PermissionDecisionContext {
    fn from_tool_context(context: &ToolExecutionContext<'_>) -> Self {
        Self {
            turn_id: context.turn_id,
            provider: context.provider.clone(),
            tools: context.tools.clone(),
            config: context.config.clone(),
            redactor: context.redactor.clone(),
            tx: context.tx.clone(),
            cancel: context.cancel.clone(),
            approval_ids: context.approval_ids.clone(),
            session_rules: context.session_rules.clone(),
            ai_reviewer_state: context.ai_reviewer_state.clone(),
            session_mode: context.session_mode.clone(),
            session_log: context.session_log.clone(),
            conversation_state: context.conversation_state.clone(),
            telemetry: context.telemetry.clone(),
        }
    }

    /// Session id derived from the session log handle, used by plan-mode
    /// path-scoped write exception (issue 17). Mirrors
    /// `ToolExecutionContext::session_id_for_plan_mode`.
    fn session_id_for_plan_mode(&self) -> Option<String> {
        self.session_log
            .as_ref()
            .map(|handle| handle.session_id().to_string())
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

async fn handle_request_user_input_call(
    context: &ToolExecutionContext<'_>,
    call: &ToolCall,
) -> ToolResult {
    let active_mode = load_session_mode(&context.session_mode);
    if active_mode != SessionMode::Plan {
        return control_tool_result(
            call,
            ToolStatus::Denied,
            json!({
                "ok": false,
                "status": "refused",
                "mode": active_mode.as_str(),
                "error": "request_user_input is only available in Plan mode"
            }),
        );
    }

    #[derive(Deserialize)]
    struct Args {
        question: String,
        #[serde(default)]
        choices: Vec<ArgChoice>,
        #[serde(default)]
        allow_freeform: bool,
    }
    #[derive(Deserialize)]
    struct ArgChoice {
        label: String,
        value: String,
    }

    let args: Args = match serde_json::from_value(call.arguments.clone()) {
        Ok(args) => args,
        Err(error) => {
            return control_tool_result(
                call,
                ToolStatus::Error,
                json!({
                    "ok": false,
                    "error": format!("invalid request_user_input arguments: {error}")
                }),
            );
        }
    };

    let question = args.question.trim().to_string();
    if question.is_empty() {
        return control_tool_result(
            call,
            ToolStatus::Error,
            json!({
                "ok": false,
                "error": "request_user_input.question must be non-empty"
            }),
        );
    }

    let request = RequestUserInputRequest {
        question,
        choices: args
            .choices
            .into_iter()
            .map(|c| RequestUserInputChoice {
                label: c.label,
                value: c.value,
            })
            .collect(),
        allow_freeform: args.allow_freeform,
    };

    let (response_tx, response_rx) = oneshot::channel::<RequestUserInputResponse>();
    if context
        .tx
        .send(AgentEvent::RequestUserInputRequested {
            turn_id: context.turn_id,
            request,
            response_tx,
        })
        .await
        .is_err()
    {
        return control_tool_result(
            call,
            ToolStatus::Error,
            json!({
                "ok": false,
                "error": "TUI is no longer receiving events; cannot ask the user"
            }),
        );
    }

    let response = tokio::select! {
        biased;
        _ = context.cancel.cancelled() => RequestUserInputResponse::cancelled(),
        result = response_rx => result.unwrap_or_else(|_| RequestUserInputResponse::cancelled()),
    };

    let mut payload = json!({
        "ok": true,
        "action": match response.action {
            RequestUserInputAction::Choice => "choice",
            RequestUserInputAction::Freeform => "freeform",
            RequestUserInputAction::Cancelled => "cancelled",
        },
    });
    if let Some(map) = payload.as_object_mut() {
        if let Some(choice) = response.choice_value {
            map.insert("choice_value".to_string(), Value::String(choice));
        }
        if let Some(text) = response.freeform {
            map.insert("freeform".to_string(), Value::String(text));
        }
    }
    let status = if matches!(response.action, RequestUserInputAction::Cancelled) {
        ToolStatus::Cancelled
    } else {
        ToolStatus::Success
    };
    control_tool_result(call, status, payload)
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
    let session_id_for_plan_mode = context.session_id_for_plan_mode();
    let plan_edit_allowed = plan_mode::plan_edit_allowed_in_workspace(
        active_mode,
        &context.config.workspace_root,
        session_id_for_plan_mode.as_deref(),
    );
    if mode_refuses_capability(active_mode, tool.capability, plan_edit_allowed) {
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
    /// Structured JSON payload extracted from the final assistant message,
    /// when the subagent honored the deterministic-keys contract. `None`
    /// when no JSON tail is present or it failed to parse, in which case
    /// `summary` carries the raw text and callers can fall back to it.
    structured_output: Option<Value>,
}

/// Increments the per-kind subagent call bucket. The four audited buckets
/// (delegate/explore/plan/review) feed `/cost`-style telemetry; kinds
/// outside that set (e.g. `doc_help`) are intentionally not bucketed so
/// the rollup matches the operator-facing taxonomy.
fn record_subagent_kind_call(metrics: &mut TurnMetrics, kind: SubagentKind) {
    if let Some(bucket) = metrics.subagent_by_kind.bucket_mut(kind.as_str()) {
        bucket.calls += 1;
    }
}

fn record_subagent_kind_failure(metrics: &mut TurnMetrics, kind: SubagentKind) {
    if let Some(bucket) = metrics.subagent_by_kind.bucket_mut(kind.as_str()) {
        bucket.failures += 1;
    }
}

fn record_subagent_kind_execution(
    metrics: &mut TurnMetrics,
    kind: SubagentKind,
    execution: &TurnMetrics,
) {
    if let Some(bucket) = metrics.subagent_by_kind.bucket_mut(kind.as_str()) {
        bucket.tool_calls += execution.tool_calls;
        bucket.bytes_read += execution.bytes_read;
        merge_cost(&mut bucket.provider, &execution.provider);
    }
}

async fn handle_subagent_call(
    context: &ToolExecutionContext<'_>,
    call: &ToolCall,
    kind: SubagentKind,
    broker: &mut CostBroker,
) -> ToolResult {
    broker.metrics.subagent_calls += 1;
    record_subagent_kind_call(&mut broker.metrics, kind);
    if !context.config.subagents.enabled
        || (kind == SubagentKind::Explore && !context.config.subagents.explore_enabled)
    {
        broker.metrics.subagent_failures += 1;
        record_subagent_kind_failure(&mut broker.metrics, kind);
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
                structured_output: None,
            },
        );
    }
    let request = match parse_subagent_request(call, kind) {
        Ok(request) => request,
        Err(error) => {
            broker.metrics.subagent_failures += 1;
            record_subagent_kind_failure(&mut broker.metrics, kind);
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
                    structured_output: None,
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

    let child_cancel = context.cancel.child_token();
    let lease = match context.subagents.start(
        kind.role().unwrap_or(SubagentRole::Explorer),
        child_cancel.clone(),
        SUBAGENT_MAX_CONCURRENT,
        format!("{} starting", kind.as_str()),
    ) {
        Ok(lease) => lease,
        Err(error) => {
            broker.metrics.subagent_failures += 1;
            record_subagent_kind_failure(&mut broker.metrics, kind);
            return subagent_control_result(
                call,
                kind,
                SubagentExecution {
                    status: ToolStatus::Denied,
                    summary: String::new(),
                    status_label: "capped",
                    error: Some(error),
                    metrics: TurnMetrics::default(),
                    supporting_receipts: Vec::new(),
                    model: subagent_model_for_kind(context.provider.name(), context.config, kind),
                    structured_output: None,
                },
            );
        }
    };

    let execution = run_subagent(context, kind, request).await;
    drop(lease);
    broker
        .metrics
        .merge_subagent_tool_metrics(&execution.metrics);
    record_subagent_kind_execution(&mut broker.metrics, kind, &execution.metrics);
    if execution.status != ToolStatus::Success {
        broker.metrics.subagent_failures += 1;
        record_subagent_kind_failure(&mut broker.metrics, kind);
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
    if !matches!(kind, SubagentKind::Explore) && thoroughness.is_some() {
        return Err(format!("{} does not accept thoroughness", kind.as_str()));
    }
    let prompt = match kind {
        SubagentKind::Plan => call
            .arguments
            .get("goal")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "missing required string field: goal".to_string())?
            .to_string(),
        SubagentKind::Review => call
            .arguments
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                "Review the current diff. Report only actionable findings.".to_string()
            }),
        SubagentKind::Delegate | SubagentKind::Explore | SubagentKind::DocHelp => call
            .arguments
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "missing required string field: prompt".to_string())?
            .to_string(),
    };
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
    // Plan/Delegate/Review subagents do real agent work and should be sized
    // like the main agent — peer agents (codex, opencode) apply no output cap
    // at all, CC only caps its narrow fork. Inherit the parent's cap; only
    // fall back to `max_summary_tokens` when the parent didn't set one, so
    // users with a strict global ceiling still get that ceiling honored.
    // DocHelp keeps its own floor because its "summary" IS the user-facing
    // answer (not a synopsis) — see DEFAULT_DOC_HELP_MAX_OUTPUT_TOKENS.
    config.max_output_tokens = match kind {
        SubagentKind::DocHelp => parent
            .config
            .max_output_tokens
            .or(Some(squeezy_core::DEFAULT_DOC_HELP_MAX_OUTPUT_TOKENS)),
        _ => parent
            .config
            .max_output_tokens
            .or(Some(config.subagents.max_summary_tokens)),
    };
    config.max_tool_calls_per_turn = config.subagents.max_tool_calls_per_call;
    config.max_tool_bytes_read_per_turn = config.subagents.max_tool_bytes_read_per_call;
    config.max_search_files_per_turn = config.subagents.max_search_files_per_call;
    // Subagent inherits the parent's per-round result-bytes cap directly.
    // The previous `.min(24_000)` halved the budget for a subagent that
    // already had fewer tool calls to spend; no peer agent applies a
    // smaller per-result cap to subagents than to the parent.
    let model = subagent_model_for_kind(parent.provider.name(), &config, kind);
    config.model = model.clone();

    let allowed_tools = subagent_allowed_tools(parent.all_tool_specs, kind);
    // DocHelp answers from inlined corpus, so a tool-less call is the intended
    // shape. Other subagent kinds still require at least one read-only tool.
    if allowed_tools.is_empty() && !matches!(kind, SubagentKind::DocHelp) {
        return SubagentExecution {
            status: ToolStatus::Error,
            summary: String::new(),
            status_label: "failed",
            error: Some("no read-only tools are available to the subagent".to_string()),
            metrics: TurnMetrics::default(),
            supporting_receipts: Vec::new(),
            model,
            structured_output: None,
        };
    }
    let allowed_tool_names = allowed_tools
        .iter()
        .map(|tool| tool.spec.name.clone())
        .collect::<BTreeSet<_>>();
    // Subagents in Plan mode are deliberately read-only; the active-plan
    // write exception applies to the top-level interactive session, not
    // to spawned subagents.
    let tool_specs = advertised_tool_specs(&allowed_tools, SessionMode::Plan, false);
    let instructions = subagent_instructions(kind, &request);
    let redacted_instructions = parent.redactor.redact(&instructions);
    let mut broker = CostBroker::new(&config);
    broker.metrics.redactions += redacted_instructions.redactions;
    let mut assistant_stream = StreamRedactor::new(parent.redactor.clone());
    let mut assistant_message = String::new();
    let mut conversation = vec![redact_input_item(
        LlmInputItem::UserText(subagent_user_prompt(&request)),
        &parent.redactor,
    )];
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

    let mut execution = run_subagent_loop(
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
    // Plan and Review subagents promise a JSON object on the final
    // assistant line; harvest it into `structured_output` so the
    // parent can iterate findings as data. Failure to parse keeps
    // `structured_output = None` and the raw text in `summary`.
    if matches!(kind, SubagentKind::Plan | SubagentKind::Review)
        && execution.status == ToolStatus::Success
        && execution.structured_output.is_none()
    {
        execution.structured_output = parse_subagent_structured_tail(&execution.summary);
    }
    execution
}

#[allow(clippy::too_many_arguments)]
async fn run_subagent_loop(
    parent: &ToolExecutionContext<'_>,
    config: &AppConfig,
    tool_specs: &[Arc<LlmToolSpec>],
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
    let runtime_budget = config.subagents.max_runtime_secs.map(Duration::from_secs);
    let Some(budget) = runtime_budget else {
        return run_subagent_rounds(
            parent,
            config,
            tool_specs,
            allowed_tools,
            allowed_tool_names,
            instructions,
            hidden_tx,
            local_jobs,
            local_task_state,
            local_loaded_schemas,
            local_mode,
            local_exploration,
            seen_outputs,
            broker,
            assistant_stream,
            assistant_message,
            conversation,
            supporting_receipts,
            model,
        )
        .await;
    };
    let loop_model = model.clone();
    let timed = tokio::time::timeout(
        budget,
        run_subagent_rounds(
            parent,
            config,
            tool_specs,
            allowed_tools,
            allowed_tool_names,
            instructions,
            hidden_tx,
            local_jobs,
            local_task_state,
            local_loaded_schemas,
            local_mode,
            local_exploration,
            seen_outputs,
            broker,
            assistant_stream,
            assistant_message,
            conversation,
            supporting_receipts,
            loop_model,
        ),
    )
    .await;
    match timed {
        Ok(execution) => execution,
        Err(_) => {
            broker.metrics.redactions += assistant_stream.total_redactions();
            SubagentExecution {
                status: ToolStatus::Error,
                summary: String::new(),
                status_label: "timed_out",
                error: Some(format!(
                    "subagent exceeded {}s wall-clock budget",
                    budget.as_secs()
                )),
                metrics: broker.metrics.clone(),
                supporting_receipts: std::mem::take(supporting_receipts),
                model,
                structured_output: None,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_subagent_rounds(
    parent: &ToolExecutionContext<'_>,
    config: &AppConfig,
    tool_specs: &[Arc<LlmToolSpec>],
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
        let request_model: Arc<str> = Arc::from(config.model.as_str());
        let llm_request = LlmRequest {
            model: Arc::clone(&request_model),
            instructions: Arc::from(instructions),
            input: Arc::from(conversation.as_slice()),
            max_output_tokens: config.max_output_tokens,
            response_verbosity: request_response_verbosity(config, parent.provider.name()),
            reasoning_effort: request_reasoning_effort(config, parent.provider.name()),
            previous_response_id: None,
            cache_key: None,
            tools: Arc::from(tool_specs),
            store: false,
            output_schema: None,
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
                        structured_output: None,
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
                LlmEvent::ReasoningDelta { .. } => {}
                LlmEvent::ReasoningDone(_) => {}
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
                        structured_output: None,
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
                        origin: ToolOrigin::Subagent,
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
                        ai_reviewer_state: parent.ai_reviewer_state.clone(),
                        session_mode: local_mode.clone(),
                        session_log: None,
                        conversation_state: None,
                        task_state: local_task_state.clone(),
                        all_tool_specs: allowed_tools,
                        loaded_tool_schemas: local_loaded_schemas.clone(),
                        exploration_state: local_exploration.clone(),
                        subagents: parent.subagents.clone(),
                        hooks: parent.hooks.clone(),
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
            let output = parent.redactor.redact(&pending.result.model_output()).text;
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
        structured_output: None,
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
        structured_output: None,
    }
}

/// Tries to extract a single JSON object from the tail of a Plan/Review
/// subagent's final assistant message. Models that obey the deterministic-
/// keys contract emit `{"findings": [...], "summary": "..."}` on the last
/// non-empty line; we accept either the whole trimmed text being JSON or
/// the largest `{...}` substring near the end. Returns `None` when no
/// brace pair is found or it fails to parse — callers fall back to the
/// raw `summary` string in that case.
fn parse_subagent_structured_tail(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && value.is_object()
    {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if start >= end {
        return None;
    }
    let slice = trimmed.get(start..=end)?;
    serde_json::from_str::<Value>(slice)
        .ok()
        .filter(Value::is_object)
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
    if let Some(structured) = execution.structured_output {
        content["structured_output"] = structured;
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
            let base = role_config(SubagentRole::Explorer).instructions;
            format!("{base}\n\nThoroughness: {thoroughness}.")
        }
        SubagentKind::DocHelp => {
            "You are Squeezy's hidden documentation subagent. Answer the user's Squeezy help question using ONLY the inlined bundled doc corpus and the inlined redacted config snapshot provided in the user prompt. You have no tools and must not request any; the corpus is already in your context. Cite specific bundled doc paths (e.g., `docs/external/PROVIDERS.md`) and relevant config sections (e.g., `[model]`) inline in your answer. If the inlined docs do not cover the question, say so explicitly and point the user to https://squeezyagent.com/docs/ and https://github.com/esqueezy/squeezy rather than guessing. Do not mention internal agent mechanics, do not invent file paths beyond the inlined corpus, and do not ask the user follow-up questions.".to_string()
        }
        SubagentKind::Plan => {
            let base = role_config(SubagentRole::Planner).instructions;
            format!(
                "{base}\n\nReturn structured JSON-ready findings: ordered steps with rationale, impacted files/symbols, and a recommended plan_id when plan_patch is called. Do not modify files or run shell commands.\n\n{SUBAGENT_JSON_TAIL_INSTRUCTION}"
            )
        }
        SubagentKind::Review => {
            let base = role_config(SubagentRole::Reviewer).instructions;
            format!(
                "{base}\n\nReport actionable issues only. Each finding must include severity (blocker|warning|info), file, line (if known), message, and suggested_fix when one is obvious. Return pass=true only when no blocker or warning remains.\n\n{SUBAGENT_JSON_TAIL_INSTRUCTION}"
            )
        }
    }
}

fn subagent_model_for_kind(provider: &str, config: &AppConfig, kind: SubagentKind) -> String {
    let parent_model = config.model.clone();
    // Honor the role catalog's model policy where it applies. `Delegate`
    // has no role overlay and keeps the parent model. `Explore` defers to
    // the configured explore_model and falls back to a cheap default for
    // the provider when one is known.
    let policy = kind
        .role()
        .map(|role| role_config(role).model_policy)
        .unwrap_or(RoleModelPolicy::Parent);
    match (kind, policy) {
        (SubagentKind::Explore, _) => config.subagents.explore_model.clone().unwrap_or_else(|| {
            default_cheap_model_for_provider(provider)
                .unwrap_or(&parent_model)
                .to_string()
        }),
        (SubagentKind::DocHelp, _) => parent_model,
        (_, RoleModelPolicy::Parent) => parent_model,
        (_, RoleModelPolicy::Cheap) => default_cheap_model_for_provider(provider)
            .unwrap_or(&parent_model)
            .to_string(),
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

// DocHelp answers from the inlined bundled doc corpus, not from filesystem
// search — those tools would read the user's working directory (not the
// bundled docs that ship inside the binary) and produce misleading hits.
const DOC_HELP_SUBAGENT_TOOL_NAMES: &[&str] = &[];

fn subagent_allowed_tools(
    all_tool_specs: &[AdvertisedTool],
    kind: SubagentKind,
) -> Vec<AdvertisedTool> {
    let names: BTreeSet<&str> = match kind {
        SubagentKind::Delegate => DELEGATE_SUBAGENT_TOOL_NAMES.iter().copied().collect(),
        SubagentKind::Explore => EXPLORE_SUBAGENT_TOOL_NAMES.iter().copied().collect(),
        SubagentKind::DocHelp => DOC_HELP_SUBAGENT_TOOL_NAMES.iter().copied().collect(),
        SubagentKind::Plan => role_config(SubagentRole::Planner)
            .allowed_tools
            .iter()
            .copied()
            .collect(),
        SubagentKind::Review => role_config(SubagentRole::Reviewer)
            .allowed_tools
            .iter()
            .copied()
            .collect(),
    };
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
    // Including `command` keeps distinct shell invocations distinct: otherwise
    // any two cargo errors with exit 101 collapse to the same key and trip the
    // guard on unrelated failures.
    let command = result
        .content
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(format!(
        "{}:{:?}:{path}:{command}:{}",
        result.tool_name,
        result.status,
        tool_failure_detail(result)
    ))
}

fn is_control_tool_name(name: &str) -> bool {
    matches!(
        name,
        TASK_STATE_TOOL_NAME | LOAD_TOOL_SCHEMA_TOOL_NAME | REQUEST_USER_INPUT_TOOL_NAME
    )
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
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
        if call.name == REQUEST_USER_INPUT_TOOL_NAME {
            let result = handle_request_user_input_call(&context, call).await;
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
        if has_invalid_tool_arguments(call) {
            let result = invalid_tool_arguments_result(call);
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
        let subagent_kind = match call.name.as_str() {
            DELEGATE_TOOL_NAME => Some(SubagentKind::Delegate),
            EXPLORE_TOOL_NAME => Some(SubagentKind::Explore),
            DELEGATE_PLAN_TOOL_NAME => Some(SubagentKind::Plan),
            DELEGATE_REVIEW_TOOL_NAME => Some(SubagentKind::Review),
            _ => None,
        };
        if let Some(kind) = subagent_kind {
            let _ = context
                .tx
                .send(AgentEvent::ToolCallStarted {
                    turn_id: context.turn_id,
                    call: redact_tool_call(call.clone(), &context.redactor),
                    origin: context.origin,
                })
                .await;
            let result = Box::pin(handle_subagent_call(&context, call, kind, broker)).await;
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
                results[index] = Some(result);
                recorded[index] = true;
                continue;
            }
            let result = run_one_tool(context.clone(), tool_sequence, call).await;
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
            results[index] = Some(result);
            recorded[index] = true;
        }
    }
    flush_parallel_batch(&context, broker, &mut results, &mut parallel_batch).await;

    let mut out = collect_recorded_results(
        results,
        recorded,
        broker,
        context.config,
        &context.telemetry,
    );
    mark_intra_batch_duplicates(&calls, &mut out, context.tools);
    out
}

/// Stamp a `duplicate_of` hint onto any tool result whose call has the
/// same `(tool_name, args_sha256)` as an earlier call in the same batch,
/// for tools where re-running can only produce the same answer
/// (`is_parallel_safe`). The execution still happens — flipping that to
/// a real skip needs to thread through cancellation, event emission,
/// and broker accounting — but the marker gives the model immediate
/// feedback so it stops issuing the same grep three times in a row.
fn mark_intra_batch_duplicates(
    calls: &[ToolCall],
    results: &mut [ToolResult],
    tools: &ToolRegistry,
) {
    let mut first_by_key: BTreeMap<(String, String), String> = BTreeMap::new();
    for (call, result) in calls.iter().zip(results.iter_mut()) {
        if !tools.is_parallel_safe(call) {
            continue;
        }
        let Some(args_sha) = tool_call_args_sha256(call) else {
            continue;
        };
        let key = (call.name.clone(), args_sha);
        match first_by_key.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(call.call_id.clone());
            }
            std::collections::btree_map::Entry::Occupied(slot) => {
                if let Some(obj) = result.content.as_object_mut() {
                    obj.insert("duplicate_of".to_string(), json!(slot.get().clone()));
                    obj.entry("hint").or_insert_with(|| {
                        json!(
                            "This call is identical to an earlier call in the same response. \
                             Do not issue duplicate tool calls; reuse the earlier output."
                        )
                    });
                }
            }
        }
    }
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
                origin: ToolOrigin::Model,
            })
            .await;
        record_and_emit_progress(broker, result, &tx, turn_id).await;
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
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
                record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
            record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
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
        record_and_emit_progress(broker, &result, &context.tx, context.turn_id).await;
        results[index] = Some(result);
    }
}

/// Fan out a `HookEvent::PreToolUse` to every registered handler when
/// a hook registry is installed. Mutation and deny replies are logged
/// but not yet applied — mirrors the observational contract of
/// [`TurnRuntime::dispatch_pre_turn`] so a follow-up commit can wire
/// enforcement without changing the call site. Returns immediately
/// when no registry is configured so the no-hooks path costs zero
/// allocations.
fn dispatch_pre_tool_use(context: &ToolExecutionContext<'_>, call: &ToolCall) {
    let Some(registry) = context.hooks.as_ref() else {
        return;
    };
    if registry.is_empty() {
        return;
    }
    let payload = json!({
        "turn_id": context.turn_id.to_string(),
        "tool_name": call.name,
        "call_id": call.call_id,
    });
    let results = registry.dispatch(HookEvent::PreToolUse, payload);
    for (idx, result) in results.iter().enumerate() {
        if let Some(mutate) = result.mutate.as_ref() {
            tracing::debug!(
                target: "squeezy::hooks",
                turn_id = %context.turn_id,
                tool_name = %call.name,
                call_id = %call.call_id,
                handler_index = idx,
                %mutate,
                "PreToolUse handler proposed a mutation (not yet applied)"
            );
        }
        if !result.allow {
            tracing::debug!(
                target: "squeezy::hooks",
                turn_id = %context.turn_id,
                tool_name = %call.name,
                call_id = %call.call_id,
                handler_index = idx,
                message = result.message.as_deref().unwrap_or(""),
                "PreToolUse handler returned allow=false (not yet enforced)"
            );
        }
    }
}

/// Fan out a `HookEvent::PostToolUse` to every registered handler after
/// a tool result is available. Same observational contract as
/// [`dispatch_pre_tool_use`]; the payload adds `status` so audit
/// handlers can record per-tool outcomes.
fn dispatch_post_tool_use(context: &ToolExecutionContext<'_>, result: &ToolResult) {
    let Some(registry) = context.hooks.as_ref() else {
        return;
    };
    if registry.is_empty() {
        return;
    }
    let payload = json!({
        "turn_id": context.turn_id.to_string(),
        "tool_name": result.tool_name,
        "call_id": result.call_id,
        "status": tool_status_str(result.status),
    });
    let results = registry.dispatch(HookEvent::PostToolUse, payload);
    for (idx, hook_result) in results.iter().enumerate() {
        if let Some(mutate) = hook_result.mutate.as_ref() {
            tracing::debug!(
                target: "squeezy::hooks",
                turn_id = %context.turn_id,
                tool_name = %result.tool_name,
                call_id = %result.call_id,
                handler_index = idx,
                %mutate,
                "PostToolUse handler proposed a mutation (not yet applied)"
            );
        }
        if !hook_result.allow {
            tracing::debug!(
                target: "squeezy::hooks",
                turn_id = %context.turn_id,
                tool_name = %result.tool_name,
                call_id = %result.call_id,
                handler_index = idx,
                message = hook_result.message.as_deref().unwrap_or(""),
                "PostToolUse handler returned allow=false (not yet enforced)"
            );
        }
    }
}

fn tool_status_str(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Success => "success",
        ToolStatus::Error => "error",
        ToolStatus::Denied => "denied",
        ToolStatus::Stale => "stale",
        ToolStatus::Cancelled => "cancelled",
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
            origin: context.origin,
        })
        .await;
    let started = Instant::now();
    let shell_ask_approver = if call.name == "shell" {
        Some(shell_ask_approver_for_context(&context))
    } else {
        None
    };
    // Capture a borrow-able snapshot of the call before it moves into the
    // tool registry, so paired-SHA telemetry (F06) can hash its arguments
    // when emitting the completion event.
    let call_for_telemetry = call.clone();
    let progress_call_id = call_for_telemetry.call_id.clone();
    let progress_tool_name = call_for_telemetry.name.clone();
    // Fire the PreToolUse hook once per executed tool call, immediately
    // before the tool registry takes ownership of the call. Mutation /
    // deny replies are currently observational — see
    // `dispatch_pre_tool_use` for the contract that will tighten when
    // enforcement lands.
    dispatch_pre_tool_use(&context, &call_for_telemetry);
    let exec_future = context.tools.execute_for_group_with_options(
        call,
        tracked_job
            .as_ref()
            .map(|(_, cancel)| cancel.clone())
            .unwrap_or_else(|| context.cancel.clone()),
        context.turn_id.to_string(),
        ToolExecutionOptions { shell_ask_approver },
    );
    tokio::pin!(exec_future);
    let mut progress_ticker = tokio::time::interval(TOOL_PROGRESS_INTERVAL);
    // `interval` fires immediately on first poll; skip that tick so the
    // heartbeat only fires once the tool has actually been running.
    progress_ticker.tick().await;
    let result = loop {
        tokio::select! {
            r = &mut exec_future => break r,
            _ = progress_ticker.tick() => {
                let _ = context
                    .tx
                    .send(AgentEvent::ToolProgress {
                        turn_id: context.turn_id,
                        call_id: progress_call_id.clone(),
                        tool_name: progress_tool_name.clone(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    })
                    .await;
            }
        }
    };
    // Fire the PostToolUse hook as soon as the tool result is in hand,
    // before downstream job/telemetry bookkeeping. Same observational
    // contract as `dispatch_pre_tool_use`.
    dispatch_post_tool_use(&context, &result);
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

/// Number of *completed* tool calls between successive
/// `AgentEvent::CostUpdate` emissions within a single turn.
const COST_UPDATE_STRIDE: u64 = 3;

/// How often a still-running tool call emits an
/// `AgentEvent::ToolProgress` heartbeat. A user staring at a
/// terminal needs feedback within roughly a second to feel the
/// agent is alive but stable.
const TOOL_PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// Emit an `AgentEvent::CostUpdate` if the broker has just crossed a
/// `COST_UPDATE_STRIDE`-sized boundary. Call this immediately after
/// `broker.record_executed_result(...)` at every tool-completion site.
async fn maybe_emit_cost_update(
    broker: &CostBroker,
    tx: &mpsc::Sender<AgentEvent>,
    turn_id: TurnId,
) {
    if let Some(snap) = broker.progress_snapshot_if_due(COST_UPDATE_STRIDE) {
        let _ = tx
            .send(AgentEvent::CostUpdate {
                turn_id,
                tool_count: snap.tool_count,
                input_tokens: snap.input_tokens,
                micro_usd: snap.micro_usd,
            })
            .await;
    }
}

/// Record an executed tool result and emit a progress callout if the
/// stride boundary was crossed. Replaces direct calls to
/// `broker.record_executed_result` at tool-completion sites so the
/// progress event fires for every completion path (success, denial,
/// budget refusal, cancellation).
async fn record_and_emit_progress(
    broker: &mut CostBroker,
    result: &ToolResult,
    tx: &mpsc::Sender<AgentEvent>,
    turn_id: TurnId,
) {
    broker.record_executed_result(result);
    maybe_emit_cost_update(broker, tx, turn_id).await;
    maybe_emit_shell_sandbox_fallback_warning(tx, turn_id, result).await;
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
    // `approval.best_effort.fallback{tool=shell}` ticks once per silent
    // shell-sandbox degradation. Co-located with the per-tool event so
    // every call site that already calls `emit_tool_telemetry` benefits
    // without threading the new event through individual handlers.
    if let Some(fallback) = shell_best_effort_fallback_from_result(result) {
        telemetry.spawn(TelemetryEvent::shell_sandbox_best_effort_fallback(
            &fallback.backend,
        ));
    }
}

/// Detect a shell best_effort sandbox fallback in `result` and, when this
/// is the first occurrence in the session, publish a one-shot
/// [`AgentEvent::ShellSandboxBestEffortFallback`] so the TUI can warn the
/// user. The per-call telemetry counter is emitted separately by
/// [`emit_tool_telemetry`]; this function only handles the user-visible
/// once-per-session signal.
async fn maybe_emit_shell_sandbox_fallback_warning(
    tx: &mpsc::Sender<AgentEvent>,
    turn_id: TurnId,
    result: &ToolResult,
) {
    let Some(ShellBestEffortFallback {
        backend,
        fallback_count,
        first_in_session,
    }) = shell_best_effort_fallback_from_result(result)
    else {
        return;
    };
    if !first_in_session {
        return;
    }
    let _ = tx
        .send(AgentEvent::ShellSandboxBestEffortFallback {
            turn_id,
            backend,
            fallback_count,
        })
        .await;
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

pub(crate) fn is_budget_denied(result: &ToolResult) -> bool {
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

/// Maximum bytes of preceding assistant text passed in
/// [`ToolApprovalRequest::context`]. Sized to fit a few sentences without
/// dominating the approval modal.
const APPROVAL_CONTEXT_CAP: usize = 300;

/// Extract the most recent assistant message from `state`, redact it, and
/// head-truncate to [`APPROVAL_CONTEXT_CAP`] bytes so the approval modal
/// can render "you asked me to X, so I'm trying Y" above the buttons.
async fn approval_context_from_state(
    state: Option<&Arc<Mutex<ConversationState>>>,
    redactor: &Redactor,
) -> Option<String> {
    let state = state?;
    let guard = state.lock().await;
    let last_assistant = guard
        .transcript
        .iter()
        .rev()
        .find(|item| item.role == Role::Assistant)?;
    let redacted = redactor.redact(&last_assistant.content).text;
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(head_truncate_bytes(trimmed, APPROVAL_CONTEXT_CAP))
}

/// Truncate `value` to at most `cap` bytes on a UTF-8 boundary, appending
/// an ellipsis when truncation occurred.
fn head_truncate_bytes(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_string();
    }
    let mut end = cap;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = value[..end].trim_end().to_string();
    out.push('…');
    out
}

async fn permission_decision(
    call: &ToolCall,
    context: &ToolExecutionContext<'_>,
) -> ApprovalDecision {
    if is_direct_user_shell_call(call) {
        return ApprovalDecision::Approved;
    }
    let runtime = PermissionDecisionContext::from_tool_context(context);
    let request = runtime.tools.permission_request(call);
    permission_decision_for_request(&runtime, call, request).await
}

async fn permission_decision_for_request(
    context: &PermissionDecisionContext,
    call: &ToolCall,
    request: PermissionRequest,
) -> ApprovalDecision {
    let active_mode = load_session_mode(&context.session_mode);
    let session_id_for_plan_mode = context.session_id_for_plan_mode();
    let active_plan = plan_mode::latest_plan_path(
        &context.config.workspace_root,
        session_id_for_plan_mode.as_deref(),
    );
    if let Some(verdict) = mode_permission_verdict(active_mode, &request, active_plan.as_deref()) {
        log_permission_verdict(&request, &verdict);
        return ApprovalDecision::Denied(context.redactor.redact(&verdict.reason).text);
    }
    let session_rules = snapshot_session_rules(&context.session_rules);
    let mut verdict = context
        .config
        .permissions
        .evaluate_with_extra(&request, &session_rules);
    if verdict.action == PermissionAction::Ask && context.config.permissions.ai_reviewer.enabled {
        let transcript = if let Some(conversation_state) = &context.conversation_state {
            let state = conversation_state.lock().await;
            Some(ai_reviewer::AiReviewerTranscriptSnapshot {
                items: state.transcript.clone(),
                history_version: state.context_compaction.generation,
                entry_count: state.transcript.len(),
            })
        } else {
            None
        };
        match ai_reviewer::review_permission(ai_reviewer::AiReviewerInput {
            config: &context.config,
            provider: context.provider.clone(),
            request: &request,
            transcript,
            state: context.ai_reviewer_state.clone(),
            turn_id: context.turn_id,
            cancel: context.cancel.child_token(),
            telemetry: context.telemetry.clone(),
        })
        .await
        {
            ai_reviewer::AiReviewerOutcome::Verdict(reviewed) => {
                log_session_event(
                    context.session_log.as_ref(),
                    &context.redactor,
                    "permission_ai_reviewer_decided",
                    Some(context.turn_id),
                    Some(reviewed.action.as_str().to_string()),
                    json!({
                        "action": reviewed.action.as_str(),
                        "reason": reviewed.reason.clone(),
                        "capability": request.capability.as_str(),
                        "target": request.target.clone(),
                    }),
                );
                verdict = reviewed;
            }
            ai_reviewer::AiReviewerOutcome::NoDecision { reason } => {
                log_session_event(
                    context.session_log.as_ref(),
                    &context.redactor,
                    "permission_ai_reviewer_no_decision",
                    Some(context.turn_id),
                    Some(reason.clone()),
                    json!({
                        "reason": reason,
                        "capability": request.capability.as_str(),
                        "target": request.target.clone(),
                    }),
                );
            }
            ai_reviewer::AiReviewerOutcome::CircuitTripped { reason } => {
                let reason = context.redactor.redact(&reason).text;
                log_session_event(
                    context.session_log.as_ref(),
                    &context.redactor,
                    "permission_ai_reviewer_tripped",
                    Some(context.turn_id),
                    Some(reason.clone()),
                    json!({
                        "reason": reason,
                        "capability": request.capability.as_str(),
                        "target": request.target.clone(),
                    }),
                );
                let _ = context
                    .tx
                    .send(AgentEvent::AiReviewerTripped {
                        turn_id: context.turn_id,
                        reason,
                    })
                    .await;
            }
        }
    }
    if should_classify_shell(&context.config, context.provider.name(), &request, &verdict)
        && let Some(classifier) = classify_ambiguous_shell(
            context.provider.clone(),
            &context.config,
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
            let approval_context =
                approval_context_from_state(context.conversation_state.as_ref(), &context.redactor)
                    .await;
            let approval_request = ToolApprovalRequest {
                id: context.approval_ids.fetch_add(1, Ordering::Relaxed),
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                scope: legacy_scope_for_capability(request.capability),
                permission: redact_permission_request(request.clone(), &context.redactor),
                matched_rule: verdict.matched_rule,
                reason: context.redactor.redact(&verdict.reason).text,
                context: approval_context,
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
            let send_result = match send_approval.or_cancel(&context.cancel).await {
                Ok(result) => result,
                Err(CancelErr::Cancelled) => return ApprovalDecision::Cancelled,
            };
            if send_result.is_err() {
                return ApprovalDecision::Denied("approval channel closed".to_string());
            }
            let decision = match decision_rx.or_cancel(&context.cancel).await {
                Ok(decision) => decision,
                Err(CancelErr::Cancelled) => return ApprovalDecision::Cancelled,
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
                Ok(ToolApprovalDecision::AllowSession) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Session,
                        PermissionAction::Allow,
                    )
                    .await;
                    log_session_event(
                        context.session_log.as_ref(),
                        &context.redactor,
                        "permission_session_rule_installed",
                        Some(context.turn_id),
                        Some(request.target.clone()),
                        json!({
                            "capability": request.capability.as_str(),
                            "target": request.target,
                            "action": "allow",
                        }),
                    );
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AllowRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Allow,
                    )
                    .await;
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AllowRuleProject) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Project,
                        PermissionAction::Allow,
                    )
                    .await;
                    ApprovalDecision::Approved
                }
                Ok(ToolApprovalDecision::AskRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Ask,
                    )
                    .await;
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
                    )
                    .await;
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
                Ok(ToolApprovalDecision::DenySession) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Session,
                        PermissionAction::Deny,
                    )
                    .await;
                    log_session_event(
                        context.session_log.as_ref(),
                        &context.redactor,
                        "permission_session_rule_installed",
                        Some(context.turn_id),
                        Some(request.target.clone()),
                        json!({
                            "capability": request.capability.as_str(),
                            "target": request.target,
                            "action": "deny",
                        }),
                    );
                    ApprovalDecision::Denied(permission_denied_reason(
                        &request,
                        "user denied and installed a session rule",
                    ))
                }
                Ok(ToolApprovalDecision::DenyRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Deny,
                    )
                    .await;
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
                    )
                    .await;
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

fn shell_ask_approver_for_context(context: &ToolExecutionContext<'_>) -> ShellAskApprover {
    let runtime = PermissionDecisionContext::from_tool_context(context);
    Arc::new(move |request: ShellAskRequest| {
        let runtime = runtime.clone();
        Box::pin(async move {
            let synthetic_call = ToolCall {
                call_id: format!("{}:ask", request.call_id),
                name: "shell".to_string(),
                arguments: json!({
                    "command": request.command,
                    "workdir": request.workdir.display().to_string(),
                    "description": request.justification,
                }),
            };
            let permission = runtime.tools.permission_request(&synthetic_call);
            match permission_decision_for_request(&runtime, &synthetic_call, permission).await {
                ApprovalDecision::Approved => ShellAskDecision::allow(),
                ApprovalDecision::Denied(reason) => ShellAskDecision::deny(reason),
                ApprovalDecision::Cancelled => {
                    ShellAskDecision::deny("in-flight permission request was cancelled")
                }
            }
        })
    })
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
    active_plan_path: Option<&Path>,
) -> Option<PermissionVerdict> {
    let plan_edit_allowed = matches!(
        (mode, request.capability),
        (SessionMode::Plan, PermissionCapability::Edit)
    ) && active_plan_path
        .is_some_and(|active| plan_mode::is_active_plan_path(Path::new(&request.target), active));
    if !mode_refuses_capability(mode, request.capability, plan_edit_allowed) {
        return None;
    }
    let reason = if mode == SessionMode::Plan && request.capability == PermissionCapability::Edit {
        match active_plan_path {
            Some(active) => format!(
                "Plan mode: only the active plan file is editable ({}); requested target was {}",
                active.display(),
                request.target,
            ),
            None => format!(
                "{} mode refuses {} (no active plan file to edit)",
                mode.as_str(),
                request.capability.as_str()
            ),
        }
    } else {
        format!(
            "{} mode refuses {}",
            mode.as_str(),
            request.capability.as_str()
        )
    };
    Some(PermissionVerdict {
        action: PermissionAction::Deny,
        matched_rule: None,
        reason,
    })
}

/// Single source of truth for whether a session mode forbids a capability.
/// Plan mode allows Read, Search, and (when `plan_edit_allowed` is true)
/// Edit; Build mode allows everything (the configured `PermissionPolicy`
/// still applies). The capability list is intentionally exhaustive
/// (`match`) so adding a new capability is a compile-time prompt to
/// decide whether plan mode admits it. `plan_edit_allowed` is computed
/// by `plan_mode::plan_edit_allowed_in_workspace` at schema-build sites
/// and by `plan_mode::is_active_plan_path` at runtime (issue 2).
fn mode_refuses_capability(
    mode: SessionMode,
    capability: PermissionCapability,
    plan_edit_allowed: bool,
) -> bool {
    if mode == SessionMode::Build {
        return false;
    }
    match capability {
        PermissionCapability::Read | PermissionCapability::Search => false,
        PermissionCapability::Edit => !plan_edit_allowed,
        PermissionCapability::Shell
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
        model: Arc::from(config.model.as_str()),
        instructions: Arc::from(
            "You classify shell-command risk for a local coding agent. Return JSON only.",
        ),
        input: Arc::from(vec![LlmInputItem::UserText(prompt)]),
        max_output_tokens: Some(80),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        output_schema: None,
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
            LlmEvent::Started
            | LlmEvent::ToolCall(_)
            | LlmEvent::ReasoningDelta { .. }
            | LlmEvent::ReasoningDone(_) => {}
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
async fn install_persistent_rule(
    context: &PermissionDecisionContext,
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

    let path = match persistence_path_for(&context.config, source) {
        Some(path) => path,
        None => return,
    };
    // Persistence touches the filesystem and uses a file-presence lock
    // with a 10ms retry sleep; running it on a Tokio worker would block
    // other tasks. `spawn_blocking` parks the work on a dedicated
    // blocking thread instead.
    let persisted = {
        let rule = rule.clone();
        let path_for_blocking = path.clone();
        match tokio::task::spawn_blocking(move || {
            persist_permission_rule(&path_for_blocking, &rule)
        })
        .await
        {
            Ok(Ok(persisted)) => persisted,
            Ok(Err(err)) => {
                tracing::warn!(
                    target: "squeezy::permissions",
                    path = %path.display(),
                    error = %err,
                    "failed to persist permission rule",
                );
                return;
            }
            Err(join_err) => {
                tracing::warn!(
                    target: "squeezy::permissions",
                    path = %path.display(),
                    error = %join_err,
                    "permission persistence task panicked",
                );
                return;
            }
        }
    };
    if !persisted {
        tracing::info!(
            target: "squeezy::permissions",
            path = %path.display(),
            capability = %rule.capability,
            target = %rule.target,
            action = %rule.action.as_str(),
            source = %rule.source.as_str(),
            "permission rule already persisted",
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
    spec: Arc<LlmToolSpec>,
    capability: PermissionCapability,
}

pub(crate) fn advertised_tool(spec: ToolSpec) -> AdvertisedTool {
    AdvertisedTool {
        capability: spec.capability,
        spec: Arc::new(LlmToolSpec {
            name: spec.name,
            description: spec.description,
            parameters: spec.parameters,
            strict: false,
        }),
    }
}

/// Synthetic control tools that are advertised to the model on every
/// request. Progress/task state is intentionally not model-visible: the
/// runtime derives visible working state from turn and tool lifecycle events,
/// so simple prompts cannot burn full model rounds on bookkeeping-only calls.
/// `delegate` and `explore` are gated on [`SubagentConfig::enabled`] /
/// `explore_enabled` so we don't spend prompt tokens advertising tools the
/// agent would refuse on every call.
fn core_control_tools(
    subagents: &SubagentConfig,
    session_mode: SessionMode,
) -> Vec<AdvertisedTool> {
    let mut tools = Vec::new();
    if subagents.enabled {
        tools.push(delegate_advertised_tool());
        if subagents.explore_enabled {
            tools.push(explore_advertised_tool());
        }
        tools.push(delegate_plan_advertised_tool());
        tools.push(delegate_review_advertised_tool());
    }
    if session_mode == SessionMode::Plan {
        tools.push(request_user_input_advertised_tool());
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
        spec: Arc::new(LlmToolSpec {
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
        }),
    }
}

fn delegate_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: Arc::new(LlmToolSpec {
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
        }),
    }
}

fn delegate_plan_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: Arc::new(LlmToolSpec {
            name: DELEGATE_PLAN_TOOL_NAME.to_string(),
            description: "Delegate read-only implementation planning to a Planner subagent. The parent receives ordered steps, impacted files/symbols, and (when plan_patch is used) a plan_id to bind future edits to.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "Concrete implementation goal the planner should produce steps for."
                    },
                    "scope": {
                        "type": ["string", "null"],
                        "description": "Optional paths, modules, symbols, or constraints the plan must stay within."
                    }
                },
                "required": ["goal"]
            }),
            strict: false,
        }),
    }
}

fn delegate_review_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: Arc::new(LlmToolSpec {
            name: DELEGATE_REVIEW_TOOL_NAME.to_string(),
            description: "Delegate read-only review of the current diff to a Reviewer subagent. Returns actionable findings (severity, file, line, message, suggested_fix) and a pass flag.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "scope": {
                        "type": ["string", "null"],
                        "description": "Optional paths or globs to focus the review on. Defaults to the full pending diff."
                    },
                    "prompt": {
                        "type": ["string", "null"],
                        "description": "Optional additional review instructions for this turn."
                    }
                }
            }),
            strict: false,
        }),
    }
}

/// Plan-mode tool that lets the model pause the turn and ask the user a
/// clarifying multiple-choice (or free-form) question. The capability is
/// `Read` so it survives Plan-mode tool filtering; mode gating happens at
/// execute time so a Build-mode call returns a clear error instead of
/// silently disappearing.
fn request_user_input_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: Arc::new(LlmToolSpec {
            name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
            description:
                "Plan mode only. Pause the turn and ask the user a clarifying question. Provide a question; optionally provide multiple-choice options with stable values. Returns the user's selection (or notes they cancelled)."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "Question to display to the user. Should be a complete sentence."
                    },
                    "choices": {
                        "type": "array",
                        "description": "Multiple-choice options. Omit or pass an empty array for free-form input.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "label": {
                                    "type": "string",
                                    "description": "Short human-readable label shown to the user."
                                },
                                "value": {
                                    "type": "string",
                                    "description": "Stable value returned to the model when this choice is picked."
                                }
                            },
                            "required": ["label", "value"]
                        }
                    },
                    "allow_freeform": {
                        "type": "boolean",
                        "description": "When true, the user may also type a free-form answer alongside choices. Default false."
                    }
                },
                "required": ["question"]
            }),
            strict: false,
        }),
    }
}

fn explore_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: Arc::new(LlmToolSpec {
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
        }),
    }
}

fn advertised_tool_specs(
    tools: &[AdvertisedTool],
    mode: SessionMode,
    plan_edit_allowed: bool,
) -> Vec<Arc<LlmToolSpec>> {
    tools
        .iter()
        .filter(|tool| !mode_refuses_capability(mode, tool.capability, plan_edit_allowed))
        .map(|tool| Arc::clone(&tool.spec))
        .collect()
}

fn synthetic_tool_by_name(name: &str) -> Option<AdvertisedTool> {
    match name {
        DELEGATE_TOOL_NAME => Some(delegate_advertised_tool()),
        EXPLORE_TOOL_NAME => Some(explore_advertised_tool()),
        DELEGATE_PLAN_TOOL_NAME => Some(delegate_plan_advertised_tool()),
        DELEGATE_REVIEW_TOOL_NAME => Some(delegate_review_advertised_tool()),
        LOAD_TOOL_SCHEMA_TOOL_NAME => Some(load_tool_schema_advertised_tool()),
        REQUEST_USER_INPUT_TOOL_NAME => Some(request_user_input_advertised_tool()),
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
            LlmInputItem::Reasoning(payload) => {
                shape.reasoning_items += 1;
                shape.reasoning_bytes += payload.display_text().len();
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
    plan_edit_allowed: bool,
) -> Vec<Arc<LlmToolSpec>> {
    if !schema_config.lazy_schema_loading {
        return advertised_tool_specs(tools, mode, plan_edit_allowed);
    }

    // Specs are stored as `Arc<LlmToolSpec>` so a per-round "spec list"
    // build is a sequence of cheap atomic refcount bumps regardless of
    // how many tools (first-party or MCP-loaded) end up in the request.
    let mut specs = Vec::new();
    let mut seen = BTreeSet::new();
    let advertised_names: BTreeSet<&str> =
        tools.iter().map(|tool| tool.spec.name.as_str()).collect();
    let synthetic_order = [
        DELEGATE_TOOL_NAME,
        EXPLORE_TOOL_NAME,
        LOAD_TOOL_SCHEMA_TOOL_NAME,
        REQUEST_USER_INPUT_TOOL_NAME,
    ];
    for name in synthetic_order
        .into_iter()
        .filter(|name| {
            // Synthetic control tools may have been filtered out of
            // `core_control_tools` (e.g. subagents disabled, or Plan-only
            // tools in Build mode). In that case don't push them back
            // into the request via name lookup.
            *name == LOAD_TOOL_SCHEMA_TOOL_NAME || advertised_names.contains(name)
        })
        .chain(schema_config.core.iter().map(String::as_str))
    {
        push_tool_spec_by_name(tools, name, mode, plan_edit_allowed, &mut specs, &mut seen);
    }
    for name in loaded_tool_schemas {
        push_tool_spec_by_name(tools, name, mode, plan_edit_allowed, &mut specs, &mut seen);
    }
    specs
}

fn push_tool_spec_by_name(
    tools: &[AdvertisedTool],
    name: &str,
    mode: SessionMode,
    plan_edit_allowed: bool,
    specs: &mut Vec<Arc<LlmToolSpec>>,
    seen: &mut BTreeSet<String>,
) {
    if !seen.insert(name.to_string()) {
        return;
    }
    if let Some(tool) = synthetic_tool_by_name(name) {
        if !mode_refuses_capability(mode, tool.capability, plan_edit_allowed) {
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
    if !mode_refuses_capability(mode, tool.capability, plan_edit_allowed) {
        specs.push(Arc::clone(&tool.spec));
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
    plan_edit_allowed: bool,
) -> Option<String> {
    if !schema_config.lazy_schema_loading {
        return None;
    }
    let mut rows = tools
        .iter()
        .filter(|tool| {
            !mode_refuses_capability(mode, tool.capability, plan_edit_allowed)
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
    // prompt-prefix caching. Note: the Anthropic provider marks the last
    // tool definition with `cache_control: ephemeral` (see
    // `crates/squeezy-llm/src/anthropic.rs` `request_body`), so byte-stable
    // tool specs are load-bearing for that prefix cache as well.
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
    plan_edit_allowed: bool,
) -> String {
    match tool_schema_index(tools, mode, schema_config, plan_edit_allowed) {
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

/// Redact a single `LlmInputItem`. The `Redactor` is idempotent and
/// keeps a `Cow::Borrowed` until a pattern matches, so calling this on
/// an already-redacted item is allocation-free.
///
/// The conversation invariant is that every item stored in
/// `ConversationState::conversation` (or in the in-flight `conversation`
/// / `next_input` buffers within `TurnRuntime::run`) has already been
/// passed through this function, so the per-request build path never
/// needs to walk the conversation again to redact it.
fn redact_input_item(item: LlmInputItem, redactor: &Redactor) -> LlmInputItem {
    match item {
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
        LlmInputItem::FunctionCallOutput { call_id, output } => LlmInputItem::FunctionCallOutput {
            call_id,
            output: redactor.redact(&output).text,
        },
        // Reasoning payloads are model-signed blobs. Redacting the opaque
        // bytes would break replay; redact only the human-readable summary
        // fields so secrets that surface in the chain-of-thought are hidden
        // from the TUI without invalidating the signature.
        LlmInputItem::Reasoning(payload) => {
            LlmInputItem::Reasoning(redact_reasoning_payload(payload, redactor))
        }
    }
}

fn redact_reasoning_payload(payload: ReasoningPayload, redactor: &Redactor) -> ReasoningPayload {
    match payload {
        ReasoningPayload::OpenAi {
            item_id,
            summary,
            encrypted_content,
        } => ReasoningPayload::OpenAi {
            item_id,
            summary: summary
                .into_iter()
                .map(|text| redactor.redact(&text).text)
                .collect(),
            encrypted_content,
        },
        ReasoningPayload::Anthropic { blocks } => ReasoningPayload::Anthropic {
            blocks: blocks
                .into_iter()
                .map(|block| {
                    let text = if block.text.is_empty() {
                        block.text
                    } else {
                        redactor.redact(&block.text).text
                    };
                    squeezy_core::AnthropicThinkingBlock {
                        kind: block.kind,
                        text,
                        signature: block.signature,
                        data: block.data,
                    }
                })
                .collect(),
        },
        ReasoningPayload::Google {
            summary,
            thought_signature,
        } => ReasoningPayload::Google {
            summary: summary
                .into_iter()
                .map(|text| redactor.redact(&text).text)
                .collect(),
            thought_signature,
        },
    }
}

/// Normalize a vector of `LlmInputItem`s so every entry satisfies the
/// "already redacted" invariant and the conversation does not contain
/// any orphan `FunctionCallOutput` whose declaring `FunctionCall` is
/// missing. Used to upgrade conversation state loaded from a resume
/// tape that may pre-date either invariant (insertion-time redaction
/// or compaction's orphan-drop). The orphan check is a last-resort
/// safety net: OpenAI 400s the whole turn with *"No tool call found
/// for function call output with call_id …"* if an orphan reaches the
/// provider, and the failure is sticky.
fn redact_llm_input_items(input: Vec<LlmInputItem>, redactor: &Redactor) -> Vec<LlmInputItem> {
    let redacted: Vec<LlmInputItem> = input
        .into_iter()
        .map(|item| redact_input_item(item, redactor))
        .collect();
    drop_orphan_function_call_outputs(redacted)
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

pub(crate) fn log_session_event(
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

pub(crate) fn tool_result_summary(result: &ToolResult) -> String {
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

pub(crate) fn collapse_status_text(text: &str) -> String {
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

pub(crate) fn unix_timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn replay_hash(value: &impl Serialize) -> String {
    sha256_hex(serde_json::to_vec(value).unwrap_or_default())
}

/// Returns a stable LlmRequest snapshot for replay-hash purposes.
///
/// `cache_key` is derived from the live session id, which changes
/// across record/replay runs, so it must be excluded from the
/// divergence hash.
fn replay_request_view(request: &LlmRequest) -> LlmRequest {
    let mut view = request.clone();
    view.cache_key = None;
    view
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

pub(crate) fn llm_input_to_resume_item(item: LlmInputItem) -> ResumeItem {
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
        LlmInputItem::Reasoning(payload) => ResumeItem::Reasoning { payload },
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
        ResumeItem::Reasoning { payload } => LlmInputItem::Reasoning(payload),
    }
}

/// Combined token count from a `CostSnapshot`. Sums `input_tokens`,
/// `output_tokens`, and `reasoning_output_tokens` when present; falls back
/// to `None` if the provider reported no usage.
fn total_tokens_from_cost(cost: &CostSnapshot) -> Option<u64> {
    let mut total: u64 = 0;
    let mut saw_any = false;
    for value in [
        cost.input_tokens,
        cost.output_tokens,
        cost.reasoning_output_tokens,
    ]
    .into_iter()
    .flatten()
    {
        saw_any = true;
        total = total.saturating_add(value);
    }
    if saw_any { Some(total) } else { None }
}

/// Mirror of the gate inside `maybe_compact_mid_turn`. Returns `true`
/// when the configured threshold is crossed so the agent can fire a
/// `HookEvent::PreCompact` before the rewrite call. Kept here (rather
/// than in `context_compaction.rs`) because the hook fan-out is an
/// agent-loop concern; the function reads only public config and
/// estimator state so it stays a thin predicate.
fn mid_turn_compaction_will_fire(
    config: &AppConfig,
    conversation: &[LlmInputItem],
    last_total_tokens: Option<u64>,
) -> bool {
    if !config.context_compaction.enabled_mid_turn {
        return false;
    }
    let Some(window) = config.context_compaction.model_context_window else {
        return false;
    };
    if window == 0 {
        return false;
    }
    let threshold = window
        .saturating_mul(config.context_compaction.threshold_percent.min(100) as u64)
        .saturating_div(100);
    let observed = last_total_tokens
        .unwrap_or_else(|| estimate_context(conversation).estimated_tokens);
    observed >= threshold
}

pub(crate) fn compact_text(text: &str, max_chars: usize) -> String {
    truncate_chars(&collapse_status_text(text), max_chars)
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    [left, right].into_iter().flatten().reduce(|a, b| a + b)
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
    /// Snippet of the most recent assistant message (head-truncated to
    /// ~300 chars) so the approval dialog can show why the tool is
    /// being run. `None` when no assistant message is available (e.g.
    /// the very first turn or subagent contexts without a transcript).
    pub context: Option<String>,
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
    AllowSession,
    AllowRuleUser,
    AllowRuleProject,
    AskRuleUser,
    AskRuleProject,
    DenyOnce,
    DenySession,
    DenyRuleUser,
    DenyRuleProject,
    Cancelled,
}

enum ApprovalDecision {
    Approved,
    Denied(String),
    Cancelled,
}

/// Request payload sent to the TUI when the model calls
/// `request_user_input` from Plan mode. The TUI renders a modal, gathers
/// the user's choice, and replies via the matching
/// [`RequestUserInputResponse`] over a oneshot channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUserInputRequest {
    /// Question to display to the user.
    pub question: String,
    /// Optional multiple-choice options. Empty means "free-form only".
    pub choices: Vec<RequestUserInputChoice>,
    /// When true, the UI offers a free-form text path alongside any
    /// configured choices.
    pub allow_freeform: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUserInputChoice {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestUserInputAction {
    Choice,
    Freeform,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUserInputResponse {
    pub action: RequestUserInputAction,
    pub choice_value: Option<String>,
    pub freeform: Option<String>,
}

impl RequestUserInputResponse {
    pub fn choice(value: impl Into<String>) -> Self {
        Self {
            action: RequestUserInputAction::Choice,
            choice_value: Some(value.into()),
            freeform: None,
        }
    }

    pub fn freeform(text: impl Into<String>) -> Self {
        Self {
            action: RequestUserInputAction::Freeform,
            choice_value: None,
            freeform: Some(text.into()),
        }
    }

    pub fn cancelled() -> Self {
        Self {
            action: RequestUserInputAction::Cancelled,
            choice_value: None,
            freeform: None,
        }
    }
}

/// Structured result of [`Agent::dispatch_command`]. Designed to be
/// serializable for non-TUI consumers (e.g. eval traces); the TUI is
/// free to render its own version on top.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandOutcome {
    Compacted,
    ModeChanged { mode: String, changed: bool },
    CostSnapshot { debug: String },
    JobsList { count: usize },
    PermissionsList { count: usize },
    Unsupported { command: String },
    Error { command: String, message: String },
}

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
    /// Incremental reasoning/thinking tokens emitted by the model. Rendered
    /// in the TUI as a grey transient block; not part of the visible
    /// assistant message.
    ReasoningDelta {
        turn_id: TurnId,
        delta: String,
    },
    /// A reasoning block has finished streaming. Carries the provider-tagged
    /// payload so the TUI can store the segment as its own collapsible
    /// transcript entry and clear the live "thinking..." buffer before the
    /// next block (or tool call, or text) starts.
    ReasoningSegment {
        turn_id: TurnId,
        snapshot: ReasoningSnapshot,
    },
    ToolCallQueued {
        turn_id: TurnId,
        call: ToolCall,
    },
    ToolCallStarted {
        turn_id: TurnId,
        call: ToolCall,
        /// Whether the call comes from the planner preflight, the model
        /// itself, or a subagent. Lets transcript renderers swap icons
        /// (🧭 / 🔧 / 🤖) and lets findings attribute hits correctly.
        origin: ToolOrigin,
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
    RequestUserInputRequested {
        turn_id: TurnId,
        request: RequestUserInputRequest,
        response_tx: oneshot::Sender<RequestUserInputResponse>,
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
    AiReviewerTripped {
        turn_id: TurnId,
        reason: String,
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
        /// Post-turn estimate of the conversation footprint, used by the
        /// TUI to update its context-budget indicator without needing a
        /// follow-up `context_estimate_snapshot()` call.
        context_estimate: ContextEstimate,
    },
    /// Emitted at most once per session, the first time the running provider
    /// cost crosses `cost_warn_percent` of the configured
    /// `max_session_cost_usd_micros` cap. The TUI renders a transcript
    /// notice; non-TUI consumers (replay tooling, telemetry) can ignore it.
    CostWarning {
        turn_id: TurnId,
        status: CostCapStatus,
    },
    /// Emitted at most once per session, the first time the shell tool's OS
    /// sandbox backend silently degrades to the best_effort path (probe
    /// failure, runtime sandbox_apply error, etc.). The TUI surfaces a
    /// warning so users see the degradation; the per-call telemetry counter
    /// `approval.best_effort.fallback{tool=shell}` keeps ticking on every
    /// fallback for backend dashboards.
    ShellSandboxBestEffortFallback {
        turn_id: TurnId,
        backend: String,
        fallback_count: u64,
    },
    /// Per-turn progress callout emitted every few tool calls so a user
    /// watching a live transcript can see cost accumulating before the
    /// turn finishes. Carries the turn's running input-token count and
    /// estimated USD-micro cost so far; consumers (eval, TUI) render
    /// it inline.
    CostUpdate {
        turn_id: TurnId,
        tool_count: u64,
        input_tokens: u64,
        micro_usd: u64,
    },
    /// Periodic heartbeat while a single tool call is still running.
    /// Emitted on a fixed interval (see `TOOL_PROGRESS_INTERVAL`) so a
    /// watcher can tell a long-running tool apart from a hung one.
    ToolProgress {
        turn_id: TurnId,
        call_id: String,
        tool_name: String,
        elapsed_ms: u64,
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
