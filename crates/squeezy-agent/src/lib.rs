use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    env,
    fmt::Write as _,
    fs,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, RwLock,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextAttachmentKind, ContextAttachmentSource,
    ContextAttachmentStatus, ContextCompactionRecord, ContextCompactionState,
    ContextCompactionTrigger, ContextEstimate, ContextPin, CostOrigin, CostSnapshot,
    DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES, DEFAULT_OLLAMA_MODEL, ModelTier, PROJECT_SETTINGS_FILE,
    PermissionAction, PermissionCapability, PermissionPolicyMode, PermissionRequest,
    PermissionRisk, PermissionRule, PermissionRuleSource, PermissionScope, PermissionVerdict,
    ProviderConfig, Redactor, ResponseVerbosity, Role, SessionMetrics, SessionMode, SqueezyError,
    StreamRedactor, SubagentConfig, TaskStateSnapshot, TaskStateStatus, ToolSchemaConfig,
    TranscriptItem, TurnId, TurnMetrics, context_attachment_preview,
    context_attachment_storage_text, default_settings_path, detect_context_attachment_kind,
};
use squeezy_hooks::{HookPayload, HookRegistry, HookResult};
use squeezy_llm::{
    CONTEXT_1M_BETA, CacheRetention, CacheSpec, CitationSource, ContextLimitInput,
    INTERLEAVED_THINKING_BETA, INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY,
    INVALID_TOOL_ARGUMENTS_RAW_KEY, LlmEvent, LlmInputItem, LlmOutputSchema, LlmProvider,
    LlmRequest, LlmStream, LlmToolCall, LlmToolSpec, ReasoningPayload, ReasoningSnapshot,
    RequestTokenEstimate, StopReason, capabilities_for, estimate_cost,
    estimate_request_context_full, fetch_ollama_context_window, provider_honors_output_schema,
};
use squeezy_skills::{
    DocSection, HelpAnswer, HelpCitation, HelpStatus, SkillActivationKind, SqueezyHelp,
    matches_squeezy_help_input, relevant_doc_sections_for_input,
};
use squeezy_store::{
    BugReportBundle, BugReportOptions, HydratedTranscriptItem, ResumeItem, SessionEvent,
    SessionEventKind, SessionHandle, SessionMetadata, SessionQuery, SessionRecord,
    SessionReplayEvent, SessionReplayEventKind, SessionReplayTape, SessionResumeState,
    SessionStatus, SessionStore, SqueezyStore,
};
use squeezy_telemetry::{
    ConfigChangeReport, ErrorKind, FeedbackClient, FeedbackSubmitResult, HelpAnswerRatedReport,
    HelpAnswerSourceKind, HelpRatingKind, McpDiscoveryReport, PreparedFeedback, ProviderErrorKind,
    ReportUpload, SessionStatusKind as TelemetrySessionStatusKind, SessionTelemetryReport,
    SkillActivationReport, SlashAliasKind, SlashArgShape, SlashOutcome, SlashSurface,
    SlashTelemetryReport, StartupRoute, TelemetryClient, TelemetryEvent, ToolCostProperties,
    ToolStatusKind as TelemetryToolStatusKind, ToolTelemetryReport, WebRequestReport,
    prepare_feedback,
};
use squeezy_tools::{
    McpElicitationHandler, McpElicitationRequest, McpElicitationResponse, McpStatusSnapshot,
    PlanModeShellSafety, ShellAskApprover, ShellAskDecision, ShellAskRequest,
    ShellBestEffortFallback, ShellPreClassification, ShellWindowsDegraded, ToolCall, ToolCostHint,
    ToolExecutionOptions, ToolOutputConfig, ToolReceipt, ToolRegistry, ToolRegistryRuntime,
    ToolResult, ToolRuntimeConfig, ToolSpec, ToolStatus, WebToolConfig,
    classify_plan_mode_shell_command, pre_classify_shell, sha256_hex,
    shell_best_effort_fallback_from_result, shell_windows_degraded_from_result,
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
pub mod dispatch;
mod exploration_compiler;
pub mod export_html;
mod memory_extraction;
mod micro_compaction;
mod permission_persist;
mod plan_mode;
mod roles;
pub mod subagent_catalog;
mod turn_phases;
mod turn_router;

use turn_phases::request::{
    effective_tool_choice, request_beta_headers, request_reasoning_effort,
    request_reasoning_effort_for_tier, request_response_verbosity, subagent_role_reasoning_effort,
};
use turn_phases::stream::next_llm_stream_event;
use turn_phases::tools::execute_tool_calls;

pub use dispatch::{
    CompactSubcommand, DispatchCommand, DispatchCommandKind, DispatchCommandParseError,
    DispatchOutcome,
};

use cancel::{CancelErr, OrCancelExt};
#[cfg(test)]
use context_compaction::build_compaction_summary;
use context_compaction::{
    PendingToolResult, SeenToolOutputs, compact_conversation, compact_conversation_with_strategy,
    context_compaction_decision, drop_orphan_function_call_outputs, estimate_context,
    estimated_tokens, maybe_compact_conversation, next_context_pin_id, pack_tool_results,
    repair_orphan_function_calls,
};
use cost_broker::{
    CostBroker, format_cap_reached_reason, format_pressure_gate_reason,
    format_round_input_gate_reason, llm_request_input_bytes, llm_request_overhead_bytes,
    round_input_gate_status,
};
use exploration_compiler::{ExplorationTurnState, compile_exploration_plan};
use micro_compaction::{SuccessfulEdit, mask_expired_reads_after_edits, maybe_micro_compact};
use permission_persist::persist_permission_rule;
use roles::{RoleModelPolicy, SubagentRole, role_config};

pub use ai_reviewer::{ReviewerAuditEntry, ReviewerAuditVerdict};
pub use context_compaction::ContextCompactionReport;
pub use cost_broker::{
    CostCapStatus, format_cap_unenforceable_notice, format_warn_threshold_notice,
};
pub use export_html::{ExportError, ExportOpts, ExportTheme, export_session_to_html};
pub use plan_mode::{PROPOSED_PLAN_CLOSE_TAG, PROPOSED_PLAN_OPEN_TAG, strip_proposed_plan_blocks};
pub use subagent_catalog::{
    PROJECT_SUBAGENTS_DIR, SubagentCatalog, SubagentDefinition, SubagentSource, USER_SUBAGENTS_DIR,
};

// Emergency belt on tool rounds per turn. 200 keeps a true safety
// ceiling without truncating legitimate long-running exploration.
const MAX_TOOL_ROUNDS: usize = 200;
const MAX_PAUSE_TURN_REISSUES: usize = 2;
const MAX_CONTROL_ONLY_TOOL_ROUNDS: usize = 2;
const LOCAL_SHELL_TIMEOUT_MS: u64 = 10_000;
const LOCAL_SHELL_OUTPUT_BYTE_CAP: usize = 32_000;
const TASK_STATE_TOOL_NAME: &str = "update_task_state";
const LOAD_TOOL_SCHEMA_TOOL_NAME: &str = "load_tool_schema";
const DELEGATE_TOOL_NAME: &str = "delegate";
const EXPLORE_TOOL_NAME: &str = "explore";
const DELEGATE_PLAN_TOOL_NAME: &str = "delegate_plan";
const DELEGATE_REVIEW_TOOL_NAME: &str = "delegate_review";
const DELEGATE_CHAIN_TOOL_NAME: &str = "delegate_chain";
const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";
/// Placeholder substituted in each chain step's prompt with the prior
/// step's summary. Documented here so the constant is the single source
/// of truth for both the tool description and the runtime substitution.
const DELEGATE_CHAIN_PREVIOUS_PLACEHOLDER: &str = "{previous}";
/// Hard cap on the number of steps a single `delegate_chain` call may
/// declare. Each step burns a full subagent lease + LLM round, so the
/// chain is intentionally narrower than the parent agent's per-turn tool
/// budget. A modest cap is enough to thread a non-trivial multi-stage
/// research workflow without letting the model commit the entire turn
/// budget to one chain.
const DELEGATE_CHAIN_MAX_STEPS: usize = 16;
/// Anti-redundant-delegation gate. A whole-task `delegate` is refused once the
/// parent has ALREADY pulled substantial context for the task in-context,
/// because the cold subagent starts with an empty conversation + empty
/// read-dedup store and re-reads the very files the parent already holds — pure
/// double-work (measured: a parent that grep/read-storms 20+ calls then
/// delegates pays the subagent to re-derive the same findings). Keyed on the
/// parent's own exploration magnitude (turn-spanning, parent-only metrics), NOT
/// on a delegate count: a context-isolating delegate fired *before* the parent
/// explores has both counters near zero and is intentionally exempt. Only the
/// broad `Delegate` kind is gated; scoped `delegate_plan`/`delegate_review`
/// pass through.
const REDUNDANT_DELEGATE_EXPLORE_CALLS: u64 = 8;
const REDUNDANT_DELEGATE_READ_BYTES: u64 = 32_768;
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
#[allow(dead_code)]
const SUBAGENT_MAX_CONCURRENT: usize = squeezy_core::DEFAULT_SUBAGENT_MAX_CONCURRENT;
// Compaction summary truncation budget — survivor policy chunk for the
// SUMMARY_BLOCK family. Sister budgets live in `context_compaction.rs`;
// this one stays here because it is *also* used by
// `instructions_with_pinned_context` to bound the per-turn pinned block
// inserted into the live instructions, not just the compaction summary.
//
/// Cap on a single pin's summary text. ≈ 100 tokens — wide enough for a
/// one-paragraph user-pinned reminder, narrow enough that a dozen pins fit
/// inside `model_context_window * threshold_percent` without crowding out
/// the live conversation.
pub(crate) const COMPACTION_PIN_SUMMARY_MAX_CHARS: usize = 400;

#[derive(Debug, Clone)]
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
    calibration_source: CalibrationSource,
    /// Mirror of the per-turn router's sticky-window counter, kept
    /// in sync with `Agent::routing_state.sticky.remaining_turns` so
    /// that every existing `to_resume_state()` call site persists the
    /// router's cross-turn state without a parallel plumbing change.
    /// Read back into the live router on `Agent::resume`.
    routing_sticky_remaining_turns: u8,
    routing_session_disabled: bool,
    routing_prior_turn_was_hard: bool,
    /// Per-`(provider, model)` observed context ceiling: the estimated input
    /// size at which the provider last returned a context-window-exceeded
    /// error. Clamps the resolved window down for that route for the rest of
    /// the session (so `/context` and the reroute fit-check stop trusting an
    /// over-optimistic catalog/override). In-memory only — a best-effort safety
    /// signal, not persisted across resume.
    observed_context_ceilings: HashMap<(String, String), u64>,
}

impl Default for ConversationState {
    fn default() -> Self {
        Self {
            previous_response_id: None,
            conversation: Vec::new(),
            transcript: Vec::new(),
            context_attachments: Vec::new(),
            context_compaction: ContextCompactionState::default(),
            cost: CostSnapshot::default(),
            metrics: SessionMetrics::default(),
            redactions: 0,
            token_calibration: squeezy_llm::TokenCalibration::default(),
            calibration_source: CalibrationSource::HardCodedDefault,
            routing_sticky_remaining_turns: 0,
            routing_session_disabled: false,
            routing_prior_turn_was_hard: false,
            observed_context_ceilings: HashMap::new(),
        }
    }
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
            calibration_source: CalibrationSource::ResumedSession,
            routing_sticky_remaining_turns: state.routing_sticky_remaining_turns,
            routing_session_disabled: state.routing_session_disabled,
            routing_prior_turn_was_hard: state.routing_prior_turn_was_hard,
            observed_context_ceilings: HashMap::new(),
        }
    }

    fn routing_sticky_remaining_turns(&self) -> u8 {
        self.routing_sticky_remaining_turns
    }

    fn set_routing_sticky_remaining_turns(&mut self, value: u8) {
        self.routing_sticky_remaining_turns = value;
    }

    fn routing_session_disabled(&self) -> bool {
        self.routing_session_disabled
    }

    fn set_routing_session_disabled(&mut self, disabled: bool) {
        self.routing_session_disabled = disabled;
    }

    fn routing_prior_turn_was_hard(&self) -> bool {
        self.routing_prior_turn_was_hard
    }

    fn set_routing_prior_turn_was_hard(&mut self, hard: bool) {
        self.routing_prior_turn_was_hard = hard;
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
            // The live `ConversationState` doesn't track the
            // hydrated-transcript shape because we don't need it
            // for the LLM context — it's a UI concern. Persist it
            // empty here; `Agent::resume` will detect "snapshot
            // has transcript but no hydrated_transcript" and
            // rebuild via `replay_resume_state` (which walks
            // events.jsonl and produces both forms in one shot).
            hydrated_transcript: Vec::new(),
            context_attachments: self.context_attachments.clone(),
            context_compaction: self.context_compaction.clone(),
            routing_sticky_remaining_turns: self.routing_sticky_remaining_turns,
            routing_session_disabled: self.routing_session_disabled,
            routing_prior_turn_was_hard: self.routing_prior_turn_was_hard,
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
                    // Replay logs predate the stop_reason surface; missing
                    // values stay `None` so older tapes remain readable.
                    let stop_reason = serde_json::from_value::<StopReason>(
                        event
                            .payload
                            .get("stop_reason")
                            .cloned()
                            .unwrap_or(Value::Null),
                    )
                    .ok();
                    let reasoning_only_stop = event
                        .payload
                        .get("reasoning_only_stop")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    events.push(LlmEvent::Completed {
                        response_id,
                        cost,
                        stop_reason,
                        reasoning_only_stop,
                    });
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
    pub image_items: usize,
    pub text_bytes: usize,
    pub tool_output_bytes: usize,
    /// Subset of `tool_output_bytes` produced by `load_skill` calls — the skill
    /// bodies materialized into the transcript. Carved out of tool outputs and
    /// reported as the "skills" bucket in `/context`.
    pub skill_output_bytes: usize,
    pub reasoning_bytes: usize,
    pub image_bytes: usize,
}

/// One discovered skill's accounting entry for the `/context` "Skills" section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillAccountingEntry {
    pub name: String,
    /// First line of the skill description.
    pub description: String,
    /// `true` when the skill body is currently materialized in this session.
    pub loaded: bool,
    /// Byte size of the always-present metadata block (no body).
    pub metadata_bytes: usize,
    /// Body byte size: exact for loaded skills (in context now), on-disk
    /// `SKILL.md` size otherwise (the cost a first load would add).
    pub body_bytes: usize,
}

/// Skill catalog accounting for `/context`. Totals split the always-present
/// metadata cost from the body cost that only loaded skills contribute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsAccounting {
    pub discovered: usize,
    pub loaded: usize,
    pub entries: Vec<SkillAccountingEntry>,
    /// Sum of every discovered skill's metadata block.
    pub metadata_bytes_total: usize,
    /// Sum of loaded skills' bodies (materialized in context).
    pub loaded_body_bytes_total: usize,
}

/// One MCP tool's accounting entry for the `/context` "MCPs" section. MCP tools
/// are lazily loaded: `stub_bytes` (tool-index line) is always present, and the
/// full schema (`full_bytes`) is attached only after first load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpToolAccountingEntry {
    pub name: String,
    pub description: String,
    /// Tool-index stub line cost (initial lazy cost; 0 when lazy loading off).
    pub stub_bytes: usize,
    /// Full schema cost — the delta a first load adds (or always-on when lazy
    /// loading is disabled).
    pub full_bytes: usize,
    /// `true` when the full schema is live in the request (loaded this session,
    /// or always-on when lazy loading is disabled).
    pub loaded: bool,
}

/// One MCP server's accounting for `/context`, grouping its tools.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpServerAccounting {
    pub name: String,
    /// Human-readable status: `ready`, `starting`, `failed: …`, `cancelled`,
    /// or `configured` when no live status is reported yet.
    pub status: String,
    pub tools: Vec<McpToolAccountingEntry>,
    /// Sum of this server's stub lines.
    pub stub_bytes: usize,
    /// Sum of this server's live full schemas (loaded tools).
    pub loaded_full_bytes: usize,
    /// Live in-context cost: `stub_bytes + loaded_full_bytes`.
    pub in_context_bytes: usize,
}

/// MCP accounting for `/context`. `in_context_bytes_total` is the live
/// request-framing cost (stub lines + loaded full schemas), carved out of
/// "system + framing".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpAccounting {
    pub servers: Vec<McpServerAccounting>,
    pub total_tools: usize,
    /// Whether lazy schema loading is active (stubs in play).
    pub lazy: bool,
    pub stub_bytes_total: usize,
    pub loaded_full_bytes_total: usize,
    pub in_context_bytes_total: usize,
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

/// Where the active `TokenCalibration` came from at session start. Shown by
/// `/cost` so users in CI / containers understand whether token estimates are
/// warm (from prior sessions) or cold (first run / shared home / corrupt file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationSource {
    /// calibration.json was absent; estimates use hard-coded provider defaults.
    HardCodedDefault,
    /// calibration.json was present but malformed; fell back to defaults.
    CorruptFallback,
    /// Loaded from the global calibration.json warm-start file.
    GlobalFile,
    /// Loaded from resumed session metadata (most accurate warm-start).
    ResumedSession,
}

impl CalibrationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HardCodedDefault => "hard-coded default (no calibration file)",
            Self::CorruptFallback => "hard-coded default (calibration file was malformed)",
            Self::GlobalFile => "global calibration.json",
            Self::ResumedSession => "resumed session metadata",
        }
    }
}

/// Snapshot of the configured budget policy for display in `/cost`. Bundles
/// all enforcement limits into one place so users can see every active
/// constraint without reading config files.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BudgetPolicySnapshot {
    pub max_session_cost_usd_micros: Option<u64>,
    pub cost_warn_percent: u8,
    pub max_round_input_tokens: Option<u64>,
    pub max_tool_calls_per_turn: u64,
    pub max_tool_bytes_read_per_turn: u64,
    pub max_search_files_per_turn: u64,
    pub disable_prompt_cache: bool,
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
    pub skills: SkillsAccounting,
    pub mcp: McpAccounting,
    /// Where the token calibration was loaded from at session start.
    pub calibration_source: CalibrationSource,
    pub budget_policy: BudgetPolicySnapshot,
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

/// Typed reason the registry refused a `start()` call. Carries the
/// `limit`/`active` counts so callers can render a "5 of 4 already
/// running" warning rather than a flat string, and `as_message` is the
/// canonical user-visible rendering used in tool results and session
/// receipts so offline replayers see a single stable phrasing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubagentStartError {
    reason: SubagentRejectionReason,
    limit: usize,
    active: usize,
}

impl SubagentStartError {
    fn as_message(&self) -> String {
        match self.reason {
            SubagentRejectionReason::ConcurrencyCap => format!(
                "subagent concurrency limit reached ({}; {} already running)",
                self.limit, self.active
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRejectionReason {
    ConcurrencyCap,
}

impl SubagentRejectionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConcurrencyCap => "concurrency_cap",
        }
    }

    /// Human-readable phrasing for the TUI pane row and transcript, as
    /// opposed to `as_str`'s machine token reserved for logs and
    /// structured/session-log fields.
    pub fn as_human(self) -> &'static str {
        match self {
            Self::ConcurrencyCap => "concurrency cap reached",
        }
    }
}

impl SubagentRegistry {
    fn start(
        &self,
        role: SubagentRole,
        cancel: CancellationToken,
        max_concurrent: usize,
        status: impl Into<String>,
    ) -> Result<SubagentLease, SubagentStartError> {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        let active = state
            .values()
            .filter(|metadata| !metadata.cancel.is_cancelled())
            .count();
        let limit = max_concurrent.max(1);
        if active >= limit {
            return Err(SubagentStartError {
                reason: SubagentRejectionReason::ConcurrencyCap,
                limit,
                active,
            });
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

/// Shared, mutex-protected cache for the Ollama live context-window probe.
/// Uses a type alias to satisfy the `type_complexity` lint.
type OllamaWindowCache = Arc<tokio::sync::Mutex<Option<(Instant, Option<u64>)>>>;

/// A point-in-time snapshot of the agent's mode and routing state.
///
/// Returned by [`Agent::mode_state_snapshot`]. Intended for the TUI status
/// line and tests to read from a single authoritative source rather than
/// piecing together routing state from multiple fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeStateSnapshot {
    /// The current session mode (`Plan` or `Build`).
    pub session_mode: squeezy_core::SessionMode,
    /// Whether session-wide auto-routing to the cheap tier is disabled
    /// (`/router off`). Does not affect an explicit `/cheap` one-shot.
    pub routing_session_disabled: bool,
    /// A one-shot `/cheap` override is pending for the next turn.
    pub pending_force_cheap: bool,
    /// A one-shot `/parent` override is pending for the next turn.
    pub pending_force_parent: bool,
    /// Number of sticky-escalation turns remaining (parent model forced
    /// after a mid-turn escalation). Zero means no active sticky window.
    pub sticky_turns_remaining: u8,
}

#[derive(Clone)]
pub struct Agent {
    /// Operator/session config in the same shape produced by settings loading.
    /// This intentionally excludes runtime-derived instruction additions
    /// (skills preamble, AGENTS.md, memory guidance) and registry-derived
    /// context windows, so settings reload comparisons do not mistake derived
    /// state for a disk edit.
    source_config: AppConfig,
    /// Runtime config used when assembling requests. This includes derived
    /// instruction additions and the active model's derived context window.
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    jobs: JobRegistry,
    telemetry: TelemetryClient,
    session_started_at: Instant,
    /// Metrics snapshot captured at agent-build time from `metadata.metrics`
    /// when the agent is constructed via [`Agent::resume_with_telemetry`].
    /// Zero for fresh sessions. Subtracted from the final `SessionMetrics`
    /// in `finish_session` so that `session_ended` reports only the delta
    /// contributed by this process run, preventing cumulative overcounting
    /// across resumptions.
    prior_metrics: SessionMetrics,
    redactor: Arc<Redactor>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
    session_log: Option<SessionHandle>,
    conversation_state: Arc<Mutex<ConversationState>>,
    active_turn: Arc<StdMutex<Option<ActiveTurn>>>,
    ai_reviewer_state: Arc<StdMutex<ai_reviewer::AiReviewerState>>,
    next_turn_id: Arc<AtomicU64>,
    next_approval_id: Arc<AtomicU64>,
    next_attachment_id: Arc<AtomicU64>,
    /// High-water mark into `conversation_state.transcript` for the automatic
    /// memory-extraction pass: items before it have already been considered.
    /// In-memory only (resets on restart → one harmless re-consideration of the
    /// resumed transcript).
    last_extracted_memory_len: Arc<AtomicUsize>,
    /// One-line summaries of what the automatic extraction pass saved/removed,
    /// queued here because the pass finishes *after* the turn's event channel
    /// closes. The TUI drains this on its poll loop and renders each as a quiet
    /// transcript line (same pattern as background-job notifications).
    memory_notices: Arc<StdMutex<Vec<String>>>,
    subagents: SubagentRegistry,
    /// Disk-loaded custom subagent definitions discovered at session start
    /// from `<ws>/.squeezy/agents/*.md` and `~/.squeezy/agents/*.md`. Shared
    /// with each `TurnRuntime` so the delegate dispatch can resolve an
    /// explicit `agent:` selection and the tool schema can advertise the
    /// available agents by name.
    subagent_catalog: Arc<SubagentCatalog>,
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
    /// Agent-level broadcast channel for events that originate outside a
    /// turn's per-call `mpsc::Sender<AgentEvent>`. The canonical use is
    /// manual `/compact` (`compact_context_manual`), which runs between
    /// turns and so has no per-turn sender to reach TUI overlays, eval
    /// capture, or MCP listeners on. Events are wrapped in `Arc` because
    /// `AgentEvent` contains non-`Clone` variants (oneshot senders);
    /// subscribers receive cheap `Arc<AgentEvent>` clones.
    event_broadcast: broadcast::Sender<Arc<AgentEvent>>,
    /// Background tasks spawned from within the agent — currently just
    /// the MCP tool-palette refresh fired during `start_turn`. Tracking
    /// them lets [`Agent::shutdown`] join the spawns before returning so
    /// callers that need the underlying `Arc<SqueezyStore>` released
    /// (e.g. a test that reopens the redb file) don't race the
    /// fire-and-forget lifetime of these tasks. New tasks may still be
    /// registered after a shutdown completes; the JoinSet is reusable.
    background_tasks: Arc<StdMutex<tokio::task::JoinSet<()>>>,
    /// Ordered gate for background MCP config actions. `/mcp` key handling
    /// must return immediately, but live registry mutations still need the
    /// old sequential semantics so rapid toggle/restart/add/remove actions
    /// settle in the same order the user requested them.
    mcp_background_queue: Arc<McpBackgroundQueue>,
    /// Root cancellation token for agent-lifetime background tasks. Cancelling
    /// the current token bounds MCP reload/toggle/restart tasks so they cannot
    /// hold tool-registry or store handles across `Agent::shutdown`. The token
    /// is renewed after shutdown because the `Agent` remains reusable.
    shutdown_token: Arc<StdMutex<CancellationToken>>,
    /// Cross-turn state for the per-turn model router. Tracks the
    /// escalation-sticky window and any pending `/cheap` / `/parent` /
    /// `/router` user override. Shared with each `TurnRuntime` via
    /// `Arc<StdMutex<_>>` so the streaming loop can engage the sticky
    /// window after an escalation and the next `start_turn` picks it up.
    routing_state: Arc<StdMutex<turn_router::RoutingPersistentState>>,
    /// Tokens of fixed request overhead — system instructions plus serialized
    /// tool schemas — measured on the most recent assembled request and carried
    /// into the next turn's post-turn compaction gate. `estimate_context` only
    /// walks conversation items, so without this the gate under-counts the real
    /// input size on tool-heavy configs (finding #2).
    last_request_overhead_tokens: Arc<AtomicU64>,
    /// The explicitly-configured `model_context_window` (from `squeezy.toml` or
    /// `SQUEEZY_CONTEXT_MODEL_CONTEXT_WINDOW`), captured *before* `build()`
    /// auto-derives a value from the model registry. A runtime model switch
    /// re-derives the window for the new model via `re_derive_model_context_window`
    /// while still letting an explicit override win (finding #1).
    configured_model_context_window: Option<u64>,
    /// Short-lived cache for the Ollama live context-window probe result.
    /// `session_accounting_snapshot()` fires a blocking HTTP call for Ollama
    /// providers; caching avoids repeated network probes when `/cost` or
    /// `/context` is invoked in quick succession.
    ollama_window_cache: OllamaWindowCache,
}

#[derive(Clone)]
struct ActiveTurn {
    turn_id: TurnId,
    cancel: CancellationToken,
}

#[derive(Default)]
struct McpBackgroundQueue {
    next_ticket: AtomicU64,
    serving: AtomicU64,
    notify: Notify,
}

impl McpBackgroundQueue {
    fn issue_ticket(&self) -> u64 {
        self.next_ticket.fetch_add(1, Ordering::AcqRel)
    }

    async fn wait_for_turn(&self, ticket: u64) {
        loop {
            if self.serving.load(Ordering::Acquire) == ticket {
                return;
            }
            self.notify.notified().await;
        }
    }

    fn finish_turn(&self) {
        self.serving.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }
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

/// Resolve the compaction window for the active model through the layered limit
/// resolver. Shared by `build()` (initial derivation) and
/// `re_derive_model_context_window` (runtime model switch) so both compute the
/// window the same way (finding #1) and so compaction sizing matches the
/// `/context` accounting window.
///
/// `global_override` is the operator's explicit *global*
/// `[context].model_context_window` (captured before the field is overwritten);
/// a per-model `[model_limits."p:m"]` entry takes precedence over it.
///
/// Returns `None` for a low-confidence (synthetic-fallback) window so mid-turn
/// compaction stays dormant rather than arming off a blanket 272K guess — the
/// "dormant when underivable" contract. Curated/models.dev/override windows arm
/// it.
fn derive_model_context_window(
    config: &AppConfig,
    provider: &dyn LlmProvider,
    global_override: Option<u64>,
) -> Option<u64> {
    let per_model = config
        .model_limits
        .get(&config.model_limit_key())
        .and_then(|entry| entry.context_window);
    let mut input = ContextLimitInput::new(provider.name(), &config.model);
    input.user_override = per_model.or(global_override);
    input.models_dev = squeezy_llm::cached_models_dev_view();
    input.effective_percent_override = config.context_compaction.effective_context_window_percent;
    input.baseline_reserve_override = config.context_compaction.baseline_reserve_tokens;
    let resolved = squeezy_llm::resolve_context_limits(&input);
    if matches!(resolved.confidence, squeezy_llm::LimitConfidence::Low) {
        None
    } else {
        resolved.context_window_tokens
    }
}

/// Whether `model`'s effective context window can hold the assembled
/// `conversation` plus the projected output, used by the reroute fit-check.
/// Resolves the target model's window through the same layered resolver
/// (per-model override → global → curated → models.dev → observed clamp), so a
/// reroute to a smaller-window model is only allowed when it fits *as-is* — we
/// never compact to squeeze into a cheaper model. Returns `true` (permissive)
/// when the window is only a low-confidence guess: a real overflow there is
/// caught by the mid-turn escalation + observed-ceiling path instead of being
/// pre-emptively skipped on a guess.
fn model_fits_conversation(
    config: &AppConfig,
    provider_name: &str,
    global_override: Option<u64>,
    model: &str,
    conversation: &[LlmInputItem],
    observed_ceiling: Option<u64>,
) -> bool {
    let key = format!(
        "{}:{}",
        squeezy_core::provider_slug(&config.provider),
        model
    );
    let per_model = config
        .model_limits
        .get(&key)
        .and_then(|entry| entry.context_window);
    let mut input = ContextLimitInput::new(provider_name, model);
    input.user_override = per_model.or(global_override);
    input.observed_ceiling = observed_ceiling;
    input.models_dev = squeezy_llm::cached_models_dev_view();
    input.effective_percent_override = config.context_compaction.effective_context_window_percent;
    input.baseline_reserve_override = config.context_compaction.baseline_reserve_tokens;
    let resolved = squeezy_llm::resolve_context_limits(&input);
    if matches!(resolved.confidence, squeezy_llm::LimitConfidence::Low) {
        return true;
    }
    let Some(effective) = squeezy_llm::effective_window_tokens(&resolved) else {
        return true;
    };
    let estimated_input = estimate_context(conversation).estimated_tokens;
    let projected_output =
        CostBroker::projected_output_tokens(config.max_output_tokens, resolved.max_output_tokens);
    estimated_input.saturating_add(projected_output) <= effective
}

impl Agent {
    pub fn new(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Self {
        let session_log = start_session_log(&config, provider.name());
        Self::new_with_session_log(config, provider, session_log)
    }

    pub fn new_with_telemetry(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        telemetry: TelemetryClient,
    ) -> Self {
        let session_log = start_session_log(&config, provider.name());
        Self::new_with_session_log_and_telemetry(config, provider, session_log, telemetry)
    }

    /// Build an agent without opening a durable session log.
    ///
    /// This is for local harnesses that need agent state transitions but do
    /// not need a resumable session or session metadata on disk.
    pub fn new_ephemeral(config: AppConfig, provider: Arc<dyn LlmProvider>) -> Self {
        Self::new_with_session_log(config, provider, None)
    }

    fn new_with_session_log(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_log: Option<SessionHandle>,
    ) -> Self {
        let telemetry = TelemetryClient::from_config(&config);
        Self::new_with_session_log_and_telemetry(config, provider, session_log, telemetry)
    }

    fn new_with_session_log_and_telemetry(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_log: Option<SessionHandle>,
        telemetry: TelemetryClient,
    ) -> Self {
        // Fresh sessions inherit the most-recent cross-session calibration so
        // the first round's estimator isn't stuck on per-provider defaults.
        // Missing or malformed files fall back to `TokenCalibration::default()`,
        // which is what `ConversationState::default()` would carry anyway.
        let store = SessionStore::open(&config);
        let (token_calibration, source_hint) = store.load_global_calibration_with_source_hint();
        let calibration_source = match source_hint {
            None => CalibrationSource::HardCodedDefault,
            Some(true) => CalibrationSource::GlobalFile,
            Some(false) => CalibrationSource::CorruptFallback,
        };
        let conversation_state = ConversationState {
            token_calibration,
            calibration_source,
            ..ConversationState::default()
        };
        Self::build(
            config,
            provider,
            session_log,
            conversation_state,
            None,
            telemetry,
            SessionMetrics::default(),
        )
    }

    pub fn resume(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_id: &str,
    ) -> squeezy_core::Result<(Self, Vec<HydratedTranscriptItem>)> {
        let telemetry = TelemetryClient::from_config(&config);
        Self::resume_with_telemetry(config, provider, session_id, telemetry)
    }

    pub fn resume_with_telemetry(
        config: AppConfig,
        provider: Arc<dyn LlmProvider>,
        session_id: &str,
        telemetry: TelemetryClient,
    ) -> squeezy_core::Result<(Self, Vec<HydratedTranscriptItem>)> {
        let store = SessionStore::open(&config);
        // The resolver and metadata reader see archived sessions, but the
        // writer and `SessionHandle::dir` use the live root only. Revive an
        // archived session back into the live tree before opening it so
        // resume reads/writes a real directory instead of a phantom live
        // path and failing with a misleading "not resumable" error.
        if store.is_archived(session_id) {
            store.unarchive_session(session_id)?;
        }
        let handle = store.open_session(session_id.to_string());
        // Prefer the durable snapshot, but fall back to replaying
        // events.jsonl when:
        //   - `resume_state.json` is missing / corrupt / non-resumable,
        //   - or the snapshot pre-dates hydrated-transcript support
        //     (`hydrated_transcript` empty alongside a non-empty
        //     `transcript`). The live `ConversationState` no longer
        //     persists `hydrated_transcript` because it's a UI
        //     concern, so the snapshot only knows the rich shape
        //     when it came straight from `replay_resume_state`.
        //     The event log is appended on every turn, so it
        //     survives both a crash that ate the snapshot AND a
        //     pre-hydrated binary that wrote a thin one.
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
        // The snapshot is authoritative for the LLM-facing conversation,
        // but the live `ConversationState` does not track the hydrated
        // (tool-result-bearing) transcript shape — that's a UI concern.
        // When the snapshot omits it, run an events.jsonl replay just to
        // recover the rich shape so the resumed TUI still shows prior
        // tool-result cards, then discard the rest of the replay state.
        let hydrated_transcript = if !resume_state.hydrated_transcript.is_empty() {
            resume_state.hydrated_transcript.clone()
        } else if !resume_state.transcript.is_empty() {
            handle
                .replay_resume_state()
                .ok()
                .map(|replay| replay.hydrated_transcript)
                .filter(|items| !items.is_empty())
                .unwrap_or_else(|| {
                    resume_state
                        .transcript
                        .iter()
                        .cloned()
                        .map(|item| HydratedTranscriptItem::Message { item })
                        .collect()
                })
        } else {
            Vec::new()
        };
        let conversation_state = ConversationState::from_resume(resume_state, &metadata);
        // Capture the cumulative metrics from all prior runs of this session
        // before passing conversation_state into build. finish_session subtracts
        // this baseline so session_ended only reports the delta contributed by
        // this process run, preventing cumulative overcounting across resumes.
        let prior_metrics = conversation_state.metrics.clone();
        let routing_sticky_remaining = conversation_state.routing_sticky_remaining_turns();
        let routing_session_disabled = conversation_state.routing_session_disabled();
        let agent = Self::build(
            config,
            provider,
            Some(handle.clone()),
            conversation_state,
            None,
            telemetry,
            prior_metrics,
        );
        if routing_sticky_remaining > 0 || routing_session_disabled {
            // Honour the persisted sticky window so a follow-up
            // prompt on a resumed mid-hard-task session continues to
            // skip the per-turn router until the window expires. Also
            // honour `/router off`, which is session state rather than
            // a one-shot override.
            let mut state = agent.routing_state.lock().expect("routing state lock");
            state.sticky.remaining_turns = routing_sticky_remaining;
            state.pending_override.session_disabled = routing_session_disabled;
        }
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
        Ok((agent, hydrated_transcript))
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
            TelemetryClient::disabled(),
            SessionMetrics::default(),
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
        telemetry: TelemetryClient,
        prior_metrics: SessionMetrics,
    ) -> Self {
        let source_config = config.clone();
        // Arm context compaction by default. The mid-turn micro-compaction
        // tier (and the full tier) early-returns when
        // `context_compaction.model_context_window` is `None`, and that field
        // is only ever populated from explicit config — it was never derived
        // from the model registry, so in practice compaction *never fired*.
        // A long single-turn tool storm then re-sends its whole growing
        // transcript to the provider every round (quadratic in tool calls;
        // billed as cache-write on Anthropic). Derive the window from the
        // model's own registered context size so compaction can do its job.
        //
        // Capture the operator's explicit window (if any) *before* deriving so
        // a runtime model switch can re-derive for the new model while still
        // letting an explicit override win (finding #1). Explicit config
        // (`squeezy.toml` / `SQUEEZY_CONTEXT_MODEL_CONTEXT_WINDOW`) takes
        // precedence; otherwise we fall back to the registry value.
        let configured_model_context_window = config.context_compaction.model_context_window;
        config.context_compaction.model_context_window = derive_model_context_window(
            &config,
            provider.as_ref(),
            configured_model_context_window,
        );
        let output_config = ToolOutputConfig {
            spill_threshold_bytes: config.tool_spill_threshold_bytes,
            preview_bytes: config.tool_preview_bytes,
            retention_days: config.tool_output_retention_days,
            output_dir: config.cache.tool_outputs.clone(),
        };
        let websearch_provider =
            squeezy_tools::WebSearchProvider::parse(&config.websearch_provider).unwrap_or_default();
        let web_config = WebToolConfig {
            provider: websearch_provider,
            exa_mcp_url: config.exa_mcp_url.clone(),
            exa_api_key: env::var(&config.exa_api_key_env).ok(),
            parallel_mcp_url: config.parallel_mcp_url.clone(),
            parallel_api_key: env::var(&config.parallel_api_key_env).ok(),
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
        // Open only the small session-side state store synchronously. The
        // graph cache lives in `graph.redb` and is opened by the registry's
        // deferred graph task so a large semantic cache cannot block prompt
        // entry during session startup.
        let store = match SqueezyStore::open(&config.workspace_root, config.cache.root.as_deref()) {
            Ok(s) => Some(Arc::new(s)),
            Err(ref e) => {
                // On Windows, redb uses exclusive file locks; a second
                // Squeezy process (or a leftover handle) can prevent the
                // store from opening. This degrades tool receipt
                // persistence silently — warn so the user has a signal
                // in logs and support reports, even though the agent
                // continues to function without the store.
                tracing::warn!(
                    target: "squeezy::store",
                    error = %e,
                    path = %squeezy_store::state_path(
                        std::path::Path::new(&config.workspace_root),
                        config.cache.root.as_deref(),
                    ).display(),
                    "state.redb could not be opened; tool receipt persistence and \
                     read-snapshot cache are unavailable for this session \
                     (another Squeezy instance may hold the lock)",
                );
                None
            }
        };
        if let Some(store) = store.clone() {
            // Pruning expired compaction checkpoints is a best-effort GC
            // write transaction; nothing on the input path depends on it.
            // Run it on the blocking pool (when a runtime is present) so the
            // redb write never gates prompt entry. Sync construction
            // contexts (tests, no current runtime) keep the inline prune.
            let now: u128 = unix_timestamp_millis() as u128;
            let ttl_ms: u128 = (squeezy_store::DEFAULT_COMPACTION_CHECKPOINT_RETENTION_DAYS
                as u128)
                * 24
                * 60
                * 60
                * 1_000;
            let cutoff = now.saturating_sub(ttl_ms);
            let prune = move || {
                if let Err(err) = store.prune_compaction_checkpoints(cutoff) {
                    tracing::warn!(
                        target: "squeezy::store",
                        error = %err,
                        "failed to prune compaction_checkpoints; old entries may persist",
                    );
                }
            };
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn_blocking(prune);
                }
                Err(_) => prune(),
            }
        }
        let registry_runtime = ToolRegistryRuntime::new_with_graph_cache_root(
            store.clone(),
            redactor.clone(),
            config.cache.root.clone(),
        )
        .with_telemetry(telemetry.clone());
        let tools = ToolRegistry::new_with_configs_skills_and_mcp(
            config.workspace_root.clone(),
            ToolRuntimeConfig {
                output: output_config.clone(),
                web: web_config.clone(),
                shell_sandbox: config.permissions.shell_sandbox.clone(),
                mcp_servers: config.mcp_servers.clone(),
                checkpoints_enabled: config.checkpoints_enabled,
                checkpoint_store: squeezy_vcs::CheckpointStoreOptions {
                    retention_days: config.tools.checkpoint_retention_days,
                    max_file_bytes: config.tools.checkpoint_max_file_bytes,
                    cleanup_interval_secs: config.tools.checkpoint_cleanup_interval_secs,
                },
                full_access: config.permissions.mode == PermissionPolicyMode::FullAccess,
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
                    checkpoint_store: squeezy_vcs::CheckpointStoreOptions {
                        retention_days: config.tools.checkpoint_retention_days,
                        max_file_bytes: config.tools.checkpoint_max_file_bytes,
                        cleanup_interval_secs: config.tools.checkpoint_cleanup_interval_secs,
                    },
                    full_access: config.permissions.mode == PermissionPolicyMode::FullAccess,
                },
                config.skills.clone(),
                &config.graph,
                registry_runtime,
            )
            .expect("current directory must be a valid tool root")
        });
        append_session_instruction_blocks(&mut config, &tools, session_log.as_ref(), &redactor);
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
        // Discover disk-loaded custom subagents once at session start so the
        // delegate dispatch and tool advertisement can read them without
        // re-walking the filesystem on every turn.
        let subagent_catalog = Arc::new(SubagentCatalog::discover(&config.workspace_root, None));
        let session_metrics = Arc::new(Mutex::new(conversation_state.metrics.clone()));
        let next_attachment_id = next_attachment_counter(&conversation_state.context_attachments);
        let (event_broadcast, _) = broadcast::channel(64);
        // Opt-in: register skill-declared `hooks:` only when the user
        // has flipped `[skills] hooks_enabled = true`. The handler
        // implementation shells out via `sh -c` with the same trust as
        // the Squeezy process, so the default-off gate is the safety
        // boundary.
        let hooks = if config.skills.hooks_enabled {
            let mut registry = squeezy_hooks::HookRegistry::new();
            let installed = tools.register_skill_hooks(&mut registry);
            if installed == 0 {
                None
            } else {
                log_session_event(
                    session_log.as_ref(),
                    &redactor,
                    "skills_hooks_enabled",
                    None,
                    Some(format!(
                        "{installed} skill hook handler(s) registered for this session"
                    )),
                    json!({ "installed": installed }),
                );
                Some(Arc::new(registry))
            }
        } else {
            None
        };
        // Seed the memory-extraction high-water mark at the resumed transcript
        // length so a resumed session does not re-extract (and re-save) facts
        // already considered in the prior session — only new turns are scanned.
        let initial_extracted_memory_len = conversation_state.transcript.len();
        let agent = Self {
            source_config,
            telemetry,
            session_started_at: Instant::now(),
            prior_metrics,
            config,
            provider,
            tools,
            jobs: JobRegistry::new(),
            redactor,
            session_metrics,
            session_log,
            conversation_state: Arc::new(Mutex::new(conversation_state)),
            active_turn: Arc::new(StdMutex::new(None)),
            ai_reviewer_state: Arc::new(StdMutex::new(ai_reviewer::AiReviewerState::default())),
            next_turn_id: Arc::new(AtomicU64::new(1)),
            next_approval_id: Arc::new(AtomicU64::new(1)),
            last_extracted_memory_len: Arc::new(AtomicUsize::new(initial_extracted_memory_len)),
            memory_notices: Arc::new(StdMutex::new(Vec::new())),
            next_attachment_id: Arc::new(AtomicU64::new(next_attachment_id)),
            subagents: SubagentRegistry::default(),
            subagent_catalog,
            session_rules: Arc::new(RwLock::new(Vec::new())),
            session_mode: Arc::new(AtomicU8::new(initial_session_mode.to_u8())),
            loaded_tool_schemas: Arc::new(Mutex::new(Vec::new())),
            store,
            replay,
            hooks,
            pending_swap: None,
            event_broadcast,
            background_tasks: Arc::new(StdMutex::new(tokio::task::JoinSet::new())),
            mcp_background_queue: Arc::new(McpBackgroundQueue::default()),
            shutdown_token: Arc::new(StdMutex::new(CancellationToken::new())),
            routing_state: Arc::new(StdMutex::new(turn_router::RoutingPersistentState::default())),
            last_request_overhead_tokens: Arc::new(AtomicU64::new(0)),
            configured_model_context_window,
            ollama_window_cache: Arc::new(tokio::sync::Mutex::new(None)),
        };
        if let Some(log) = agent.session_log.as_ref() {
            agent.telemetry.set_store_session_id(log.session_id());
        }
        agent
    }

    /// Borrow the current effective config.
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Force the next turn onto the provider's small-fast cheap tier
    /// even when the heuristic would not have fired and the LLM judge
    /// would have voted parent. Used by the `/cheap` slash command.
    /// One-shot — consumed by the next `start_turn`.
    pub fn request_routing_force_cheap(&self) {
        let mut state = self.routing_state.lock().expect("routing state lock");
        state.pending_override.force_cheap = true;
        state.pending_override.force_parent = false;
    }

    /// Force the next turn onto the user's configured parent model,
    /// bypassing the router entirely. Used by the `/parent` slash
    /// command. One-shot — consumed by the next `start_turn`.
    pub fn request_routing_force_parent(&self) {
        let mut state = self.routing_state.lock().expect("routing state lock");
        state.pending_override.force_parent = true;
        state.pending_override.force_cheap = false;
    }

    /// Toggle the master routing switch for the rest of the session.
    /// When `disabled` is `true`, the per-turn router never picks the
    /// cheap tier implicitly; explicit `/cheap` still works. Used by
    /// `/router off|on`.
    pub fn set_routing_session_disabled(&self, disabled: bool) {
        let mut state = self.routing_state.lock().expect("routing state lock");
        state.pending_override.session_disabled = disabled;
        drop(state);
        if let Ok(mut conversation_state) = self.conversation_state.try_lock() {
            conversation_state.set_routing_session_disabled(disabled);
            if let Some(session) = &self.session_log {
                let _ = session.write_resume_state(&conversation_state.to_resume_state());
            }
            return;
        }
        let conversation_state = self.conversation_state.clone();
        let session_log = self.session_log.clone();
        tokio::spawn(async move {
            let resume = {
                let mut conversation_state = conversation_state.lock().await;
                conversation_state.set_routing_session_disabled(disabled);
                conversation_state.to_resume_state()
            };
            if let Some(session) = session_log {
                let _ = session.write_resume_state(&resume);
            }
        });
    }

    /// Resolve the cheap-tier model for the current provider, honoring any
    /// explicit overrides in `[model].small_fast_model` or
    /// `[providers.<name>].cheap_model` before falling back to the built-in
    /// per-provider mini tier. Returns `None` when the provider has no
    /// distinct cheap tier and no override is configured; `/cheap` will fall
    /// back to the parent model in that case (the TUI surfaces a preflight
    /// notice so the fallback is not silent).
    pub fn cheap_model(&self) -> Option<String> {
        cheap_model_for(self.provider.name(), &self.config)
    }

    /// Return a point-in-time snapshot of the agent's mode and routing state.
    ///
    /// This is the single authoritative source the TUI status line and tests
    /// should read: current session mode, whether auto-routing is
    /// session-disabled, pending one-shot overrides, and the sticky-escalation
    /// window. Routing fields are read from the same `routing_state` lock so
    /// that portion of the snapshot is internally consistent.
    pub fn mode_state_snapshot(&self) -> ModeStateSnapshot {
        let session_mode = self.session_mode();
        let routing = self.routing_state.lock().expect("routing state lock");
        ModeStateSnapshot {
            session_mode,
            routing_session_disabled: routing.pending_override.session_disabled,
            pending_force_cheap: routing.pending_override.force_cheap,
            pending_force_parent: routing.pending_override.force_parent,
            sticky_turns_remaining: routing.sticky.remaining_turns,
        }
    }

    /// Test-only handle to the subagent registry so callers can
    /// pre-saturate it and exercise the cap-rejection path without
    /// having to script `SUBAGENT_MAX_CONCURRENT` real subagents.
    #[cfg(test)]
    pub(crate) fn subagent_registry_for_test(&self) -> SubagentRegistry {
        self.subagents.clone()
    }

    /// Clone the current operator/session config — used by the config screen to
    /// initialize its editing buffer. This intentionally excludes runtime-only
    /// instruction additions and registry-derived context windows.
    pub fn config_snapshot(&self) -> AppConfig {
        self.source_config.clone()
    }

    /// Clone the current config in the same shape as settings loading produces,
    /// for external reload comparisons.
    pub fn settings_reload_config_snapshot(&self) -> AppConfig {
        self.source_config.clone()
    }

    /// Replace the in-process config immediately. Use for Immediate-tier
    /// saves: verbosity, permissions, telemetry on/off — fields that are
    /// consulted fresh on each operation. Fields baked into derived state at
    /// build time (tools/MCP/redactor) are NOT rebuilt; pair this with the
    /// "restart required" badge in the UI for those.
    pub fn replace_config(&mut self, next: AppConfig) {
        if next.telemetry != self.source_config.telemetry {
            self.telemetry = TelemetryClient::from_config(&next);
        }
        let skills_changed = next.skills != self.source_config.skills;
        let workspace_changed = next.workspace_root != self.source_config.workspace_root;
        self.schedule_mcp_servers_reload_if_changed(&next);
        self.source_config = next;
        self.config = self.source_config.clone();
        // A reloaded config carries the operator's explicit window (or None) —
        // it is not registry-derived (only `build()` derives). Refresh the
        // explicit baseline, then re-derive for the active model so the window
        // stays correct after a settings reload that changes the model (#1).
        self.configured_model_context_window = self.config.context_compaction.model_context_window;
        self.re_derive_model_context_window();
        if skills_changed || workspace_changed {
            self.rebuild_skills_catalog();
            // A catalog rebuild must also rebuild the hook registry: the old
            // registry could still reference handlers for skills that were
            // disabled, removed, or had their hooks edited, and
            // `hooks_enabled` might have been toggled. Leaving `self.hooks`
            // stale would let old `PreToolUse`/`PostToolUse` shell-outs keep
            // firing against the user's stated intent.
            self.rebuild_hooks_registry();
        }
        append_session_instruction_blocks(
            &mut self.config,
            &self.tools,
            self.session_log.as_ref(),
            &self.redactor,
        );
    }

    /// Spawn a background `replace_mcp_servers` against the tool
    /// registry when the incoming `[mcp.servers]` map differs from the
    /// currently-installed one. Shared between `replace_config`
    /// (settings reload + Immediate-tier saves) and
    /// `drain_pending_swap` (NextPrompt-tier saves that also change
    /// provider) so a server-map change is never silently dropped on
    /// either path. The tool *runtime* outside the MCP registry still
    /// needs a full restart for other field changes — this helper
    /// only addresses the registry hot-reload gap.
    fn schedule_mcp_servers_reload_if_changed(&self, next: &AppConfig) {
        if next.mcp_servers == self.config.mcp_servers {
            return;
        }
        let tools = self.tools.clone();
        let servers = next.mcp_servers.clone();
        let cancel = self.mcp_shutdown_child_token();
        let task = async move {
            let _ = tools.replace_mcp_servers(servers, cancel).await;
        };
        // Hand the spawn to the tracked `JoinSet` so `Agent::shutdown`
        // waits for the registry to settle before dropping the redb
        // store. Lock poisoning here only comes from a panic inside
        // another spawn site; we recover the inner data rather than
        // panic — the registry must stay usable across config edits.
        match self.background_tasks.lock() {
            Ok(mut tasks) => {
                tasks.spawn(task);
            }
            Err(poison) => {
                poison.into_inner().spawn(task);
            }
        }
    }

    /// Rebuild the skill catalog from the current `config.skills` and
    /// workspace root. Called by `replace_config` when the skill
    /// surface changed so external `settings.toml` edits — including
    /// `[[skills.config]]` enable/disable entries and dropping a new
    /// `SKILL.md` — take effect without a session restart.
    pub fn rebuild_skills_catalog(&self) -> usize {
        let count = self
            .tools
            .rebuild_skills(&self.config.workspace_root, &self.config.skills);
        log_session_event(
            self.session_log.as_ref(),
            &self.redactor,
            "skills_catalog_rebuilt",
            None,
            Some(format!(
                "{count} skill(s) in the catalog after settings reload"
            )),
            json!({ "skills_count": count }),
        );
        count
    }

    /// Rebuild the hook registry from the current `config.skills` state.
    ///
    /// Called by `replace_config`/`drain_pending_swap` whenever the skills
    /// config or workspace root changes. This enforces the trust boundary
    /// declared by the `[skills] hooks_enabled` gate: if the flag was
    /// toggled off, individual skills were disabled, or hook commands were
    /// edited, the stale handlers in the old registry must not keep firing.
    /// A fresh registry is built from the current catalog snapshot and
    /// installed atomically; the old registry is discarded.
    pub fn rebuild_hooks_registry(&mut self) {
        if !self.config.skills.hooks_enabled {
            // Gate is now off — clear any previously installed registry
            // so existing handlers stop dispatching immediately.
            self.hooks = None;
            return;
        }
        let mut registry = squeezy_hooks::HookRegistry::new();
        let installed = self.tools.register_skill_hooks(&mut registry);
        self.hooks = if installed == 0 {
            None
        } else {
            log_session_event(
                self.session_log.as_ref(),
                &self.redactor,
                "skills_hooks_rebuilt",
                None,
                Some(format!(
                    "{installed} skill hook handler(s) re-registered after settings reload"
                )),
                json!({ "installed": installed }),
            );
            Some(Arc::new(registry))
        };
    }

    /// Replace the LLM client. The in-flight turn (if any) holds a clone of
    /// the old `Arc` so it finishes against the old client; subsequent turns
    /// pick up the new one.
    pub fn replace_provider(&mut self, next: Arc<dyn LlmProvider>, model: String) {
        self.provider = next;
        self.source_config.model = model.clone();
        self.config.model = model;
        self.re_derive_model_context_window();
    }

    /// Re-derive `model_context_window` for the active provider/model after a
    /// runtime switch (finding #1). An explicit operator override always wins;
    /// otherwise the window is recomputed from the model registry so mid-turn
    /// micro/full thresholds track the *new* model's window for the rest of the
    /// session. Without this, `build()` baked in the *old* model's window and
    /// the swap paths never recomputed it.
    fn re_derive_model_context_window(&mut self) {
        self.config.context_compaction.model_context_window = derive_model_context_window(
            &self.config,
            self.provider.as_ref(),
            self.configured_model_context_window,
        );
    }

    /// The operator's explicit context-window override for the active model: a
    /// per-model `[model_limits."p:m"]` entry, else the global configured value
    /// captured at build. This is the resolver's "user override" layer; keeping
    /// it here lets the `/context` snapshot and the reroute fit-check resolve
    /// the window identically to `derive_model_context_window`.
    fn operator_context_window_override(&self) -> Option<u64> {
        self.config
            .model_limits
            .get(&self.config.model_limit_key())
            .and_then(|entry| entry.context_window)
            .or(self.configured_model_context_window)
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
        let skills_changed = swap.config.skills != self.source_config.skills;
        let workspace_changed = swap.config.workspace_root != self.source_config.workspace_root;
        // Honour any `[mcp.servers]` drift bundled into the swap on
        // the same beat as the provider/config replacement. Without
        // this, a settings-watcher reload that *also* changes the
        // provider would arm a `PendingConfigSwap` instead of going
        // through `replace_config`, and the MCP registry would
        // silently fall out of sync with `AppConfig.mcp_servers` until
        // the next process restart.
        self.schedule_mcp_servers_reload_if_changed(&swap.config);
        self.source_config = swap.config.clone();
        self.config = self.source_config.clone();
        if let Some(provider) = swap.provider.clone() {
            self.provider = provider;
        }
        // The swapped-in config carries the operator's explicit window (or
        // None) — it is not registry-derived. Refresh the explicit baseline,
        // then re-derive for the (possibly new) model/provider so mid-turn
        // thresholds track the new window for the rest of the session (#1).
        self.configured_model_context_window = self.config.context_compaction.model_context_window;
        self.re_derive_model_context_window();
        if skills_changed || workspace_changed {
            self.rebuild_skills_catalog();
            self.rebuild_hooks_registry();
        }
        append_session_instruction_blocks(
            &mut self.config,
            &self.tools,
            self.session_log.as_ref(),
            &self.redactor,
        );
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

    /// Current per-language file counts from the workspace graph, or
    /// `None` when the graph has not finished its initial open yet.
    /// Cheap to poll (graph state is in-memory; opportunistically
    /// refreshes only when the file watcher has queued changes).
    pub fn current_language_report(&self) -> Option<squeezy_tools::LanguageReport> {
        self.tools.current_language_report()
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
    ///
    /// Cancellation: this is a directly-awaited (sync) entry point whose
    /// caller can drop the future to abort, so it issues a fresh
    /// `CancellationToken` rather than a `mcp_shutdown_child_token()` child.
    /// `Agent::shutdown` cannot interrupt an in-flight call here; the
    /// `_in_background` siblings (e.g. `set_mcp_server_enabled_in_background`,
    /// `restart_mcp_server_in_background`) are the ones that adopt the
    /// shutdown-rooted token because they outlive their caller.
    pub async fn execute_local_tool(&self, call: ToolCall) -> ToolResult {
        self.tools
            .execute_for_group(call, CancellationToken::new(), "manual".to_string())
            .await
    }

    /// Refresh the MCP tool palette synchronously. Production turns kick a
    /// background refresh on each `start_turn`; this helper lets tests
    /// and the eval harness pre-warm the cache so the very first turn
    /// can issue `mcp__*` tool calls without racing the background task.
    ///
    /// See [`Agent::execute_local_tool`] for the rationale behind the fresh
    /// `CancellationToken`: this is a sync entry point whose caller controls
    /// cancellation by dropping the future, so it does not enrol in the
    /// shutdown-rooted token tree.
    pub async fn refresh_mcp_tools(&self) -> squeezy_tools::McpRefreshOutcome {
        self.tools.refresh_mcp_tools(CancellationToken::new()).await
    }

    /// Toggle an MCP server's `enabled` flag without restarting the
    /// agent. Returns the same refresh outcome `refresh_mcp_tools`
    /// produces so the caller (the `/mcp` config page, eval driver)
    /// can pull the new per-server status.
    ///
    /// Cancellation: as with `execute_local_tool` and `refresh_mcp_tools`,
    /// this is a sync call whose caller owns the lifetime, so we mint a
    /// fresh token. The `_in_background` sibling spawns into the agent's
    /// JoinSet and therefore uses `mcp_shutdown_child_token()` so
    /// `Agent::shutdown` can drain it.
    pub async fn set_mcp_server_enabled(
        &mut self,
        server_name: &str,
        enabled: bool,
    ) -> squeezy_tools::McpResult<squeezy_tools::McpRefreshOutcome> {
        let outcome = self
            .tools
            .set_mcp_server_enabled(server_name, enabled, CancellationToken::new())
            .await?;
        // Keep `self.config.mcp_servers` aligned with the registry so
        // the next config snapshot reflects the toggle without going
        // back to disk.
        if let Some(server) = self.config.mcp_servers.get_mut(server_name) {
            server.enabled = enabled;
        }
        Ok(outcome)
    }

    /// Toggle an MCP server's `enabled` flag and run discovery in the
    /// background. This is the interactive-TUI path: update the agent's
    /// config snapshot immediately so `/mcp` reflects the requested state,
    /// then let the registry publish `Starting` / final status without
    /// blocking redraws.
    pub fn set_mcp_server_enabled_in_background(&mut self, server_name: String, enabled: bool) {
        if let Some(server) = self.config.mcp_servers.get_mut(&server_name) {
            server.enabled = enabled;
        }
        let tools = self.tools.clone();
        let cancel = self.mcp_shutdown_child_token();
        let task = async move {
            let _ = tools
                .set_mcp_server_enabled(&server_name, enabled, cancel)
                .await;
        };
        self.spawn_mcp_background_task(task);
    }

    /// Restart an MCP server in place: tear down its live session and
    /// re-run discovery.
    pub async fn restart_mcp_server(
        &self,
        server_name: &str,
    ) -> squeezy_tools::McpResult<squeezy_tools::McpRefreshOutcome> {
        self.tools
            .restart_mcp_server(server_name, self.mcp_shutdown_child_token())
            .await
    }

    /// Restart an MCP server without blocking the caller. The registry owns
    /// the `Starting` / `Ready` / `Failed` snapshot transitions; the TUI polls
    /// that snapshot while this task runs.
    pub fn restart_mcp_server_in_background(&self, server_name: String) {
        let tools = self.tools.clone();
        let cancel = self.mcp_shutdown_child_token();
        let task = async move {
            let _ = tools.restart_mcp_server(&server_name, cancel).await;
        };
        self.spawn_mcp_background_task(task);
    }

    /// Replace the entire MCP server map without restarting the
    /// agent. Used for add/remove flows from the `/mcp` config page
    /// and to honour external `settings.toml` edits picked up by the
    /// settings watcher.
    pub async fn replace_mcp_servers(
        &mut self,
        servers: std::collections::BTreeMap<String, squeezy_core::McpServerConfig>,
    ) -> squeezy_tools::McpRefreshOutcome {
        self.config.mcp_servers = servers.clone();
        self.tools
            .replace_mcp_servers(servers, self.mcp_shutdown_child_token())
            .await
    }

    /// Replace the MCP server map in the background, keeping the agent config
    /// snapshot aligned immediately so `/mcp` browse rows do not wait on
    /// discovery before reflecting add/remove operations.
    pub fn replace_mcp_servers_in_background(
        &mut self,
        servers: std::collections::BTreeMap<String, squeezy_core::McpServerConfig>,
    ) {
        self.config.mcp_servers = servers.clone();
        let tools = self.tools.clone();
        let cancel = self.mcp_shutdown_child_token();
        let task = async move {
            let _ = tools.replace_mcp_servers(servers, cancel).await;
        };
        self.spawn_mcp_background_task(task);
    }

    fn spawn_mcp_background_task<F>(&self, task: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let ticket = self.mcp_background_queue.issue_ticket();
        let queue = self.mcp_background_queue.clone();
        let task = async move {
            queue.wait_for_turn(ticket).await;
            let result = AssertUnwindSafe(task).catch_unwind().await;
            queue.finish_turn();
            if result.is_err() {
                tracing::warn!(
                    target: "squeezy::mcp",
                    ticket,
                    "background MCP config action panicked"
                );
            }
        };
        match self.background_tasks.lock() {
            Ok(mut tasks) => {
                tasks.spawn(task);
            }
            Err(poison) => {
                poison.into_inner().spawn(task);
            }
        }
    }

    fn mcp_shutdown_child_token(&self) -> CancellationToken {
        match self.shutdown_token.lock() {
            Ok(token) => token.child_token(),
            Err(poison) => poison.into_inner().child_token(),
        }
    }

    /// Snapshot of the registry's live server map. Mirrors
    /// `AppConfig.mcp_servers` but reads from the registry directly so
    /// callers see post-`replace_mcp_servers` state.
    pub fn mcp_servers(&self) -> std::collections::BTreeMap<String, squeezy_core::McpServerConfig> {
        self.tools.mcp_servers()
    }

    pub fn mcp_status_snapshot(&self) -> squeezy_tools::McpStatusSnapshot {
        self.tools.mcp_status_snapshot()
    }

    /// Drain every background task the agent spawned (currently just the
    /// MCP tool-palette refresh from `start_turn`) and wait for it to
    /// finish. Once this returns, the spawned tasks have dropped their
    /// `Arc<SqueezyStore>` clones, so a caller that owns the agent can
    /// safely drop it and re-open the redb store without racing the
    /// background lifetime. Tests rely on this for deterministic shared-
    /// state-store assertions on Windows, where the redb lock is
    /// exclusive and a same-process re-open fails while any handle is
    /// still alive. The agent remains usable after shutdown: a fresh
    /// `start_turn` will simply register new tasks into the now-empty
    /// JoinSet.
    pub async fn shutdown(&self) {
        // Signal all agent-lifetime background tasks (MCP reload, toggle,
        // restart) to stop. This bounds their lifetime so callers can safely
        // drop the agent and reopen any held file handles (e.g. redb on
        // Windows, which uses an exclusive lock).
        match self.shutdown_token.lock() {
            Ok(token) => token.cancel(),
            Err(poison) => poison.into_inner().cancel(),
        }
        let mut tasks = match self.background_tasks.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(poison) => std::mem::take(&mut *poison.into_inner()),
        };
        while tasks.join_next().await.is_some() {}
        match self.shutdown_token.lock() {
            Ok(mut token) => *token = CancellationToken::new(),
            Err(poison) => *poison.into_inner() = CancellationToken::new(),
        }
    }

    pub fn subscribe_jobs(&self) -> broadcast::Receiver<JobEvent> {
        self.jobs.subscribe()
    }

    /// Subscribe to agent-level events that fire outside a turn's per-call
    /// `mpsc::Sender<AgentEvent>`. Currently used by manual `/compact`
    /// (`compact_context_manual`) to fan out `AgentEvent::ContextCompacted`
    /// to TUI overlays, eval capture, MCP listeners, and any other
    /// out-of-turn subscriber. The auto-compaction and mid-turn
    /// micro-compaction paths continue to send through the per-turn
    /// `mpsc` so in-turn consumers see compaction in the same stream as
    /// the surrounding tool calls and assistant text; this broadcast is
    /// the supplementary path for events with no active turn.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Arc<AgentEvent>> {
        self.event_broadcast.subscribe()
    }

    pub fn jobs_snapshot(&self) -> Vec<JobSnapshot> {
        self.jobs.snapshot()
    }

    pub fn job_notifications(&self) -> Vec<JobNotification> {
        self.jobs.notifications()
    }

    /// Take and clear any one-line summaries queued by the automatic memory
    /// extraction pass since the last call. The TUI drains this each poll and
    /// renders each as a quiet transcript line.
    pub fn drain_memory_notices(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .memory_notices
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
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

    pub fn record_slash_command_telemetry(
        &self,
        command: &str,
        surface: SlashSurface,
        outcome: SlashOutcome,
        alias_kind: SlashAliasKind,
        arg_shape: SlashArgShape,
    ) {
        self.telemetry.spawn(TelemetryEvent::slash_command_used(
            SlashTelemetryReport::new(command, surface, outcome, alias_kind, arg_shape),
        ));
    }

    pub fn record_startup_ready_telemetry(&self, route: StartupRoute, duration: Duration) {
        self.telemetry
            .spawn(TelemetryEvent::startup_ready(&self.config, route, duration));
    }

    pub fn record_config_change_telemetry(&self, report: ConfigChangeReport<'_>) {
        self.telemetry
            .spawn(TelemetryEvent::config_change_committed(report));
    }

    /// Fire anonymous `help_answer_rated` telemetry for a thumbs-up / -down on
    /// the most recent `/help` answer. `topic` is the curated topic id, `source`
    /// is how the answer was produced, and `rating` is the direction. No prompt
    /// or answer text crosses this boundary — see [`HelpAnswerRatedReport`].
    pub fn record_help_answer_rated_telemetry(
        &self,
        topic: &str,
        source: HelpAnswerSourceKind,
        rating: HelpRatingKind,
    ) {
        self.telemetry
            .spawn(TelemetryEvent::help_answer_rated(HelpAnswerRatedReport {
                topic,
                source,
                rating,
            }));
    }

    /// Fire `prompt_template_expanded` telemetry for a template that matched
    /// a user's slash input. `source_token` is the safe token for the template
    /// source (e.g. `"user"` or `"project"`), `arg_count` is the number of
    /// positional arguments supplied, and `queued` distinguishes a queued
    /// expansion (turn is active) from an immediately-started one.
    pub fn record_prompt_template_telemetry(
        &self,
        source_token: &str,
        arg_count: u32,
        queued: bool,
    ) {
        self.telemetry
            .spawn(TelemetryEvent::prompt_template_expanded(
                source_token,
                arg_count,
                queued,
            ));
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
        // Live provider window probe (Ollama only today). Folded into the
        // resolver as the `provider_live_window` layer rather than a blanket
        // override so its provenance shows as "provider live".
        // Cache with a 30-second TTL so repeated /cost or /context invocations
        // in quick succession skip the blocking network probe.
        const OLLAMA_WINDOW_CACHE_TTL: Duration = Duration::from_secs(30);
        let provider_live_window = match &self.config.provider {
            ProviderConfig::Ollama(ollama) => {
                let mut cache = self.ollama_window_cache.lock().await;
                let cached = cache
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < OLLAMA_WINDOW_CACHE_TTL);
                if let Some((_, window)) = cached {
                    *window
                } else {
                    let window =
                        fetch_ollama_context_window(&ollama.base_url, &self.config.model).await;
                    *cache = Some((Instant::now(), window));
                    window
                }
            }
            _ => None,
        };
        let observed_ceiling = state
            .observed_context_ceilings
            .get(&(self.provider.name().to_string(), self.config.model.clone()))
            .copied();
        let limit_input = ContextLimitInput {
            provider: self.provider.name(),
            model: &self.config.model,
            user_override: self.operator_context_window_override(),
            provider_live_window,
            observed_ceiling,
            models_dev: squeezy_llm::cached_models_dev_view(),
            effective_percent_override: self
                .config
                .context_compaction
                .effective_context_window_percent,
            baseline_reserve_override: self.config.context_compaction.baseline_reserve_tokens,
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
            transmitted_request: estimate_request_context_full(
                &limit_input,
                &transmitted_request,
                Some(&state.token_calibration),
            ),
            full_history_request: estimate_request_context_full(
                &limit_input,
                &full_history_request,
                Some(&state.token_calibration),
            ),
            skills: self.skills_accounting(),
            mcp: self.mcp_accounting(&loaded_tool_schemas),
            calibration_source: state.calibration_source,
            budget_policy: BudgetPolicySnapshot {
                max_session_cost_usd_micros: self.config.max_session_cost_usd_micros,
                cost_warn_percent: self.config.cost_warn_percent,
                max_round_input_tokens: self.config.max_round_input_tokens,
                max_tool_calls_per_turn: self.config.max_tool_calls_per_turn,
                max_tool_bytes_read_per_turn: self.config.max_tool_bytes_read_per_turn,
                max_search_files_per_turn: self.config.max_search_files_per_turn,
                disable_prompt_cache: self.config.disable_prompt_cache,
            },
        }
    }

    /// Build the `/context` "Skills" view: every discovered skill with its
    /// always-present metadata cost split from its body cost. A skill is
    /// `loaded` when its body is materialized this session; body bytes are
    /// exact for loaded skills and the on-disk `SKILL.md` size (first-load cost)
    /// otherwise.
    fn skills_accounting(&self) -> SkillsAccounting {
        let breakdown = self.tools.skill_context_breakdown();
        let mut entries = Vec::with_capacity(breakdown.len());
        let mut loaded = 0;
        let mut metadata_bytes_total = 0;
        let mut loaded_body_bytes_total = 0;
        for item in breakdown {
            metadata_bytes_total += item.metadata_bytes;
            if item.loaded {
                loaded += 1;
                loaded_body_bytes_total += item.body_bytes;
            }
            entries.push(SkillAccountingEntry {
                name: item.name,
                description: item
                    .description
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                loaded: item.loaded,
                metadata_bytes: item.metadata_bytes,
                body_bytes: item.body_bytes,
            });
        }
        SkillsAccounting {
            discovered: entries.len(),
            loaded,
            entries,
            metadata_bytes_total,
            loaded_body_bytes_total,
        }
    }

    /// Build the `/context` "MCPs" view: connected MCP tools grouped by server,
    /// each split into its lazy stub cost and full-schema (first-load) cost,
    /// with per-server live status. `loaded_tool_schemas` is the set of tool
    /// names whose full schema is attached to the request this session.
    fn mcp_accounting(&self, loaded_tool_schemas: &[String]) -> McpAccounting {
        let lazy = self.config.tools.lazy_schema_loading;
        let loaded_set: BTreeSet<&str> = loaded_tool_schemas.iter().map(String::as_str).collect();
        let status = self.tools.mcp_status_snapshot();
        let tool_infos = self.tools.mcp_tool_schema_infos();
        let total_tools = tool_infos.len();

        // Group tools under their owning server. Seed the map from the status
        // snapshot so configured-but-toolless servers still render.
        let mut servers: BTreeMap<String, McpServerAccounting> = BTreeMap::new();
        for (name, server_status) in &status.per_server {
            servers.insert(
                name.clone(),
                McpServerAccounting {
                    name: name.clone(),
                    status: format_mcp_status(server_status),
                    ..McpServerAccounting::default()
                },
            );
        }
        let mut stub_bytes_total = 0;
        let mut loaded_full_bytes_total = 0;
        for info in tool_infos {
            // Without lazy loading every schema is always sent (no stub, always
            // "loaded"). With it, the stub line is always present and the full
            // schema is live only after `load_tool_schema`.
            let full_live = !lazy || loaded_set.contains(info.name.as_str());
            let stub = if lazy { info.stub_bytes } else { 0 };
            let live_full = if full_live { info.full_bytes } else { 0 };
            stub_bytes_total += stub;
            loaded_full_bytes_total += live_full;
            let entry = servers
                .entry(info.server.clone())
                .or_insert_with(|| McpServerAccounting {
                    name: info.server.clone(),
                    status: "configured".to_string(),
                    ..McpServerAccounting::default()
                });
            entry.stub_bytes += stub;
            entry.loaded_full_bytes += live_full;
            entry.in_context_bytes += stub + live_full;
            entry.tools.push(McpToolAccountingEntry {
                name: info.name,
                description: info.description,
                stub_bytes: stub,
                full_bytes: info.full_bytes,
                loaded: full_live,
            });
        }
        McpAccounting {
            servers: servers.into_values().collect(),
            total_tools,
            lazy,
            stub_bytes_total,
            loaded_full_bytes_total,
            in_context_bytes_total: stub_bytes_total + loaded_full_bytes_total,
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
        let raw_instructions =
            instructions_with_batch_hint(&raw_instructions, self.config.batch_tool_calls_hint);
        let request_instructions = self.redactor.redact(&raw_instructions).text;
        let mut all_tool_specs =
            core_control_tools(&self.config.subagents, mode, &self.subagent_catalog);
        all_tool_specs.extend(self.tools.specs().iter().cloned().map(advertised_tool));
        retain_non_excluded_tools(&mut all_tool_specs, &self.config.tools);
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
            temperature: self.config.temperature,
            top_p: self.config.top_p,
            seed: self.config.seed,
            stop: self.config.stop.clone(),
            frequency_penalty: self.config.frequency_penalty,
            presence_penalty: self.config.presence_penalty,
            response_verbosity: request_response_verbosity(&self.config, self.provider.name()),
            reasoning_effort: request_reasoning_effort(&self.config, self.provider.name()),
            previous_response_id: if include_response_state {
                previous_response_id
            } else {
                None
            },
            cache_key: None,
            cache: self.session_prompt_cache_key().into(),
            disable_prompt_cache: self.config.disable_prompt_cache,
            tools: Arc::from(request_tool_specs(
                &all_tool_specs,
                mode,
                &self.config.tools,
                loaded_tool_schemas,
                plan_edit_allowed,
            )),
            store,
            tool_choice: self.config.tool_choice.clone(),
            output_schema: None,
            // Mirror the wire request so context/token accounting reflects
            // the same `parallel_tool_calls` choice the real request sends.
            parallel_tool_calls: self.config.parallel_tool_calls,
            beta_headers: request_beta_headers(&self.config, self.provider.name()),
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

    /// Set (or clear, when `name` is `None`) the active session's
    /// `display_name`. The new value is persisted to the session's
    /// `metadata.json` *and* refreshed in the cross-project global
    /// index so the resume picker — both same-cwd and Tab-toggled
    /// cross-project — surfaces the user-facing name on the next
    /// open. Returns the post-update metadata snapshot.
    ///
    /// Errors when no session log is attached (session logging
    /// disabled at startup).
    pub fn set_session_display_name(
        &self,
        name: Option<String>,
    ) -> squeezy_core::Result<SessionMetadata> {
        let Some(handle) = self.session_log.as_ref() else {
            return Err(SqueezyError::Agent(
                "no active session to rename".to_string(),
            ));
        };
        let normalized = name.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        handle.update_metadata_and_index(|metadata| {
            metadata.display_name = normalized;
        })
    }

    /// Append `label` to the active session's `labels` list, deduping
    /// case-sensitively so muscle-memory re-runs stay no-ops. Returns
    /// `(metadata, added)`; `added` is `false` when the label was
    /// already present, in which case the metadata snapshot is still
    /// returned so callers can echo the current label set.
    ///
    /// Empty labels are rejected so the user never accidentally
    /// inserts a blank tag.
    pub fn add_session_label(
        &self,
        label: String,
    ) -> squeezy_core::Result<(SessionMetadata, bool)> {
        let Some(handle) = self.session_log.as_ref() else {
            return Err(SqueezyError::Agent(
                "no active session to label".to_string(),
            ));
        };
        let normalized = label.trim().to_string();
        if normalized.is_empty() {
            return Err(SqueezyError::Agent("label must not be empty".to_string()));
        }
        let mut added = false;
        let snapshot = handle.update_metadata_and_index(|metadata| {
            if metadata
                .labels
                .iter()
                .any(|existing| existing == &normalized)
            {
                return;
            }
            metadata.labels.push(normalized.clone());
            added = true;
        })?;
        Ok((snapshot, added))
    }

    pub fn prepare_feedback(&self, message: &str) -> squeezy_core::Result<PreparedFeedback> {
        prepare_feedback(&self.config, message, "tui")
    }

    pub async fn submit_feedback(
        &self,
        feedback: &PreparedFeedback,
    ) -> squeezy_core::Result<FeedbackSubmitResult> {
        FeedbackClient::from_config_with_session(
            &self.config,
            self.telemetry.session_id().as_deref(),
        )
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
        FeedbackClient::from_config_with_session(
            &self.config,
            self.telemetry.session_id().as_deref(),
        )
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

    pub fn resume_current(
        &mut self,
        session_id: &str,
    ) -> squeezy_core::Result<Vec<HydratedTranscriptItem>> {
        let (agent, transcript) =
            Self::resume(self.config.clone(), self.provider.clone(), session_id)?;
        *self = agent;
        Ok(transcript)
    }

    /// Branch the active session into a sibling session that inherits the
    /// current transcript-so-far. The parent session is left resumable on
    /// disk (status flipped to `Completed`) so the user can rewind to it via
    /// `/resume`. The fork copies the live conversation state into a fresh
    /// session log; subsequent turns append only to the new session.
    ///
    /// Returns the new session id, or an error if no active session log is
    /// attached (e.g. when session logging was disabled at startup).
    pub async fn fork_current(&mut self) -> squeezy_core::Result<String> {
        let Some(parent) = self.session_log.clone() else {
            return Err(SqueezyError::Agent("no active session to fork".to_string()));
        };
        let parent_session_id = parent.session_id().to_string();
        let state = self.conversation_state.lock().await.clone();
        let resume_state = state.to_resume_state();
        // Finalise the parent with the latest resume snapshot so `/resume
        // <parent>` later picks up exactly where the fork branched, and so
        // retention treats it as a normal closed session rather than an
        // orphaned running one.
        parent.write_resume_state(&resume_state)?;
        parent.finish(
            SessionStatus::Completed,
            state.cost.clone(),
            state.metrics.clone(),
            state.redactions,
        )?;
        // Seed the new session with the inherited cost/metrics so accounting
        // reflects work the user has already paid for; the conversation copy
        // lives in resume_state.json.
        let store = SessionStore::open(&self.config);
        let metadata = SessionMetadata {
            cost: state.cost,
            metrics: state.metrics,
            redactions: state.redactions,
            token_calibration: state.token_calibration,
            parent_id: Some(parent_session_id.clone()),
            ..SessionMetadata::new(&self.config, self.provider.name())
        };
        let child = store.start_session(metadata)?;
        let new_session_id = child.session_id().to_string();
        child.write_resume_state(&resume_state)?;
        // Record fork lineage so replay / bug-report tooling can attribute
        // the child to its parent. Use the free-form append: the typed
        // SessionEventKind enum has no `Forked` variant.
        let _ = child.append_event(SessionEvent::new(
            "session_forked",
            None,
            Some(format!("forked from {parent_session_id}")),
            json!({ "parent_session_id": parent_session_id }),
        ));
        self.session_log = Some(child);
        Ok(new_session_id)
    }

    /// Clear the live conversation and start a clean slate, mirroring
    /// Claude Code's `/clear`.
    ///
    /// When a durable session log is attached the outgoing session is
    /// finalised on disk (latest resume snapshot written, status flipped
    /// to `Completed`) so the pre-clear conversation stays resumable via
    /// `/resume`, then a fresh empty session is opened and bound; its id
    /// is returned as `Some`. When session logging is disabled
    /// (`new_ephemeral`, or logging failed at startup) only the
    /// in-memory conversation is wiped and `None` is returned.
    ///
    /// Either way the live conversation, transcript, attachments, cost
    /// and metrics are reset. Cross-session token calibration is
    /// preserved so the new conversation's token estimator stays warm,
    /// and the user's `/router off` toggle survives because it is a
    /// session preference rather than conversation state.
    pub async fn clear_conversation(&mut self) -> squeezy_core::Result<Option<String>> {
        // Rotate the durable session first (if any) so a failure to
        // persist the outgoing conversation aborts before we drop it
        // from memory.
        let new_session_id = if let Some(current) = self.session_log.clone() {
            let (resume_state, cost, metrics, redactions) = {
                let state = self.conversation_state.lock().await;
                (
                    state.to_resume_state(),
                    state.cost.clone(),
                    state.metrics.clone(),
                    state.redactions,
                )
            };
            current.write_resume_state(&resume_state)?;
            current.finish(SessionStatus::Completed, cost, metrics, redactions)?;

            let store = SessionStore::open(&self.config);
            let metadata = SessionMetadata::new(&self.config, self.provider.name());
            let fresh = store.start_session(metadata)?;
            let new_session_id = fresh.session_id().to_string();
            let _ = fresh.append_event(SessionEvent::new(
                "session_started",
                None,
                Some("session started (cleared)".to_string()),
                json!({ "cleared_from": current.session_id() }),
            ));
            self.session_log = Some(fresh);
            Some(new_session_id)
        } else {
            None
        };

        // Wipe the live conversation but carry over the warm token
        // estimator and the session-wide routing toggle so the next turn
        // doesn't fall back to provider defaults or silently re-enable a
        // router the user turned off.
        {
            let mut state = self.conversation_state.lock().await;
            let token_calibration = state.token_calibration.clone();
            let calibration_source = state.calibration_source;
            let routing_session_disabled = state.routing_session_disabled();
            *state = ConversationState {
                token_calibration,
                calibration_source,
                routing_session_disabled,
                ..ConversationState::default()
            };
        }
        // Drop the per-turn router's sticky window — it tracked the hard
        // task that is now gone — while leaving the `/router off`
        // override (mirrored above) in place.
        if let Ok(mut routing) = self.routing_state.lock() {
            routing.sticky.remaining_turns = 0;
        }
        Ok(new_session_id)
    }

    /// Branch the active session into a sibling that lives under a
    /// **different** workspace's project dir. Unlike [`fork_current`], the
    /// running process keeps writing to its current session — only the new
    /// child artifact is stamped under `target_workspace_root`'s
    /// `.squeezy/sessions/` tree, with `metadata.cwd` /
    /// `metadata.workspace_root` rewritten to point at the target and
    /// `metadata.parent_id` retaining the cross-workspace lineage. The user
    /// then opens the new session manually in the target dir, or via
    /// `squeezy --workspace <target> sessions resume <new_id>`; this method
    /// deliberately does **not** auto-cd the running process.
    ///
    /// Returns the new session id, or an error if no active session log is
    /// attached (e.g. when session logging was disabled at startup) or the
    /// target workspace cannot be prepared for writes.
    pub async fn fork_into(
        &mut self,
        target_workspace_root: &Path,
    ) -> squeezy_core::Result<String> {
        let Some(parent) = self.session_log.clone() else {
            return Err(SqueezyError::Agent("no active session to fork".to_string()));
        };
        // Make sure the target dir exists before we ask `SessionStore::open`
        // to resolve `.squeezy/sessions/` against it. Without this a typo
        // surfaces deep inside `create_dir_all` with a path that mixes the
        // canonicalisation fallback with the user's relative input, which
        // is much harder to diagnose than a clean "target not found".
        fs::create_dir_all(target_workspace_root)?;
        let target_root = fs::canonicalize(target_workspace_root)
            .unwrap_or_else(|_| target_workspace_root.to_path_buf());
        let parent_session_id = parent.session_id().to_string();
        let state = self.conversation_state.lock().await.clone();
        let resume_state = state.to_resume_state();
        // Rewrite workspace_root in a config clone so the target store
        // resolves `.squeezy/sessions/` (and any relative `cache.root` /
        // `session_logs.log_dir`) against the target dir. Absolute paths in
        // the user's config keep their original behaviour, which is the
        // documented expectation for absolutely-rooted caches.
        let mut target_config = self.config.clone();
        target_config.workspace_root = target_root.clone();
        let store = SessionStore::open(&target_config);
        let mut metadata = SessionMetadata {
            cost: state.cost.clone(),
            metrics: state.metrics.clone(),
            redactions: state.redactions,
            token_calibration: state.token_calibration.clone(),
            parent_id: Some(parent_session_id.clone()),
            ..SessionMetadata::new(&target_config, self.provider.name())
        };
        // `SessionMetadata::new` picks `cwd` from the running process via
        // `env::current_dir()`, which would still be repo A. Pin it to the
        // target so `squeezy sessions resume` and the missing-cwd guard
        // both pick the target on open.
        metadata.cwd = target_root.display().to_string();
        let child = store.start_session(metadata)?;
        let new_session_id = child.session_id().to_string();
        child.write_resume_state(&resume_state)?;
        // Record cross-workspace fork lineage so replay / bug-report tooling
        // and the TUI session list can attribute the child to its parent
        // even when the two live in different project trees.
        let _ = child.append_event(SessionEvent::new(
            "session_forked",
            None,
            Some(format!(
                "forked from {parent_session_id} into {}",
                target_root.display()
            )),
            json!({
                "parent_session_id": parent_session_id,
                "target_workspace_root": target_root.display().to_string(),
            }),
        ));
        // Deliberately do not swap `self.session_log` — the in-process agent
        // is still bound to repo A's filesystem and tools, so we let the user
        // open the new session manually in the target dir (or via
        // session-id resume) rather than auto-cd-ing the running process.
        Ok(new_session_id)
    }

    pub async fn finish_session(&self, status: SessionStatus) {
        let Some(session) = &self.session_log else {
            return;
        };
        let state = self.conversation_state.lock().await.clone();
        let _ = session.write_resume_state(&state.to_resume_state());
        let metrics = state.metrics.clone();
        let _ = session.finish(status, state.cost, state.metrics, state.redactions);
        let p = &self.prior_metrics;
        // Build per-kind subagent count map. Subtract prior_metrics so resumed
        // sessions only report the delta since this process started.
        let mut subagent_kind_counts = std::collections::BTreeMap::new();
        for (kind, bucket, prior_bucket) in [
            (
                "delegate",
                &metrics.subagent_by_kind.delegate,
                &p.subagent_by_kind.delegate,
            ),
            (
                "explore",
                &metrics.subagent_by_kind.explore,
                &p.subagent_by_kind.explore,
            ),
            (
                "plan",
                &metrics.subagent_by_kind.plan,
                &p.subagent_by_kind.plan,
            ),
            (
                "review",
                &metrics.subagent_by_kind.review,
                &p.subagent_by_kind.review,
            ),
        ] {
            let calls = bucket.calls.saturating_sub(prior_bucket.calls);
            let failures = bucket.failures.saturating_sub(prior_bucket.failures);
            if calls > 0 {
                *subagent_kind_counts
                    .entry(format!("{kind}_calls"))
                    .or_default() += calls;
            }
            if failures > 0 {
                *subagent_kind_counts
                    .entry(format!("{kind}_failures"))
                    .or_default() += failures;
            }
        }
        self.telemetry
            .record(TelemetryEvent::session_ended(
                &self.config,
                SessionTelemetryReport {
                    duration_ms: self.session_started_at.elapsed().as_millis() as u64,
                    status: telemetry_session_status(status),
                    store_session_id: Some(session.session_id().to_string()),
                    turns: metrics.turns.saturating_sub(p.turns),
                    tool_calls: metrics.tool_calls.saturating_sub(p.tool_calls),
                    tool_successes: metrics.tool_successes.saturating_sub(p.tool_successes),
                    tool_errors: metrics.tool_errors.saturating_sub(p.tool_errors),
                    tool_denials: metrics.tool_denials.saturating_sub(p.tool_denials),
                    tool_cancellations: metrics
                        .tool_cancellations
                        .saturating_sub(p.tool_cancellations),
                    budget_denials: metrics.budget_denials.saturating_sub(p.budget_denials),
                    subagent_calls: metrics.subagent_calls.saturating_sub(p.subagent_calls),
                    subagent_failures: metrics
                        .subagent_failures
                        .saturating_sub(p.subagent_failures),
                    subagent_kind_counts,
                    subagent_cap_rejections: 0,
                },
            ))
            .await;
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
            // The active session may still be in the lazy-F12 Pending
            // state if no substantive event has been appended yet.
            // Materialise so `SessionStore::show(...)` sees a real
            // metadata.json on disk; flush_events then catches anything
            // buffered in the writer.
            let _ = session.materialize_now();
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

    /// Byte-oriented paste path used when the incoming payload may
    /// not be valid UTF-8 — chiefly images dropped through a
    /// terminal's image-aware paste protocol or via direct binary
    /// upload. The bytes flow through the same detect/redact pipeline
    /// as [`Agent::attach_pasted_context`]; when
    /// [`squeezy_core::detect_image_mime`] confirms a vision-routable
    /// payload (PNG/JPEG/GIF/WEBP) the attachment is stored
    /// [`ContextAttachmentKind::Image`] and fans into a
    /// `LlmInputItem::Image` on the next turn.
    pub async fn attach_pasted_bytes(
        &self,
        bytes: Vec<u8>,
    ) -> squeezy_core::Result<ContextAttachmentUpdate> {
        self.attach_context_bytes(
            ContextAttachmentSource::Paste,
            "pasted context".to_string(),
            None,
            bytes,
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

    pub async fn compact_context_manual(
        &self,
    ) -> squeezy_core::Result<Option<ContextCompactionReport>> {
        // Clone the inputs and release the async mutex before the
        // (potentially long-running) model-assisted compaction await.
        // `compact_conversation_with_strategy` only touches these local
        // clones, so holding `conversation_state` across its network
        // round-trip would needlessly block every concurrent reader
        // (the TUI's per-frame context/cost snapshots) for up to
        // `model_assisted_timeout_secs`.
        let (mut conversation, mut context_compaction, attachments) = {
            let state = self.conversation_state.lock().await;
            (
                state.conversation.clone(),
                state.context_compaction.clone(),
                state.context_attachments.clone(),
            )
        };
        let Some(report) = compact_conversation_with_strategy(
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
            0,
        )
        .await
        else {
            // squeezy-kkdb (audit B4): the conversation has no
            // compaction-eligible items yet (empty session, only the
            // synthetic head, or already maximally compacted). Treat
            // this as a clean no-op rather than an error so callers
            // surface a graceful "nothing to compact" message.
            return Ok(None);
        };
        // Re-acquire the mutex to commit the compacted state.
        let mut state = self.conversation_state.lock().await;
        state.conversation = conversation;
        state.context_compaction = context_compaction;
        state.previous_response_id = None;
        if let Some(session) = &self.session_log {
            session.write_resume_state(&state.to_resume_state())?;
        }
        drop(state);
        self.log_compaction_event(&report);
        // Mirror the auto-compaction (`maybe_compact_conversation` post-turn)
        // and mid-turn micro-compaction broadcasts so any `AgentEvent`
        // subscriber — TUI overlays, eval capture, MCP listeners — observes
        // a manual `/compact` the same way it observes an automatic one.
        // Manual compaction runs between turns and so has no per-call
        // `mpsc::Sender<AgentEvent>`; the agent-level broadcast at
        // `event_broadcast` is the supplementary fan-out. `TurnId::INVALID`
        // marks this as out-of-turn so consumers don't conflate it with a
        // real turn id.
        let _ = self
            .event_broadcast
            .send(Arc::new(AgentEvent::ContextCompacted {
                turn_id: TurnId::INVALID,
                report: report.clone(),
            }));
        Ok(Some(report))
    }

    /// Dispatch a typed slash command. Every entry in
    /// `squeezy-tui`'s `SLASH_COMMANDS` table maps to a
    /// [`DispatchCommand`] variant; variants whose action lives wholly
    /// in `Agent` execute here, while variants whose effect lives in
    /// the TUI renderer (overlays, transcript pushes, clipboard, …)
    /// return [`DispatchOutcome::TuiOnly`] so the TUI can run its
    /// existing helper while RPC/eval drivers see a structured value.
    pub async fn dispatch_command(&self, cmd: DispatchCommand) -> DispatchOutcome {
        match cmd {
            DispatchCommand::Compact { subcommand } => match subcommand {
                CompactSubcommand::History => DispatchOutcome::TuiOnly {
                    command: "/compact history".into(),
                },
                CompactSubcommand::Undo => match self.compact_context_undo().await {
                    Ok(Some(_)) => DispatchOutcome::CompactedUndo { restored: true },
                    Ok(None) => DispatchOutcome::CompactedUndo { restored: false },
                    Err(err) => DispatchOutcome::Error {
                        command: "/compact".into(),
                        message: format!("{err}"),
                    },
                },
                CompactSubcommand::Run => match self.compact_context_manual().await {
                    Ok(Some(_)) => DispatchOutcome::Compacted { skipped: false },
                    Ok(None) => DispatchOutcome::Compacted { skipped: true },
                    Err(err) => DispatchOutcome::Error {
                        command: "/compact".into(),
                        message: format!("{err}"),
                    },
                },
            },
            DispatchCommand::Plan { prompt } => {
                let changed = self.set_session_mode(SessionMode::Plan, "dispatch_command");
                DispatchOutcome::ModeChanged {
                    mode: "plan".into(),
                    changed,
                    prompt,
                }
            }
            DispatchCommand::Build { prompt } => {
                let changed = self.set_session_mode(SessionMode::Build, "dispatch_command");
                DispatchOutcome::ModeChanged {
                    mode: "build".into(),
                    changed,
                    prompt,
                }
            }
            DispatchCommand::Cost => {
                let snap = self.session_accounting_snapshot().await;
                DispatchOutcome::CostSnapshot {
                    debug: format!("{snap:?}"),
                }
            }
            DispatchCommand::Context => {
                let snap = self.session_accounting_snapshot().await;
                DispatchOutcome::ContextSnapshot {
                    debug: format!("{snap:?}"),
                }
            }
            DispatchCommand::Reviewer => {
                let entries = self.reviewer_audit_snapshot();
                DispatchOutcome::ReviewerSnapshot {
                    count: entries.len(),
                }
            }
            DispatchCommand::Tasks => {
                let jobs = self.jobs_snapshot();
                DispatchOutcome::JobsList { count: jobs.len() }
            }
            DispatchCommand::Task { id } => {
                let job_id = id.parse::<JobId>().ok();
                let found = job_id.and_then(|id| self.job_snapshot(id)).is_some();
                DispatchOutcome::TaskDetail { id, found }
            }
            DispatchCommand::TaskCancel { id } => {
                let cancelled = id
                    .parse::<JobId>()
                    .ok()
                    .map(|id| self.cancel_job(id))
                    .unwrap_or(false);
                DispatchOutcome::TaskCancel { id, cancelled }
            }
            DispatchCommand::Permissions => {
                let rules = self.session_rules_snapshot();
                DispatchOutcome::PermissionsList { count: rules.len() }
            }
            DispatchCommand::Attach { path } => {
                match self.attach_file_context(PathBuf::from(&path)).await {
                    Ok(update) => DispatchOutcome::Attached {
                        id: update.attachment.id.clone(),
                    },
                    Err(err) => DispatchOutcome::Error {
                        command: "/attach".into(),
                        message: format!("{err}"),
                    },
                }
            }
            DispatchCommand::Detach { id } => match self.detach_context_attachment(&id).await {
                Ok(attachment) => DispatchOutcome::Detached {
                    id: attachment.id.clone(),
                },
                Err(err) => DispatchOutcome::Error {
                    command: "/detach".into(),
                    message: format!("{err}"),
                },
            },
            DispatchCommand::Attachments => {
                let count = self.context_attachments_snapshot().await.len();
                DispatchOutcome::AttachmentsList { count }
            }
            DispatchCommand::Pins => {
                let count = self.context_compaction_snapshot().await.pinned.len();
                DispatchOutcome::PinsList { count }
            }
            DispatchCommand::Unpin { id } => match self.unpin_context_entry(&id).await {
                Ok(pin) => DispatchOutcome::Unpinned { id: pin.id },
                Err(err) => DispatchOutcome::Error {
                    command: "/unpin".into(),
                    message: format!("{err}"),
                },
            },
            DispatchCommand::Sessions => match self.list_sessions(&SessionQuery::default()) {
                Ok(sessions) => DispatchOutcome::SessionsList {
                    count: sessions.len(),
                },
                Err(err) => DispatchOutcome::Error {
                    command: "/sessions".into(),
                    message: format!("{err}"),
                },
            },
            DispatchCommand::Session { id } => {
                let exists = self.show_session(&id).is_ok();
                DispatchOutcome::SessionDetail {
                    session_id: id,
                    exists,
                }
            }
            DispatchCommand::SessionRename { name } => {
                let normalized = if name.trim().is_empty() {
                    None
                } else {
                    Some(name)
                };
                match self.set_session_display_name(normalized) {
                    Ok(metadata) => DispatchOutcome::SessionRenamed {
                        session_id: metadata.session_id,
                        display_name: metadata.display_name,
                    },
                    Err(err) => DispatchOutcome::Error {
                        command: "/session".into(),
                        message: format!("{err}"),
                    },
                }
            }
            DispatchCommand::SessionLabel { name } => match self.add_session_label(name.clone()) {
                Ok((metadata, added)) => DispatchOutcome::SessionLabelled {
                    session_id: metadata.session_id,
                    label: name,
                    added,
                    labels: metadata.labels,
                },
                Err(err) => DispatchOutcome::Error {
                    command: "/session".into(),
                    message: format!("{err}"),
                },
            },
            DispatchCommand::SessionExport { id } => match self.export_session(&id) {
                Ok(value) => DispatchOutcome::SessionExported {
                    session_id: id,
                    bytes: serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0),
                },
                Err(err) => DispatchOutcome::Error {
                    command: "/session-export".into(),
                    message: format!("{err}"),
                },
            },
            // `/diff` returns a worktree `DiffSnapshot` so headless
            // drivers (eval, RPC) can audit the same payload the TUI
            // renders into a diff card via `handle_slash_diff`. The
            // call shells out to `git status` + `git diff` via
            // `GitVcs::snapshot`; parked on `spawn_blocking` to keep
            // the async runtime free.
            DispatchCommand::Diff => {
                let tools = self.tools.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    tools.diff_snapshot(
                        squeezy_vcs::DiffMode::Worktree,
                        squeezy_vcs::DiffOptions {
                            include_patch: true,
                            ..squeezy_vcs::DiffOptions::default()
                        },
                    )
                })
                .await
                .unwrap_or_else(|err| squeezy_vcs::DiffSnapshot {
                    vcs: squeezy_vcs::VcsInfo {
                        kind: squeezy_vcs::VcsKind::None,
                        ..squeezy_vcs::VcsInfo::default()
                    },
                    mode: squeezy_vcs::DiffMode::Worktree,
                    summary: squeezy_vcs::DiffSummary::default(),
                    files: Vec::new(),
                    truncated: false,
                    errors: vec![format!("diff snapshot task panicked: {err}")],
                });
                let vcs_kind = match snapshot.vcs.kind {
                    squeezy_vcs::VcsKind::Git => "git",
                    squeezy_vcs::VcsKind::None => "none",
                }
                .to_string();
                let files_changed = snapshot.summary.files_changed;
                let additions = snapshot.summary.additions;
                let deletions = snapshot.summary.deletions;
                let untracked_files = snapshot.summary.untracked_files;
                DispatchOutcome::DiffSnapshot {
                    vcs_kind,
                    files_changed,
                    additions,
                    deletions,
                    untracked_files,
                    snapshot: Box::new(snapshot),
                }
            }
            // `/undo` rolls back the most recent checkpoint.
            // Returns a typed `CheckpointUndo` so headless drivers
            // see the structured `RollbackResult` (or `None` when
            // checkpoints are disabled) instead of a string status.
            // The TUI keeps running the rollback through its local
            // tool job for card-lifecycle observability.
            // `CheckpointStore::rollback` writes journal entries and
            // touches the filesystem; parked on `spawn_blocking`.
            DispatchCommand::Undo => {
                let tools = self.tools.clone();
                let join =
                    tokio::task::spawn_blocking(move || tools.checkpoint_undo_latest(None)).await;
                match join {
                    Err(err) => DispatchOutcome::Error {
                        command: "/undo".into(),
                        message: format!("undo task panicked: {err}"),
                    },
                    Ok(Ok(Some(result))) => {
                        let applied = result.applied;
                        let skipped = result.skipped;
                        let checkpoint_ids = result.checkpoint_ids.clone();
                        DispatchOutcome::CheckpointUndo {
                            applied,
                            skipped,
                            checkpoint_ids,
                            result: Some(Box::new(result)),
                        }
                    }
                    Ok(Ok(None)) => DispatchOutcome::CheckpointUndo {
                        applied: false,
                        skipped: true,
                        checkpoint_ids: Vec::new(),
                        result: None,
                    },
                    Ok(Err(err)) => DispatchOutcome::Error {
                        command: "/undo".into(),
                        message: format!("{err}"),
                    },
                }
            }
            // `/fork`, `/clear`, `/resume`, `/session-export-html`,
            // `/pin`, `/checkpoint*`, `/revert-turn` require &mut
            // self or interact with TUI-owned state (transcript selection,
            // vcs background job). The TUI keeps running those
            // through its existing helpers; the agent dispatch records the
            // typed entry point via `TuiOnly` so RPC drivers still see the
            // command they invoked.
            cmd @ (DispatchCommand::Fork
            | DispatchCommand::Clear
            | DispatchCommand::Resume { .. }
            | DispatchCommand::SessionExportHtml { .. }
            | DispatchCommand::Pin { .. }
            | DispatchCommand::Checkpoints
            | DispatchCommand::CheckpointsDoctor
            | DispatchCommand::Checkpoint { .. }
            | DispatchCommand::RevertTurn { .. }
            | DispatchCommand::Help { .. }
            | DispatchCommand::Config { .. }
            | DispatchCommand::Mcp
            | DispatchCommand::Model
            | DispatchCommand::Plans { .. }
            | DispatchCommand::Feedback { .. }
            | DispatchCommand::Report { .. }
            | DispatchCommand::Effort { .. }
            | DispatchCommand::ToolVerbosity { .. }
            | DispatchCommand::Statusline
            | DispatchCommand::Theme { .. }
            | DispatchCommand::Keymap
            | DispatchCommand::Cheap
            | DispatchCommand::Parent
            | DispatchCommand::Router { .. }
            | DispatchCommand::Terminal) => DispatchOutcome::TuiOnly {
                command: cmd.slash_name().trim_start_matches('/').to_string(),
            },
        }
    }

    /// Convenience wrapper that parses a raw slash-prefixed string into
    /// a [`DispatchCommand`] and dispatches it. Returns
    /// [`DispatchOutcome::Unsupported`] for unrecognised heads (so the
    /// eval `unsupported_slash_command` rule keeps firing) and
    /// [`DispatchOutcome::Error`] for usage failures.
    pub async fn dispatch_command_raw(&self, raw: &str) -> DispatchOutcome {
        match DispatchCommand::parse(raw) {
            Ok(cmd) => {
                let command = cmd.slash_name();
                let arg_shape = telemetry_slash_arg_shape(&cmd);
                let outcome = self.dispatch_command(cmd).await;
                self.record_slash_command_telemetry(
                    command,
                    SlashSurface::AgentRaw,
                    telemetry_slash_outcome_from_dispatch(&outcome),
                    SlashAliasKind::Canonical,
                    arg_shape,
                );
                outcome
            }
            Err(DispatchCommandParseError::Unknown { command }) => {
                self.record_slash_command_telemetry(
                    "unknown",
                    SlashSurface::AgentRaw,
                    SlashOutcome::Unknown,
                    SlashAliasKind::Unknown,
                    SlashArgShape::Present,
                );
                DispatchOutcome::Unsupported { command }
            }
            Err(DispatchCommandParseError::Empty) => {
                self.record_slash_command_telemetry(
                    "unknown",
                    SlashSurface::AgentRaw,
                    SlashOutcome::UsageError,
                    SlashAliasKind::Unknown,
                    SlashArgShape::None,
                );
                DispatchOutcome::Error {
                    command: String::new(),
                    message: "empty command".to_string(),
                }
            }
            Err(DispatchCommandParseError::NotASlashCommand) => {
                self.record_slash_command_telemetry(
                    "unknown",
                    SlashSurface::AgentRaw,
                    SlashOutcome::UsageError,
                    SlashAliasKind::Unknown,
                    SlashArgShape::Present,
                );
                DispatchOutcome::Error {
                    command: raw.to_string(),
                    message: "expected a slash command".to_string(),
                }
            }
            Err(DispatchCommandParseError::Usage { command, hint }) => {
                self.record_slash_command_telemetry(
                    &command,
                    SlashSurface::AgentRaw,
                    SlashOutcome::UsageError,
                    SlashAliasKind::Canonical,
                    SlashArgShape::Present,
                );
                DispatchOutcome::Error {
                    command,
                    message: hint,
                }
            }
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
        // Guard against restoring a checkpoint from a different session when
        // two sessions share the same store and happen to generate the same
        // `ckpt-{generation}-{millis}` id. Legacy checkpoints written before
        // the session_id field was populated have an empty string; skip the
        // check in that case so they remain restorable.
        if let Some(session) = &self.session_log {
            let checkpoint_sid = checkpoint.session_id.as_str();
            if !checkpoint_sid.is_empty() && checkpoint_sid != session.session_id() {
                return Err(SqueezyError::Agent(format!(
                    "compaction checkpoint {} belongs to session {}, not the current session {}",
                    replacement_id,
                    checkpoint_sid,
                    session.session_id(),
                )));
            }
        }
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
                "conversation": report.post_compact,
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

        // F18: route vision-grade image bytes into an active
        // attachment that carries the raw payload (base64-stored so
        // resume stays JSON-safe) so `start_turn` can fan it out into
        // a `LlmInputItem::Image`. The provider-side
        // `ensure_vision_support` gate still runs before any HTTP
        // traffic — a text-only model surfaces a structured
        // `ProviderRequest` error on the next turn rather than
        // failing the attach.
        if kind.is_routable_image() {
            use base64::Engine as _;
            let media_type = squeezy_core::detect_image_mime(&bytes)
                .map(|mime| mime.to_string())
                .unwrap_or_else(|| "image/png".to_string());
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let preview = format!("[{media_type} attachment, {original_bytes} bytes]");
            let preview_bytes = preview.len();
            let attachment = ContextAttachment {
                id,
                source,
                kind,
                status: ContextAttachmentStatus::Attached,
                label: redacted_label,
                path: redacted_path,
                original_sha256,
                redacted_sha256: None,
                original_bytes,
                stored_bytes: original_bytes,
                preview_bytes,
                redactions: 0,
                preview,
                truncated: false,
                image_media_type: Some(media_type),
                image_data_base64: Some(encoded),
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
                "context_attached_image",
                None,
                Some(format!("attached image {}", attachment.id)),
                json!({ "attachment": attachment.clone() }),
            );
            return Ok(ContextAttachmentUpdate {
                attachment,
                duplicate: false,
                active: true,
            });
        }

        // Binary documents (PDF/DOCX/…) round-trip their raw bytes in the
        // shared base64 slot so `start_turn` can fan them into a
        // `LlmInputItem::Document`, mirroring the image path. The provider's
        // document-capability gate runs before any HTTP traffic, so a
        // provider without a document lowering surfaces a structured error
        // on the next turn rather than failing the attach.
        if kind.is_routable_document() {
            use base64::Engine as _;
            let media_type = squeezy_core::detect_binary_document_media_type(Some(&label))
                .unwrap_or("application/octet-stream")
                .to_string();
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let preview = format!("[{media_type} document, {original_bytes} bytes]");
            let preview_bytes = preview.len();
            let attachment = ContextAttachment {
                id,
                source,
                kind,
                status: ContextAttachmentStatus::Attached,
                label: redacted_label,
                path: redacted_path,
                original_sha256,
                redacted_sha256: None,
                original_bytes,
                stored_bytes: original_bytes,
                preview_bytes,
                redactions: 0,
                preview,
                truncated: false,
                image_media_type: Some(media_type),
                image_data_base64: Some(encoded),
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
                "context_attached_document",
                None,
                Some(format!("attached document {}", attachment.id)),
                json!({ "attachment": attachment.clone() }),
            );
            return Ok(ContextAttachmentUpdate {
                attachment,
                duplicate: false,
                active: true,
            });
        }

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
                image_media_type: None,
                image_data_base64: None,
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
            image_media_type: None,
            image_data_base64: None,
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

    /// Start a fresh user turn with `input` as the first user message.
    ///
    /// This is the "next_turn" leg of the three-way user-input surface:
    ///
    /// - [`Agent::next_turn`] — start a new user turn from scratch (this
    ///   method). Equivalent to [`Agent::start_turn`]; kept as a typed
    ///   alias so callers can express intent ("I am starting a new turn")
    ///   without leaking the internal `start_turn` name into call sites.
    /// - [`Agent::follow_up`] — append an additional user message to the
    ///   conversation without starting a new turn. Used when the user
    ///   wants to extend the current turn with more context.
    /// - [`Agent::steer`] — interrupt the running turn with new input.
    ///   See the doc comment on `steer` for the current behavior.
    ///
    /// Returns the same [`mpsc::Receiver<AgentEvent>`] stream that
    /// [`Agent::start_turn`] returns; callers drive the turn by
    /// consuming events from the receiver until the turn terminates.
    pub fn next_turn(
        &self,
        input: String,
        cancel: CancellationToken,
    ) -> mpsc::Receiver<AgentEvent> {
        self.start_turn(input, cancel)
    }

    /// Append an additional user message to the in-flight (or next)
    /// turn's conversation without starting a fresh turn.
    ///
    /// This is the "follow_up" leg of the three-way user-input surface
    /// (see [`Agent::next_turn`] for the full taxonomy). It pushes
    /// `text` onto the live conversation transcript so the message is
    /// visible to the model on the *current* turn's next provider call
    /// (or on the next turn, if no turn is currently running).
    ///
    /// Internally this dispatches through the same conversation-queue
    /// path as [`Agent::queue_user_message`], which the eval driver
    /// uses to script "interrupting user" behavior. The typed name is
    /// preferred at new call sites because it makes the intent
    /// ("continue the current turn") explicit.
    pub async fn follow_up(&self, text: String) {
        self.queue_user_message(text).await;
    }

    /// Interrupt the running turn with new user input and start a new
    /// turn from `input`.
    ///
    /// This is the "steer" leg of the three-way user-input surface
    /// (see [`Agent::next_turn`] for the full taxonomy). Semantically,
    /// `steer` should cancel the in-flight turn (if any) and replace
    /// it with a fresh turn whose first user message is `input`.
    ///
    /// This cancels the latest active turn's token before dispatching
    /// the replacement turn. Cancellation remains cooperative: provider
    /// streams and tools observe the token on their normal cancellation
    /// checkpoints, and the existing turn watchdog aborts the old task
    /// if it does not finish within the grace window.
    pub fn steer(&self, input: String, cancel: CancellationToken) -> mpsc::Receiver<AgentEvent> {
        self.cancel_active_turn();
        self.next_turn(input, cancel)
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
        self.start_turn_with_display_input(
            input.clone(),
            input,
            Vec::new(),
            cancel,
            response_verbosity,
        )
    }

    pub fn start_turn_with_display_input(
        &self,
        display_input: String,
        input: String,
        transient_input_items: Vec<LlmInputItem>,
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
        let subagent_catalog = self.subagent_catalog.clone();
        let hooks = self.hooks.clone();
        let background_tasks = self.background_tasks.clone();
        let routing_state = self.routing_state.clone();
        let active_turn = self.active_turn.clone();
        set_active_turn(&active_turn, turn_id, cancel.clone());
        let last_request_overhead_tokens = self.last_request_overhead_tokens.clone();
        let configured_model_context_window = self.configured_model_context_window;

        let memory_extraction = build_memory_extraction_context(
            &config,
            &provider,
            &conversation_state,
            &self.last_extracted_memory_len,
            &self.memory_notices,
            session_log.is_some(),
        );

        let turn_done = Arc::new(Notify::new());
        let turn_finished = CancellationToken::new();
        let panic_tx = tx.clone();
        let panic_session_log = session_log.clone();
        let panic_redactor = redactor.clone();
        let panic_telemetry = telemetry.clone();
        let monitor_tx = tx.clone();
        let monitor_session_log = session_log.clone();
        let monitor_redactor = redactor.clone();
        let monitor_cancel = cancel.clone();
        let turn_handle = spawn_observed_turn(
            ObservedTurnContext {
                turn_id,
                done: turn_done.clone(),
                tx: panic_tx,
                session_log: panic_session_log,
                redactor: panic_redactor,
                telemetry: panic_telemetry,
                active_turn: active_turn.clone(),
                turn_finished: turn_finished.clone(),
                memory_extraction,
            },
            async move {
                let redacted_input = redactor.redact(&input);
                let redacted_display_input = if display_input == input {
                    redacted_input.text.clone()
                } else {
                    redactor.redact(&display_input).text
                };
                let task_title = redacted_input.text.clone();
                let failure_session_log = session_log.clone();
                // Echo the user message into the TUI before kicking MCP
                // discovery so a slow/flaky external server never delays the
                // prompt the user just submitted.
                if tx
                    .send(AgentEvent::UserMessage {
                        turn_id,
                        message: TranscriptItem::user(redacted_display_input.clone()),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                refresh_mcp_tools_on_list_changed_in_background(McpListChangedRefreshContext {
                    tools: tools.clone(),
                    cancel: cancel.clone(),
                    session_log: session_log.clone(),
                    redactor: redactor.clone(),
                    tx: tx.clone(),
                    turn_id,
                    turn_finished: turn_finished.clone(),
                    background_tasks: background_tasks.clone(),
                    telemetry: telemetry.clone(),
                });
                if let Some((call, exclude_from_context)) = local_shell_command_call(&task_title) {
                    complete_local_tool_turn(
                        turn_id,
                        task_title,
                        call,
                        redacted_input.redactions,
                        exclude_from_context,
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
                    // Snapshot a short, redacted hint about the recent session
                    // (last assistant message / last tool error) so the DocHelp
                    // subagent can answer "why did that fail?"-style questions
                    // with awareness. Lock-and-release here so the conversation
                    // mutex is not held across the help turn. Curated answers
                    // ignore this field entirely.
                    let recent_context =
                        build_recent_help_context(&*conversation_state.lock().await, &redactor);
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
                            tx: tx.clone(),
                            recent_context,
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
                let mut all_tool_specs = core_control_tools(
                    &config.subagents,
                    load_session_mode(&session_mode),
                    &subagent_catalog,
                );
                all_tool_specs.extend(tools.specs().iter().cloned().map(advertised_tool));
                retain_non_excluded_tools(&mut all_tool_specs, &config.tools);
                warn_unknown_tool_schema_names(&all_tool_specs, &config.tools);
                refresh_mcp_tools_in_background(McpRefreshContext {
                    tools: tools.clone(),
                    cancel: cancel.clone(),
                    session_log: session_log.clone(),
                    redactor: redactor.clone(),
                    tx: tx.clone(),
                    turn_id,
                    background_tasks: background_tasks.clone(),
                    telemetry: telemetry.clone(),
                });

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
                    subagent_catalog,
                    hooks,
                    display_input: redacted_display_input,
                    transient_input_items,
                    routing_state,
                    active_turn,
                    last_request_overhead_tokens,
                    configured_model_context_window,
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
                    if let Some(provider_kind) = classify_provider_error(&error) {
                        telemetry.spawn(TelemetryEvent::provider_error(provider_kind));
                    }
                    let _ = tx
                        .send(AgentEvent::Failed {
                            turn_id,
                            error,
                            session_cost: None,
                        })
                        .await;
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

    fn cancel_active_turn(&self) {
        let current = self
            .active_turn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if let Some(turn) = current
            && !turn.cancel.is_cancelled()
        {
            turn.cancel.cancel();
        }
    }
}

fn set_active_turn(
    active_turn: &Arc<StdMutex<Option<ActiveTurn>>>,
    turn_id: TurnId,
    cancel: CancellationToken,
) {
    let mut slot = active_turn
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *slot = Some(ActiveTurn { turn_id, cancel });
}

fn clear_active_turn_if_current(active_turn: &Arc<StdMutex<Option<ActiveTurn>>>, turn_id: TurnId) {
    let mut slot = active_turn
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if slot
        .as_ref()
        .is_some_and(|active| active.turn_id == turn_id)
    {
        *slot = None;
    }
}

fn active_turn_is_current(
    active_turn: &Arc<StdMutex<Option<ActiveTurn>>>,
    turn_id: TurnId,
) -> bool {
    active_turn
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
        .is_some_and(|active| active.turn_id == turn_id)
}

struct ObservedTurnContext {
    turn_id: TurnId,
    done: Arc<Notify>,
    tx: mpsc::Sender<AgentEvent>,
    session_log: Option<SessionHandle>,
    redactor: Arc<Redactor>,
    telemetry: TelemetryClient,
    active_turn: Arc<StdMutex<Option<ActiveTurn>>>,
    turn_finished: CancellationToken,
    /// When `Some`, run the automatic memory-extraction pass after this
    /// top-level turn settles. `None` for subagents (they never reach here),
    /// replay, or when extraction is disabled / not viable.
    memory_extraction: Option<MemoryExtractionContext>,
}

fn spawn_observed_turn<F>(context: ObservedTurnContext, future: F) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let ObservedTurnContext {
        turn_id,
        done,
        tx,
        session_log,
        redactor,
        telemetry,
        active_turn,
        turn_finished,
        memory_extraction,
    } = context;
    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(future).catch_unwind().await;
        let panicked = outcome.is_err();
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
            if let Some(provider_kind) = classify_provider_error(&error) {
                telemetry.spawn(TelemetryEvent::provider_error(provider_kind));
            }
            let _ = tx
                .send(AgentEvent::Failed {
                    turn_id,
                    error,
                    session_cost: None,
                })
                .await;
        }
        clear_active_turn_if_current(&active_turn, turn_id);
        turn_finished.cancel();
        done.notify_waiters();
        // Automatic memory extraction runs detached, after the user has already
        // seen the turn complete — never on a panicked turn. Only top-level
        // turns reach `spawn_observed_turn`, so subagents are excluded for free.
        if !panicked && let Some(extraction) = memory_extraction {
            tokio::spawn(run_memory_extraction_task(
                extraction,
                session_log,
                redactor,
                turn_id,
            ));
        }
    })
}

/// Everything the detached memory-extraction task needs. Assembled in
/// `start_turn` only when extraction is enabled and viable.
struct MemoryExtractionContext {
    provider: Arc<dyn LlmProvider>,
    model: Arc<str>,
    workspace_root: std::path::PathBuf,
    conversation_state: Arc<Mutex<ConversationState>>,
    last_extracted_len: Arc<AtomicUsize>,
    notices: Arc<StdMutex<Vec<String>>>,
}

/// Build the extraction context iff automatic memory extraction is enabled and
/// viable: memory on (`user_memory_max_bytes > 0`, which replay also zeroes),
/// the auto-extract toggle set, a real recorded session (so unit tests with
/// mock providers never trigger an extraction LLM call), and a resolvable cheap
/// model.
/// The viability gate for automatic memory extraction, factored out so it can
/// be unit-tested directly. All four conditions must hold; the `session_log` +
/// cheap-model requirements are what keep unit tests (mock providers, no
/// session) from ever firing a real extraction LLM call.
fn memory_auto_extract_viable(
    config: &AppConfig,
    provider_name: &str,
    session_log_present: bool,
) -> bool {
    let cc = &config.context_compaction;
    cc.user_memory_max_bytes > 0
        && cc.memory_auto_extract
        && session_log_present
        && cheap_model_for(provider_name, config).is_some()
}

fn build_memory_extraction_context(
    config: &AppConfig,
    provider: &Arc<dyn LlmProvider>,
    conversation_state: &Arc<Mutex<ConversationState>>,
    last_extracted_len: &Arc<AtomicUsize>,
    notices: &Arc<StdMutex<Vec<String>>>,
    session_log_present: bool,
) -> Option<MemoryExtractionContext> {
    if !memory_auto_extract_viable(config, provider.name(), session_log_present) {
        return None;
    }
    let model = cheap_model_for(provider.name(), config)?;
    Some(MemoryExtractionContext {
        provider: provider.clone(),
        model: Arc::from(model),
        workspace_root: config.workspace_root.clone(),
        conversation_state: conversation_state.clone(),
        last_extracted_len: last_extracted_len.clone(),
        notices: notices.clone(),
    })
}

/// Did the most recent turn already curate memory via the `memory` tool? Scans
/// the conversation tail back to the last user message for a `memory`
/// save/delete call. Mirrors Claude Code's skip-if-direct-write gate: when the
/// model just wrote memory itself, the extraction pass is redundant.
fn turn_wrote_memory_inline(conversation: &[LlmInputItem]) -> bool {
    for item in conversation.iter().rev() {
        match item {
            // Reached the start of the current turn without finding a write.
            LlmInputItem::UserText(_) => return false,
            LlmInputItem::FunctionCall {
                name, arguments, ..
            } if name == "memory" => {
                let op = arguments
                    .get("op")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                if op.eq_ignore_ascii_case("save") || op.eq_ignore_ascii_case("delete") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Render new transcript items as plain `User:` / `Assistant:` lines for the
/// extraction prompt, skipping system/empty items.
fn render_transcript_slice(items: &[TranscriptItem]) -> String {
    let mut out = String::new();
    for item in items {
        let role = match item.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => continue,
        };
        let content = item.content.trim();
        if content.is_empty() {
            continue;
        }
        out.push_str(role);
        out.push_str(": ");
        out.push_str(content);
        out.push_str("\n\n");
    }
    out
}

/// The detached extraction pass: read the new transcript slice, run the cheap
/// LLM call, persist what it proposes, surface and log what changed.
async fn run_memory_extraction_task(
    ctx: MemoryExtractionContext,
    session_log: Option<SessionHandle>,
    redactor: Arc<Redactor>,
    turn_id: TurnId,
) {
    // Read the new slice under the lock (fast); release before the LLM call.
    let (slice_text, claimed_len) = {
        let state = ctx.conversation_state.lock().await;
        let total = state.transcript.len();
        // Skip-if-direct-write (mirrors Claude Code): if the model already
        // curated memory via the `memory` tool this turn, the extraction pass is
        // redundant — advance past the slice and bail rather than spend a call.
        if turn_wrote_memory_inline(&state.conversation) {
            ctx.last_extracted_len.store(total, Ordering::Relaxed);
            return;
        }
        let start = ctx.last_extracted_len.load(Ordering::Relaxed).min(total);
        let new_items = &state.transcript[start..];
        let new_user_chars: usize = new_items
            .iter()
            .filter(|item| item.role == Role::User)
            .map(|item| item.content.chars().count())
            .sum();
        if new_user_chars < memory_extraction::EXTRACTION_MIN_NEW_PROSE_CHARS {
            return;
        }
        (render_transcript_slice(new_items), total)
    };
    if slice_text.trim().is_empty() {
        return;
    }

    // The extractor sees the whole store (generously capped) so its dedup and
    // contradiction checks aren't fooled by a truncated index.
    let memory = squeezy_store::memory::Memory::new(Some(&ctx.workspace_root));
    let global_index = memory
        .global_index()
        .ok()
        .flatten()
        .and_then(|body| truncate_memory_index(body, memory_extraction::EXTRACTION_MAX_INDEX_BYTES))
        .unwrap_or_default();
    let project_index = memory
        .project_index()
        .ok()
        .flatten()
        .and_then(|body| truncate_memory_index(body, memory_extraction::EXTRACTION_MAX_INDEX_BYTES))
        .unwrap_or_default();

    // `None` means the LLM call failed — leave the high-water mark unadvanced so
    // the slice is retried next turn rather than silently dropped.
    let Some(result) = memory_extraction::run_extraction(
        &ctx.provider,
        ctx.model.clone(),
        &ctx.workspace_root,
        &slice_text,
        &global_index,
        &project_index,
    )
    .await
    else {
        return;
    };
    // The slice was handled (even if nothing was saved): advance past it.
    ctx.last_extracted_len.store(claimed_len, Ordering::Relaxed);

    if let Some(summary) = result.summary() {
        log_session_event(
            session_log.as_ref(),
            &redactor,
            "memory_extracted",
            Some(turn_id),
            Some(format!("auto-memory: {summary}")),
            json!({
                "saved": result.saved.len(),
                "deleted": result.deleted.len(),
                "skipped": result.skipped,
            }),
        );
        // Queue a quiet, user-visible line so auto-saved memory is never silent
        // — the TUI drains this on its poll loop (the turn's event channel has
        // already closed by the time this detached pass finishes).
        ctx.notices
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(format!("✎ memory: {summary}"));
    }
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
                    // Watchdog fallback: the round loop's primary cancel
                    // path normally fires `AgentEvent::Cancelled` first
                    // with its own partial-cost snapshot. This emission
                    // only runs when the grace window expires without the
                    // primary path checking in, in which case we have no
                    // cost-broker handle here — leave the cost+metrics
                    // payload zero rather than fabricate a number.
                    let _ = tx
                        .send(AgentEvent::Cancelled {
                            turn_id,
                            cost: CostSnapshot::default(),
                            metrics: TurnMetrics::default(),
                            session_cost: None,
                        })
                        .await;
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
        // The local bundled-doc pass produced an answer. If it is the explicit
        // "Not covered in local docs." sentinel and the web fallback is armed,
        // try one more pass over docs fetched from the published repo. The
        // fallback NEVER makes /help worse: on any failure it returns this same
        // local answer unchanged (and folds in whatever extra spend it made).
        if answer
            .body
            .trim()
            .starts_with(DOC_HELP_NOT_COVERED_SENTINEL)
            && run_doc_help_web_fallback_enabled(&deps.config)
        {
            return run_doc_help_web_fallback(
                task_title,
                deps,
                HelpTurnOutcome {
                    answer,
                    metrics: subagent.metrics,
                    cost: subagent.cost,
                },
            )
            .await;
        }
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

/// The DocHelp web fallback. Triggered only after the local bundled-doc DocHelp
/// pass returned the "Not covered in local docs." sentinel AND the gate
/// ([`run_doc_help_web_fallback_enabled`]) is open.
///
/// `local_outcome` is the original local answer with its already-accrued
/// metrics/cost. On ANY failure path (no doc chosen, every fetch empty/errored,
/// the second pass empty or itself the sentinel) this returns `local_outcome`
/// with whatever extra webfetch/subagent spend was incurred folded in — the
/// fallback must never degrade /help.
async fn run_doc_help_web_fallback(
    task_title: &str,
    deps: &HelpResolutionDeps,
    mut local_outcome: HelpTurnOutcome,
) -> HelpTurnOutcome {
    // The bundled docs did not cover this question, so SEARCH the published
    // repository (source code, docs, and issues) for an answer rather than
    // re-fetching the same bundled docs we already missed on. Run a web search
    // biased toward the repo, then keep only results that actually live in the
    // Squeezy repo — the model never supplies a URL, so the fetch host is
    // structurally allowlisted to the repo via the centralized slug.
    let search_call = ToolCall {
        call_id: "doc-help-web-search".to_string(),
        name: "websearch".to_string(),
        arguments: json!({ "query": squeezy_repo_search_query(task_title), "num_results": 6 }),
    };
    let search = deps.tools.execute(search_call, deps.cancel.clone()).await;
    // `execute` is the RAW executor: a missing search backend / offline / error
    // comes back as a ToolResult with status != Success, never a panic. Fold the
    // spend and degrade to the local answer when search is unavailable.
    fold_tool_result_into_metrics(&mut local_outcome.metrics, &search);
    if search.status != ToolStatus::Success {
        return local_outcome;
    }
    let mut seen_urls = std::collections::HashSet::new();
    let chosen_urls: Vec<String> = search
        .content
        .get("source_urls")
        .and_then(Value::as_array)
        .map(|urls| {
            urls.iter()
                .filter_map(Value::as_str)
                .filter(|url| is_squeezy_repo_url(url))
                .filter(|url| seen_urls.insert(url.to_string()))
                .take(2)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if chosen_urls.is_empty() {
        // No result inside the repo we trust: degrade to the local answer.
        return local_outcome;
    }

    // Fetch the chosen repo pages and assemble the corpus for the second pass.
    let mut fetched_markdown = String::new();
    let mut source_urls: Vec<String> = Vec::new();
    for url in &chosen_urls {
        let call = ToolCall {
            call_id: format!("doc-help-web-fetch-{}", source_urls.len() + 1),
            name: "webfetch".to_string(),
            arguments: json!({ "url": url, "format": "text" }),
        };
        let result = deps.tools.execute(call, deps.cancel.clone()).await;
        fold_tool_result_into_metrics(&mut local_outcome.metrics, &result);
        if result.status != ToolStatus::Success {
            continue;
        }
        let content = result
            .content
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("");
        if content.trim().is_empty() {
            continue;
        }
        if !fetched_markdown.is_empty() {
            fetched_markdown.push_str("\n\n");
        }
        fetched_markdown.push_str("---\nSOURCE: ");
        fetched_markdown.push_str(url);
        fetched_markdown.push_str("\n\n");
        fetched_markdown.push_str(content.trim());
        source_urls.push(url.clone());
    }

    // Nothing usable fetched: degrade gracefully to the original local answer.
    if source_urls.is_empty() {
        return local_outcome;
    }

    let primary_source = source_urls[0].clone();
    let prompt = doc_help_web_subagent_prompt(
        task_title,
        &fetched_markdown,
        &primary_source,
        deps.recent_context.as_deref().unwrap_or(""),
    );
    let request = SubagentRequest {
        prompt,
        scope: Some(format!(
            "External docs fetched from the published repository: {}",
            source_urls.join(", ")
        )),
        thoroughness: None,
        system_override: None,
        model_override: None,
        tool_filter: None,
    };

    // Toolless second pass — same ToolExecutionContext construction as
    // run_doc_help_subagent.
    let mut all_tool_specs = core_control_tools(
        &deps.config.subagents,
        load_session_mode(&deps.session_mode),
        &SubagentCatalog::empty(),
    );
    all_tool_specs.extend(deps.tools.specs().iter().cloned().map(advertised_tool));
    retain_non_excluded_tools(&mut all_tool_specs, &deps.config.tools);
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
        tx: deps.tx.clone(),
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
        subagent_catalog: Arc::new(SubagentCatalog::empty()),
        store: None,
        hooks: deps.hooks.clone(),
    };
    let execution = run_subagent(&parent, SubagentKind::DocHelp, request, None).await;

    // Fold the second-pass subagent spend regardless of outcome.
    local_outcome
        .metrics
        .merge_subagent_tool_metrics(&execution.metrics);
    local_outcome.metrics.subagent_calls += 1;
    if execution.status != ToolStatus::Success {
        local_outcome.metrics.subagent_failures += 1;
    }
    merge_cost(&mut local_outcome.cost, &execution.metrics.provider);

    // If the second pass is empty or itself the sentinel, keep the original
    // local answer (now carrying the extra spend).
    let body = execution.summary.trim();
    if execution.status != ToolStatus::Success
        || body.is_empty()
        || body.starts_with(DOC_HELP_NOT_COVERED_SENTINEL)
    {
        return local_outcome;
    }

    let answer = HelpAnswer {
        topic: "doc-help-web".to_string(),
        status: HelpStatus::Answered,
        body: execution.summary,
        citations: vec![HelpCitation::Url(primary_source)],
        config_sections: Vec::new(),
        source: squeezy_skills::HelpAnswerSource::DocHelpWeb,
    };
    HelpTurnOutcome {
        answer,
        metrics: local_outcome.metrics,
        cost: local_outcome.cost,
    }
}

/// Fold a single (webfetch) tool result's counters into `metrics` as
/// subagent-attributed I/O, mirroring how the DocHelp subagent's own tool
/// metrics are rolled up. The web fallback's webfetch is dispatched directly
/// (not inside a subagent loop), so it would otherwise be unaccounted.
fn fold_tool_result_into_metrics(metrics: &mut TurnMetrics, result: &ToolResult) {
    metrics.subagent_tool_calls += 1;
    metrics.subagent_bytes_read += result.cost_hint.bytes_read;
    metrics.subagent_files_scanned += result.cost_hint.files_scanned;
    metrics.subagent_model_output_bytes += result.cost_hint.output_bytes;
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
    tx: mpsc::Sender<AgentEvent>,
    /// Short, already-redacted snapshot of the recent session (most recent
    /// assistant message and/or most recent tool failure). Only the DocHelp
    /// subagent path consumes this; curated-topic answers never see it.
    /// `None` on a fresh session (no prior turns) or when nothing relevant
    /// was found. Built by [`build_recent_help_context`] at the call site so
    /// the conversation lock is released before the help turn runs.
    recent_context: Option<String>,
}

/// Maximum length of the redacted recent-session context appended to the
/// DocHelp prompt. Kept small so it never crowds out the bundled docs and so
/// it stays a hint rather than a transcript replay.
const RECENT_HELP_CONTEXT_MAX_CHARS: usize = 700;

/// Build a short, redacted snapshot of the recent conversation for the
/// DocHelp subagent: the most recent assistant message and, if present, the
/// most recent tool failure. Returns `None` when neither is available (fresh
/// session) so callers can skip the prompt section entirely.
///
/// The result is redacted via `redactor` and capped at
/// [`RECENT_HELP_CONTEXT_MAX_CHARS`] so secrets never leak into the subagent
/// prompt and the hint stays bounded.
fn build_recent_help_context(state: &ConversationState, redactor: &Redactor) -> Option<String> {
    // Most recent assistant message (skip empties from cancelled/reasoning-only
    // turns).
    let last_assistant = state.conversation.iter().rev().find_map(|item| match item {
        LlmInputItem::AssistantText(text) if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    });
    // Most recent tool failure, if any.
    let last_tool_error = state.conversation.iter().rev().find_map(|item| match item {
        LlmInputItem::FunctionCallOutput {
            output, is_error, ..
        } if *is_error && !output.trim().is_empty() => Some(output.as_str()),
        _ => None,
    });

    if last_assistant.is_none() && last_tool_error.is_none() {
        return None;
    }

    let mut section = String::new();
    if let Some(text) = last_assistant {
        section.push_str("Last assistant message: ");
        section.push_str(text.trim());
        section.push('\n');
    }
    if let Some(error) = last_tool_error {
        section.push_str("Most recent tool error: ");
        section.push_str(error.trim());
        section.push('\n');
    }

    let redacted = redactor.redact(section.trim()).text;
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Cap on char boundaries so the redacted hint stays bounded without
    // splitting a multi-byte char.
    let capped = if trimmed.chars().count() > RECENT_HELP_CONTEXT_MAX_CHARS {
        let mut out: String = trimmed
            .chars()
            .take(RECENT_HELP_CONTEXT_MAX_CHARS)
            .collect();
        out.push('…');
        out
    } else {
        trimmed.to_string()
    };
    Some(capped)
}

/// Scan `body` for inline `docs/external/<name>.md` path citations that the
/// DocHelp subagent is instructed to include, and return them as structured
/// [`HelpCitation::DocsPath`] entries (deduplicated, order-preserving).
fn extract_doc_citations_from_body(body: &str) -> Vec<HelpCitation> {
    let prefix = "docs/external/";
    let suffix = ".md";
    let mut seen = std::collections::HashSet::new();
    let mut citations = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(prefix) {
        rest = &rest[start + prefix.len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.')
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        if candidate.ends_with(suffix) {
            let path = format!("{prefix}{candidate}");
            if seen.insert(path.clone()) {
                citations.push(HelpCitation::DocsPath(path));
            }
        }
    }
    citations
}

async fn run_doc_help_subagent(task_title: &str, deps: &HelpResolutionDeps) -> DocHelpResolution {
    if !deps.config.subagents.enabled || deps.config.subagents.help_strict_local {
        return DocHelpResolution::skipped();
    }
    let config_inspect = deps.config.inspect_redacted();
    let sections = relevant_doc_sections_for_input(task_title);
    let prompt = doc_help_subagent_prompt(
        task_title,
        &config_inspect,
        &sections,
        deps.recent_context.as_deref(),
    );
    let request = SubagentRequest {
        prompt,
        scope: Some(
            "Inlined bundled docs (originally under docs/external) and the inlined redacted config inspect output."
                .to_string(),
        ),
        thoroughness: None,
        system_override: None,
        model_override: None,
        tool_filter: None,
    };
    let mut all_tool_specs = core_control_tools(
        &deps.config.subagents,
        load_session_mode(&deps.session_mode),
        &SubagentCatalog::empty(),
    );
    all_tool_specs.extend(deps.tools.specs().iter().cloned().map(advertised_tool));
    retain_non_excluded_tools(&mut all_tool_specs, &deps.config.tools);
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
        tx: deps.tx.clone(),
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
        subagent_catalog: Arc::new(SubagentCatalog::empty()),
        store: None,
        hooks: deps.hooks.clone(),
    };
    let execution = run_subagent(&parent, SubagentKind::DocHelp, request, None).await;

    let mut metrics = TurnMetrics::default();
    metrics.merge_subagent_tool_metrics(&execution.metrics);
    metrics.subagent_calls = 1;
    if execution.status != ToolStatus::Success {
        metrics.subagent_failures = 1;
    }
    let cost = execution.metrics.provider.clone();

    let answer = if execution.status == ToolStatus::Success && !execution.summary.trim().is_empty()
    {
        // Extract any "docs/external/<filename>.md" paths that the subagent cited
        // inline in its answer.  The subagent instruction asks it to cite by the
        // listed PATH labels, so this gives structured citations without extra cost.
        let citations = extract_doc_citations_from_body(&execution.summary);
        Some(HelpAnswer {
            topic: "doc-help".to_string(),
            status: HelpStatus::Answered,
            body: execution.summary,
            citations,
            config_sections: Vec::new(),
            source: squeezy_skills::HelpAnswerSource::DocHelpModel,
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

fn doc_help_subagent_prompt(
    task_title: &str,
    config_inspect: &str,
    sections: &[DocSection],
    recent_context: Option<&str>,
) -> String {
    // Inlining the bundled doc sections is what makes this subagent actually
    // work at runtime: end users run Squeezy outside the source tree, so
    // docs/external does not exist on disk for filesystem tools to find. We
    // inline only the most on-topic markdown sections (not whole docs) so the
    // model gets tight, relevant context for the help turn the user invoked.
    let mut prompt =
        String::with_capacity(config_inspect.len() + 4096 + sections_total_len(sections));
    prompt.push_str("User help request:\n");
    prompt.push_str(task_title.trim());
    // Recent session context (already redacted by the caller) goes between the
    // request and the heavy config/doc dump so the model reads it as a hint
    // about *this* session before the static corpus. Clearly delimited and
    // explicitly marked as possibly-irrelevant so the model only leans on it
    // when it actually answers the question.
    if let Some(context) = recent_context.map(str::trim).filter(|c| !c.is_empty()) {
        prompt
            .push_str("\n\nRecent session context (may or may not be relevant to the question):\n");
        prompt.push_str(context);
        prompt.push('\n');
    }
    prompt.push_str("\n\nRedacted config inspect:\n```toml\n");
    prompt.push_str(config_inspect.trim());
    prompt.push_str("\n```\n\nRelevant bundled doc sections (each block is one markdown section; cite by the listed PATH):\n");
    for section in sections {
        prompt.push_str("\n--- PATH: ");
        prompt.push_str(section.path);
        if !section.heading.is_empty() {
            prompt.push_str(" — ");
            prompt.push_str(section.heading);
        }
        prompt.push('\n');
        prompt.push_str(section.content.trim_end());
        prompt.push('\n');
    }
    prompt
}

fn sections_total_len(sections: &[DocSection]) -> usize {
    sections
        .iter()
        .map(|section| section.content.len() + section.path.len() + section.heading.len() + 24)
        .sum()
}

/// Sentinel the DocHelp subagent emits when neither the local bundled corpus
/// nor (in the web pass) the fetched docs cover the question. Trimmed equality
/// against this string is what arms the web-fallback escalation.
const DOC_HELP_NOT_COVERED_SENTINEL: &str = "Not covered in local docs.";

/// Web-search query for the fallback: the user's question biased toward the
/// published Squeezy repository so the search targets the project's own source,
/// docs, and issues rather than the whole web. Results are host-filtered to the
/// repo afterward ([`is_squeezy_repo_url`]); this only steers the search.
fn squeezy_repo_search_query(task_title: &str) -> String {
    format!("{} {}", task_title.trim(), squeezy_skills::SQUEEZY_REPO_URL)
}

/// True when `url` points inside the published Squeezy repository — the only
/// host the web fallback will fetch from. Recognizes the github.com repo URL and
/// its raw.githubusercontent.com form, both built from the centralized
/// [`squeezy_skills::SQUEEZY_REPO_SLUG`] so a repo rename is a one-line change.
fn is_squeezy_repo_url(url: &str) -> bool {
    let slug = squeezy_skills::SQUEEZY_REPO_SLUG;
    url == squeezy_skills::SQUEEZY_REPO_URL
        || url.starts_with(&format!("https://github.com/{slug}/"))
        || url.starts_with(&format!("https://raw.githubusercontent.com/{slug}/"))
}

/// Build the second-pass DocHelp prompt over docs fetched from the web. Unlike
/// [`doc_help_subagent_prompt`], the corpus here is the fetched markdown
/// (labeled with its source URL), and the instruction tells the model to answer
/// ONLY from these fetched docs, cite the source URL, and still emit the
/// "Not covered in local docs." sentinel if even these docs do not cover it.
fn doc_help_web_subagent_prompt(
    task_title: &str,
    fetched_markdown: &str,
    source_url: &str,
    recent_context: &str,
) -> String {
    let mut prompt = String::with_capacity(
        fetched_markdown.len() + recent_context.len() + source_url.len() + 1024,
    );
    prompt.push_str("User help request:\n");
    prompt.push_str(task_title.trim());
    let recent = recent_context.trim();
    if !recent.is_empty() {
        prompt.push_str("\n\nRecent conversation context (for disambiguation only):\n");
        prompt.push_str(recent);
    }
    prompt.push_str("\n\nThe local bundled documentation did not cover this question, so the following content was fetched from the LATEST state of the project's published repository (source code, docs, and issues) — it may be newer than the installed version. Answer the request using ONLY the fetched content below, and note when behavior may differ by version. Cite the source URL in your answer. If even this content does not cover the question, reply with exactly: ");
    prompt.push_str(DOC_HELP_NOT_COVERED_SENTINEL);
    prompt.push_str("\n\nFetched repository content (source: ");
    prompt.push_str(source_url);
    prompt.push_str("):\n\n");
    prompt.push_str(fetched_markdown.trim());
    prompt.push('\n');
    prompt
}

/// Pure gating predicate for the DocHelp web fallback. Off by default: every
/// condition must hold or the fallback is skipped and the original local answer
/// is returned unchanged. Factored out so the gate is unit-testable without a
/// live network.
fn run_doc_help_web_fallback_enabled(config: &AppConfig) -> bool {
    config.subagents.enabled
        && config.subagents.help_web_fallback
        && !config.subagents.help_strict_local
        && config.permissions.web != squeezy_core::PermissionMode::Deny
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
        // `merge_turn` folds the doc-help subagent's spend into the turn's
        // `provider` cost (its TurnMetrics carries the cost on `.provider`, and
        // the subagent loop records via `record_provider`, which never touches
        // the ledger). Mirror it into the per-model ledger as a MAIN-origin
        // entry keyed by the doc-help model so the `/cost` "By model" Σ stays
        // equal to the headline instead of silently dropping squeezy-help spend.
        if cost.estimated_usd_micros.is_some() || cost.input_tokens.is_some() {
            let provider = squeezy_llm::provider_name(&config.provider);
            let doc_help_model = subagent_model_for_kind(provider, &config, SubagentKind::DocHelp);
            state
                .metrics
                .model_ledger
                .record(provider, &doc_help_model, CostOrigin::Main, &cost);
        }
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
            stop_reason: None,
            reasoning_only_stop: false,
            session_cost: None,
        })
        .await;
}

async fn complete_local_tool_turn(
    turn_id: TurnId,
    task_title: String,
    call: ToolCall,
    seed_redactions: u64,
    exclude_from_context: bool,
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
            subagent_catalog: Arc::new(SubagentCatalog::empty()),
            store: None,
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
        // `!!cmd` (exclude_from_context) keeps the exchange visible in the
        // TUI transcript and the durable session log, but skips the
        // LLM-facing `conversation` so the next model round will not
        // replay the ad-hoc check the user ran as a sanity prompt.
        if !exclude_from_context {
            state.conversation.push(user_item);
            state
                .conversation
                .push(LlmInputItem::AssistantText(message.content.clone()));
        }
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
            stop_reason: None,
            reasoning_only_stop: false,
            session_cost: None,
        })
        .await;
}

struct McpRefreshContext {
    tools: ToolRegistry,
    cancel: CancellationToken,
    session_log: Option<SessionHandle>,
    redactor: Arc<Redactor>,
    tx: mpsc::Sender<AgentEvent>,
    turn_id: TurnId,
    background_tasks: Arc<StdMutex<tokio::task::JoinSet<()>>>,
    telemetry: TelemetryClient,
}

struct McpListChangedRefreshContext {
    tools: ToolRegistry,
    cancel: CancellationToken,
    session_log: Option<SessionHandle>,
    redactor: Arc<Redactor>,
    tx: mpsc::Sender<AgentEvent>,
    turn_id: TurnId,
    turn_finished: CancellationToken,
    background_tasks: Arc<StdMutex<tokio::task::JoinSet<()>>>,
    telemetry: TelemetryClient,
}

fn refresh_mcp_tools_in_background(ctx: McpRefreshContext) {
    let McpRefreshContext {
        tools,
        cancel,
        session_log,
        redactor,
        tx,
        turn_id,
        background_tasks,
        telemetry,
    } = ctx;
    let task = async move {
        let outcome = tools.refresh_mcp_tools(cancel).await;
        publish_mcp_refresh_outcome(
            &tools,
            outcome,
            &telemetry,
            session_log.as_ref(),
            &redactor,
            &tx,
            turn_id,
        )
        .await;
    };
    spawn_tracked_mcp_task(background_tasks, task);
}

fn refresh_mcp_tools_on_list_changed_in_background(ctx: McpListChangedRefreshContext) {
    let McpListChangedRefreshContext {
        tools,
        cancel,
        session_log,
        redactor,
        tx,
        turn_id,
        turn_finished,
        background_tasks,
        telemetry,
    } = ctx;
    let notify = tools.mcp_tool_list_changed_notify();
    let task = async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = turn_finished.cancelled() => break,
                _ = notify.notified() => {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let refresh_cancel = cancel.child_token();
                    let refresh = tools.refresh_mcp_tools(refresh_cancel.clone());
                    tokio::pin!(refresh);
                    let outcome = tokio::select! {
                        _ = cancel.cancelled() => {
                            refresh_cancel.cancel();
                            break;
                        }
                        _ = turn_finished.cancelled() => {
                            refresh_cancel.cancel();
                            break;
                        }
                        outcome = &mut refresh => outcome,
                    };
                    publish_mcp_refresh_outcome(
                        &tools,
                        outcome,
                        &telemetry,
                        session_log.as_ref(),
                        &redactor,
                        &tx,
                        turn_id,
                    )
                    .await;
                }
            }
        }
    };
    spawn_tracked_mcp_task(background_tasks, task);
}

async fn publish_mcp_refresh_outcome(
    tools: &ToolRegistry,
    outcome: squeezy_tools::McpRefreshOutcome,
    telemetry: &TelemetryClient,
    session_log: Option<&SessionHandle>,
    redactor: &Redactor,
    tx: &mpsc::Sender<AgentEvent>,
    turn_id: TurnId,
) {
    // Fire MCP discovery telemetry if the outcome has stats.
    if let Some(stats) = &outcome.discovery_stats {
        let (has_resources, has_elicitation, has_experimental) =
            tools.aggregate_mcp_capabilities().await;
        let mut error_kind_counts = std::collections::BTreeMap::new();
        for kind in &stats.error_kind_tokens {
            *error_kind_counts.entry(kind.clone()).or_default() += 1u64;
        }
        telemetry.spawn(TelemetryEvent::mcp_discovery(McpDiscoveryReport {
            servers_stdio: stats.servers_stdio,
            servers_http: stats.servers_http,
            servers_sse: stats.servers_sse,
            servers_enabled: stats.servers_enabled,
            servers_disabled: stats.servers_disabled,
            tools_discovered: stats.tools_discovered,
            tools_cached: stats.tools_cached,
            tools_stale_retained: stats.tools_stale_retained,
            tools_dropped_disabled: stats.tools_dropped_disabled,
            discovery_errors: stats.discovery_errors,
            error_kind_counts,
            has_resources,
            has_elicitation,
            has_experimental,
            duration_ms: stats.duration_ms,
        }));
    }
    log_session_event(
        session_log,
        redactor,
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
            session_log,
            redactor,
            "mcp_discovery_error",
            Some(turn_id),
            Some(error.clone()),
            json!({ "error": error }),
        );
    }
}

fn spawn_tracked_mcp_task<F>(background_tasks: Arc<StdMutex<tokio::task::JoinSet<()>>>, task: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    // Hand the spawn to a tracked `JoinSet` so `Agent::shutdown` can
    // wait for the spawn to drop its `Arc<SqueezyStore>` clone before
    // the agent's owner re-opens the redb store. Mutex contention here
    // can only come from a concurrent `start_turn` or a concurrent
    // shutdown drain; both windows are bounded and short, so the
    // blocking lock is safe — and falling back to an untracked spawn
    // would silently regress the lifecycle guarantee.
    match background_tasks.lock() {
        Ok(mut tasks) => {
            tasks.spawn(task);
        }
        Err(poison) => {
            poison.into_inner().spawn(task);
        }
    }
}

/// Parsed `!cmd` or `!!cmd` prompt. The second form runs identically to the
/// first (same direct-user shell call, same sandbox bypass) but its
/// transcript and tool output are kept out of the LLM-facing
/// `conversation` so ad-hoc checks like `!!git status` do not bloat
/// future requests or the prompt cache.
struct LocalShellCommand {
    command: String,
    exclude_from_context: bool,
}

fn local_shell_command_call(input: &str) -> Option<(ToolCall, bool)> {
    let LocalShellCommand {
        command,
        exclude_from_context,
    } = local_shell_command(input)?;
    let call = ToolCall {
        call_id: "local-shell-1".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": command,
            "description": "run the user-requested local command",
            "timeout_ms": LOCAL_SHELL_TIMEOUT_MS,
            "output_byte_cap": LOCAL_SHELL_OUTPUT_BYTE_CAP,
            "output_mode": "raw",
            "direct_user_shell": true,
            // Paired with the call_id prefix so a downstream caller (mock
            // provider, replay tape, future MCP shim) that mints
            // `local-shell-…` ids cannot silently bypass the sandbox by
            // toggling `direct_user_shell` alone.
            "direct_user_shell_nonce": squeezy_tools::direct_user_shell_nonce(),
        }),
    };
    Some((call, exclude_from_context))
}

fn local_shell_command(input: &str) -> Option<LocalShellCommand> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.lines().count() > 1 {
        return None;
    }
    let after_first = trimmed.strip_prefix('!')?;
    let (rest, exclude_from_context) = match after_first.strip_prefix('!') {
        Some(stripped) => (stripped, true),
        None => (after_first, false),
    };
    let command = nonempty_shell_command(rest)?;
    Some(LocalShellCommand {
        command,
        exclude_from_context,
    })
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
            let exit_code = result
                .content
                .get("exit_code")
                .and_then(Value::as_i64)
                .unwrap_or(-1);
            // Surface the effective-shell hint only when the failure looks
            // like it may be shell-syntax related: stderr was non-empty
            // (shell printed a message) or the exit code suggests the shell
            // itself failed (127 = command not found, 126 = not executable,
            // 2 = common syntax error exit in bash/sh).
            let shell_hint = if !stderr.is_empty() || matches!(exit_code, 2 | 126 | 127) {
                format!("\n{}", effective_shell_hint())
            } else {
                String::new()
            };
            if !stderr.is_empty() {
                format!("`{command}` failed: {detail}\n\n{stderr}{shell_hint}")
            } else {
                format!("`{command}` failed: {detail}{shell_hint}")
            }
        }
    }
}

/// Short hint about the effective shell used for `!cmd` / `!!cmd`
/// commands. Shown on failure so users know which shell to target and
/// that `SQUEEZY_SHELL` can override it.
///
/// The label is sourced from [`squeezy_tools::effective_shell_label`] so the
/// TUI's `/terminal` row and this hint always agree on what the user will
/// see — including empty-string and non-UTF-8 `SQUEEZY_SHELL` cases.
fn effective_shell_hint() -> String {
    let shell = squeezy_tools::effective_shell_label();
    format!("[shell: {shell} — set SQUEEZY_SHELL to change, e.g. SQUEEZY_SHELL=/bin/bash]")
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
    /// Disk-loaded custom subagent catalog shared with `Agent`. Read by the
    /// delegate dispatch to resolve an `agent:` selection and by the tool
    /// schema builder to advertise available agents.
    subagent_catalog: Arc<SubagentCatalog>,
    /// Hook registry shared with `Agent`. `None` when no hooks are
    /// installed — the per-round LLM call site checks this before
    /// building a `HookContext`.
    hooks: Option<Arc<HookRegistry>>,
    display_input: String,
    transient_input_items: Vec<LlmInputItem>,
    routing_state: Arc<StdMutex<turn_router::RoutingPersistentState>>,
    active_turn: Arc<StdMutex<Option<ActiveTurn>>>,
    /// Shared with the owning `Agent`: tokens of fixed request overhead
    /// (instructions + tool schemas) from the most recent assembled request,
    /// carried across turns so the post-turn compaction gate does not
    /// under-count the real input size (finding #2).
    last_request_overhead_tokens: Arc<AtomicU64>,
    /// The operator's explicit global `[context].model_context_window`,
    /// captured before `build()` derived a per-model window. Lets the reroute
    /// fit-check apply it as the cheap model's override fallback, mirroring how
    /// the parent model's compaction window honors it.
    configured_model_context_window: Option<u64>,
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

/// Apply the same session-scoped instruction additions used at startup.
fn append_session_instruction_blocks(
    config: &mut AppConfig,
    tools: &ToolRegistry,
    session_log: Option<&SessionHandle>,
    redactor: &Redactor,
) {
    if let Some(preamble) = tools.skills_preamble() {
        if preamble.omitted_count > 0 {
            log_session_event(
                session_log,
                redactor,
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
            session_log,
            redactor,
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
    if config.context_compaction.user_memory_max_bytes > 0 {
        // Surface the memory guidance whenever memory is enabled — even with
        // an empty index — so a first-time user is bootstrapped into the
        // save/recall loop instead of the feature lying dormant until they
        // hand-author `~/.squeezy/MEMORY.md`. The index body is appended when
        // present. Replay zeroes `user_memory_max_bytes`, so this whole block
        // is omitted there and the cached prefix stays byte-stable.
        let cap = config.context_compaction.user_memory_max_bytes;
        let global_index = ingest_user_memory(cap);
        let project_index = ingest_project_memory(&config.workspace_root, cap);
        log_session_event(
            session_log,
            redactor,
            "user_memory_ingested",
            None,
            Some(format!(
                "memory enabled; global index {} bytes, project index {} bytes",
                global_index.as_deref().map(str::len).unwrap_or(0),
                project_index.as_deref().map(str::len).unwrap_or(0),
            )),
            json!({
                "global_index_bytes": global_index.as_deref().map(str::len).unwrap_or(0),
                "project_index_bytes": project_index.as_deref().map(str::len).unwrap_or(0),
            }),
        );
        config.instructions = format!(
            "{}\n\n{}",
            config.instructions,
            memory_prompt_block(global_index.as_deref(), project_index.as_deref())
        );
    }
}

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
        let mut header_bytes = header.len().min(remaining);
        while header_bytes > 0 && !header.is_char_boundary(header_bytes) {
            header_bytes -= 1;
        }
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

/// Standing guidance for the file-based memory feature, stitched into the
/// system prompt at session start whenever memory is enabled. It teaches the
/// model the memory taxonomy, the save/recall discipline, and — critically —
/// what *not* to persist, so the `memory` tool produces a durable, high-signal
/// store instead of an activity log. Kept static so it stays in the cached
/// prompt prefix; the per-user index is appended separately by
/// [`memory_prompt_block`].
const MEMORY_PROMPT_GUIDANCE: &str = "\
You have a persistent, file-based memory that survives across sessions, split by scope between a \
global store (`~/.squeezy/`, shared across all your projects) and a per-project store \
(`<repo>/.squeezy/`, local to this repository). Curate it with the `memory` tool (`save`, `delete`, \
`list`, `read`) so future sessions start with what you have already learned. The indexes below are \
loaded every session; topic files are read on demand. Build it up over time so future conversations \
know who the user is, how they want to work with you, what to avoid or repeat, and the context behind \
their requests. If the user asks you to remember something, save it immediately as whichever type \
fits; if they ask you to forget something, delete it.

Types of memory:
- user — the user's role, expertise, and working preferences.
- feedback — guidance on how to approach work: corrections (\"no, not that\") and confirmations \
(\"yes, keep doing that\" — quieter, watch for them). Lead with the rule, then a **Why:** line and a \
**How to apply:** line. Guidance that applies only to *this repo* (a testing policy, a build \
invariant) is a `project` memory; reserve `feedback` for how to collaborate with the user in general.
- project — ongoing work, goals, or decisions not derivable from code or git history. Convert \
relative dates to absolute when saving. Add **Why:** / **How to apply:** lines.
- reference — where to find information in an external system (issue tracker, dashboard, channel).

Scope is automatic, decided by the type — you never choose a location. `user` and `feedback` are \
saved globally (they apply to you across every project); `project` and `reference` are saved to \
*this repository* only. Pick the right type and it routes itself.

How to save: one fact per file, one paragraph, named by a short slug (e.g. `prefers-bun-over-npm`). \
Don't write duplicates — `list` first and, if a memory already covers the topic, overwrite it by \
reusing its slug rather than adding a near-copy. If a new fact contradicts or supersedes an existing \
memory (the user changed their mind, a decision was reversed), overwrite that slug or `delete` it — \
never leave two memories that disagree. Delete memories that turn out to be wrong or outdated.

What NOT to save: never secrets, API keys, credentials, tokens, or personal data. Also skip code \
patterns, conventions, architecture, file paths, or project structure (re-derivable by reading the \
project); git history or who-changed-what (`git log` / `git blame` are authoritative); debugging fix \
recipes (the fix lives in the code); anything already in AGENTS.md; and ephemeral state that only \
matters this conversation. If asked to save one of these, ask what was *surprising* or *non-obvious* \
about it and save that instead.

When to access: when a memory seems relevant, or the user references prior-conversation work. You \
MUST consult memory when the user explicitly asks you to check, recall, or remember. If the user \
says to ignore memory, do not apply or cite it.

Before acting on memory: a memory naming a file, function, or flag is a claim that it existed when \
written — it may have been renamed or removed. Verify (read the file, grep the symbol) before \
recommending it. \"The memory says X exists\" is not \"X exists now.\"

Memory vs. other persistence: `notes_remember` / `notes_recall` remain available for structured, \
queryable observations within a project; reserve `memory` for the durable cross-session picture of \
the user and project. Use plans and tasks for work that only matters in the current conversation.";

/// Compose the `## Memory` system-prompt block: the standing
/// [`MEMORY_PROMPT_GUIDANCE`] plus the global and project indexes, each in its
/// own labeled subsection so the model always knows which scope a fact lives
/// in. Absent or empty indexes still render (with an "empty" note) so the model
/// knows it can start saving; this is the bootstrap path for a first-time user.
fn memory_prompt_block(global_index: Option<&str>, project_index: Option<&str>) -> String {
    format!(
        "## Memory\n\n{MEMORY_PROMPT_GUIDANCE}\n\n\
         ### Global memory (~/.squeezy/MEMORY.md)\n{}\n\n\
         ### This project's memory (<repo>/.squeezy/MEMORY.md)\n{}",
        memory_index_or_empty(global_index),
        memory_index_or_empty(project_index),
    )
}

fn memory_index_or_empty(index: Option<&str>) -> String {
    match index {
        Some(body) if !body.trim().is_empty() => body.trim().to_string(),
        _ => "(empty so far — as you save memories with the `memory` tool, their one-line \
              pointers will appear here)"
            .to_string(),
    }
}

/// Truncate an index body to `max_bytes` at a UTF-8 boundary, appending
/// `\n[truncated]` when capped. `None` for an empty/disabled body.
fn truncate_memory_index(body: String, max_bytes: usize) -> Option<String> {
    if max_bytes == 0 || body.is_empty() {
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

/// Read the global memory index `~/.squeezy/MEMORY.md` (preferred) or the
/// legacy lowercase `~/.squeezy/memory.md`, truncated to `max_bytes`. `None`
/// when disabled, `HOME` is unset, or neither file is present. Errors are
/// silent on purpose: best-effort enrichment, never load-bearing.
fn ingest_user_memory(max_bytes: usize) -> Option<String> {
    if max_bytes == 0 {
        return None;
    }
    let home = env::var_os("HOME")?;
    let dir = std::path::PathBuf::from(home).join(".squeezy");
    let body = fs::read_to_string(dir.join("MEMORY.md"))
        .or_else(|_| fs::read_to_string(dir.join("memory.md")))
        .ok()?;
    truncate_memory_index(body, max_bytes)
}

/// Read the project memory index `<workspace>/.squeezy/MEMORY.md`, truncated to
/// `max_bytes`. `None` when disabled or the file is absent. Best-effort.
fn ingest_project_memory(workspace_root: &std::path::Path, max_bytes: usize) -> Option<String> {
    if max_bytes == 0 {
        return None;
    }
    let body = squeezy_store::memory::Memory::new(Some(workspace_root))
        .project_index()
        .ok()??;
    truncate_memory_index(body, max_bytes)
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

/// G3 batching nudge appended to the system prompt when
/// `[model].batch_tool_calls_hint` is enabled. Kept to one sentence so the
/// added cache-prefix bytes stay negligible. Scoped to *read-only* lookups
/// so the model is never encouraged to reorder writes/edits, whose
/// correctness depends on sequencing.
const BATCH_TOOL_CALLS_HINT: &str = "When several read-only lookups (read_file, grep, definition_search, read_slice) are independent and none depends on another's result, issue them together in a single turn rather than one per turn; keep dependent steps and any file edits sequential.";

/// Append the G3 batching nudge when `enabled`. Off by default, so the
/// system prompt is byte-for-byte unchanged unless the operator opts in.
/// When enabled, the hint lands in a deterministic position (immediately
/// after the verbosity guidance, before the tool index) so the per-session
/// prefix stays byte-stable across rounds.
fn instructions_with_batch_hint(instructions: &str, enabled: bool) -> String {
    if !enabled {
        return instructions.to_string();
    }
    format!("{instructions}\n\n{BATCH_TOOL_CALLS_HINT}")
}

/// A short system-prompt suffix telling the model which rung of the cost ladder
/// it is running on. On the weak/medium rungs it primes the model to flag when a
/// task out-strips it (the refusal-phrase detector turns that into an automatic
/// escalation); once the strong model takes over after an escalation it is told
/// not to trust the weaker model's earlier work without re-verifying it (the
/// "Opus shouldn't blindly trust Haiku" requirement). Returns `None` for a plain
/// strong-tier turn that never routed cheap, so unrouted turns keep the exact
/// prompt — and prompt-cache prefix — they always had.
fn tier_trust_note(tier: ModelTier, started_cheap: bool) -> Option<String> {
    match tier {
        ModelTier::Weak | ModelTier::Medium => Some(format!(
            "[Routing] This turn is running on the {} model tier, chosen automatically to keep \
             routine work cheap. Do it well within your ability. If it turns out to need deeper \
             architectural reasoning, broad multi-file synthesis, or you are genuinely uncertain \
             how to proceed, say so plainly (e.g. \"I'm not sure\") rather than guessing — a \
             stronger model will automatically take over.",
            tier.label()
        )),
        ModelTier::Strong if started_cheap => Some(
            "[Routing] You are the strong model and have just taken over this turn from a cheaper, \
             weaker model that handled the earlier steps. Do not assume its edits, tool-result \
             interpretations, or conclusions are correct — re-verify the key work before relying \
             on it."
                .to_string(),
        ),
        ModelTier::Strong => None,
    }
}

impl TurnRuntime {
    /// Shared tail for a mid-turn escalation: engages the sticky window, mirrors
    /// it into `ConversationState` for resume, spawns telemetry, and emits the
    /// `TurnRouted` event. The caller updates `current_model` / `current_tier` /
    /// `on_cheap_turn` / re-arms the detector locally before calling this.
    async fn emit_escalation(
        &self,
        from_model: String,
        to_model: String,
        to_tier: ModelTier,
        reason_token: &str,
    ) {
        let sticky_remaining = {
            let mut state = self.routing_state.lock().expect("routing state lock");
            state
                .sticky
                .engage(self.config.routing.escalation_sticky_turns);
            state.sticky.remaining_turns
        };
        self.conversation_state
            .lock()
            .await
            .set_routing_sticky_remaining_turns(sticky_remaining);
        self.telemetry
            .spawn(TelemetryEvent::routing_escalated(reason_token));
        // An escalated rung uses its own tier-default effort: the judge's
        // per-task estimate was for the cheaper rung that just proved
        // insufficient, so it no longer applies.
        let effort = request_reasoning_effort_for_tier(
            &self.config,
            self.provider.name(),
            &to_model,
            to_tier,
            None,
        );
        let _ = self
            .tx
            .send(AgentEvent::TurnRouted {
                turn_id: self.turn_id,
                from: from_model,
                to: to_model,
                reason: format!("escalated_{reason_token}"),
                effort,
            })
            .await;
    }

    fn session_prompt_cache_key(&self) -> Option<String> {
        self.session_log
            .as_ref()
            .map(|handle| format!("squeezy::{}", handle.session_id()))
    }

    fn context_window_override_for_model(&self, model: &str) -> Option<u64> {
        let key = format!(
            "{}:{}",
            squeezy_core::provider_slug(&self.config.provider),
            model
        );
        self.config
            .model_limits
            .get(&key)
            .and_then(|entry| entry.context_window)
            .or(self.configured_model_context_window)
    }

    async fn provider_live_context_window_for_model(&self, model: &str) -> Option<u64> {
        match &self.config.provider {
            ProviderConfig::Ollama(ollama) => {
                fetch_ollama_context_window(&ollama.base_url, model).await
            }
            _ => None,
        }
    }

    /// Fan out a `HookPayload::PreTurn` to every registered handler.
    ///
    /// Returns the concatenation of every handler's
    /// `{"extra_instructions": "..."}` mutate field (in registration
    /// order, separated by blank lines). Callers append the returned
    /// text to the per-turn instructions so PreTurn handlers can
    /// inject preamble (timestamps, on-call context, policy reminders)
    /// without rewriting the whole instructions string. Mutate values
    /// without a string `extra_instructions` field are logged for audit
    /// and otherwise ignored. Returns `None` when no registry is
    /// configured, when the registry is empty, or when no handler
    /// proposed an extras mutation, so the no-hooks path costs zero
    /// allocations.
    fn dispatch_pre_turn(&self) -> Option<String> {
        let registry = self.hooks.as_ref()?;
        if registry.is_empty() {
            return None;
        }
        let results = registry.dispatch(HookPayload::PreTurn {
            turn_id: self.turn_id.to_string(),
        });
        let mut extra_blocks: Vec<String> = Vec::new();
        for (idx, result) in results.iter().enumerate() {
            if let Some(mutate) = result.mutate.as_ref() {
                let extracted = mutate
                    .get("extra_instructions")
                    .and_then(|value| value.as_str())
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                if let Some(text) = extracted {
                    tracing::debug!(
                        target: "squeezy::hooks",
                        turn_id = %self.turn_id,
                        handler_index = idx,
                        chars = text.chars().count(),
                        "PreTurn handler appended extra_instructions"
                    );
                    extra_blocks.push(text);
                } else {
                    tracing::debug!(
                        target: "squeezy::hooks",
                        turn_id = %self.turn_id,
                        handler_index = idx,
                        %mutate,
                        "PreTurn handler proposed an unsupported mutation shape (ignored)"
                    );
                }
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
        if extra_blocks.is_empty() {
            None
        } else {
            Some(extra_blocks.join("\n\n"))
        }
    }

    /// Fan out a `HookPayload::UserPromptSubmit` carrying the user
    /// input. Handlers can rewrite the prompt by returning
    /// `mutate = {"prompt": "..."}`; later handlers see the
    /// rewrites by earlier ones, so the final string the loop sees
    /// is the result of the whole chain.
    fn dispatch_user_prompt_submit(&self, input: String) -> String {
        let Some(registry) = self.hooks.as_ref() else {
            return input;
        };
        if registry.is_empty() {
            return input;
        }
        let mut current = input;
        let results = registry.dispatch(HookPayload::UserPromptSubmit {
            prompt: current.clone(),
            turn_id: self.turn_id.to_string(),
        });
        for (idx, result) in results.iter().enumerate() {
            if let Some(mutate) = result.mutate.as_ref() {
                let replacement = mutate
                    .get("prompt")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                if let Some(replacement) = replacement {
                    tracing::debug!(
                        target: "squeezy::hooks",
                        turn_id = %self.turn_id,
                        handler_index = idx,
                        old_chars = current.chars().count(),
                        new_chars = replacement.chars().count(),
                        "UserPromptSubmit handler rewrote prompt"
                    );
                    current = replacement;
                } else {
                    tracing::debug!(
                        target: "squeezy::hooks",
                        turn_id = %self.turn_id,
                        handler_index = idx,
                        %mutate,
                        "UserPromptSubmit handler proposed an unsupported mutation shape (ignored)"
                    );
                }
            }
            if !result.allow {
                tracing::debug!(
                    target: "squeezy::hooks",
                    turn_id = %self.turn_id,
                    handler_index = idx,
                    message = result.message.as_deref().unwrap_or(""),
                    "UserPromptSubmit handler returned allow=false (not yet enforced)"
                );
            }
        }
        current
    }

    /// Fan out a `HookPayload::SessionStart` once per session. Fires
    /// on the first turn of the session because hooks are installed
    /// via [`Agent::set_hooks`] after construction — dispatching from
    /// `Agent::new` would skip handlers the caller wires up later.
    fn dispatch_session_start(&self) {
        let Some(registry) = self.hooks.as_ref() else {
            return;
        };
        if registry.is_empty() {
            return;
        }
        let session_id = self.session_id().unwrap_or_else(|| "unknown".to_string());
        let results = registry.dispatch(HookPayload::SessionStart {
            session_id,
            reason: "turn_started".to_string(),
        });
        log_observational_results("SessionStart", self.turn_id, &results);
    }

    /// Fan out a `HookPayload::Setup` once per agent boot in this
    /// workspace. Companion to [`TurnRuntime::dispatch_session_start`]
    /// — handlers may install caches or run maintenance tasks
    /// without retripping on resumes.
    fn dispatch_setup(&self) {
        let Some(registry) = self.hooks.as_ref() else {
            return;
        };
        if registry.is_empty() {
            return;
        }
        let workspace = self.config.workspace_root.display().to_string();
        let results = registry.dispatch(HookPayload::Setup {
            workspace,
            reason: "agent_boot".to_string(),
        });
        log_observational_results("Setup", self.turn_id, &results);
    }

    /// Fan out a `HookPayload::Stop` at the very end of a turn.
    /// Audit handlers can capture turn boundaries without listening
    /// to the `AgentEvent::Completed` channel directly.
    fn dispatch_stop(&self) {
        let Some(registry) = self.hooks.as_ref() else {
            return;
        };
        if registry.is_empty() {
            return;
        }
        let results = registry.dispatch(HookPayload::Stop {
            turn_id: self.turn_id.to_string(),
        });
        log_observational_results("Stop", self.turn_id, &results);
    }

    /// Fan out a `HookPayload::PreCompact` when a hook registry is
    /// installed. `before_tokens` is the pre-compaction estimate so
    /// handlers can decide whether to log, veto (advisory today; not
    /// yet enforced), or react.
    fn dispatch_pre_compact(&self, before_tokens: u64) {
        let Some(registry) = self.hooks.as_ref() else {
            return;
        };
        if registry.is_empty() {
            return;
        }
        let results = registry.dispatch(HookPayload::PreCompact {
            turn_id: self.turn_id.to_string(),
            before_tokens,
        });
        log_observational_results("PreCompact", self.turn_id, &results);
    }

    /// Fan out a `HookPayload::PostCompact` carrying the before/after
    /// token counts so handlers can observe how much the rewrite
    /// shrank the conversation.
    fn dispatch_post_compact(&self, before_tokens: u64, after_tokens: u64) {
        let Some(registry) = self.hooks.as_ref() else {
            return;
        };
        if registry.is_empty() {
            return;
        }
        let results = registry.dispatch(HookPayload::PostCompact {
            turn_id: self.turn_id.to_string(),
            before_tokens,
            after_tokens,
        });
        log_observational_results("PostCompact", self.turn_id, &results);
    }

    async fn try_provider_context_overflow_compaction(
        &self,
        conversation: &mut Vec<LlmInputItem>,
        context_compaction: &mut ContextCompactionState,
        active_attachments: &[ContextAttachment],
        previous_response_id: &mut Option<String>,
        next_input: &mut Vec<LlmInputItem>,
    ) -> bool {
        let pre_estimate = estimate_context(conversation).estimated_tokens;
        self.dispatch_pre_compact(pre_estimate);
        let Some(report) = compact_conversation_with_strategy(
            conversation,
            context_compaction,
            active_attachments,
            self.store.as_deref(),
            &self.provider,
            self.session_log.as_ref(),
            &self.redactor,
            &self.config,
            ContextCompactionTrigger::Auto,
            true,
            0,
        )
        .await
        else {
            return false;
        };

        self.dispatch_post_compact(
            report.record.before.estimated_tokens,
            report.record.after.estimated_tokens,
        );
        self.log_event(
            "context_compacted",
            Some(self.turn_id),
            Some(format!(
                "provider overflow compacted gen={} {}->{} estimated tokens",
                report.record.generation,
                report.record.before.estimated_tokens,
                report.record.after.estimated_tokens,
            )),
            json!({
                "record": report.record,
                "summary": report.summary,
                "replacement_id": report.record.replacement_id,
                "conversation": report.post_compact,
                "phase": "provider_context_overflow",
            }),
        );
        let _ = self
            .tx
            .send(AgentEvent::ContextCompacted {
                turn_id: self.turn_id,
                report,
            })
            .await;
        *previous_response_id = None;
        *next_input = conversation.clone();
        true
    }

    /// Record that `model` overflowed at ~`observed` input tokens: clamp the
    /// per-route observed ceiling down to it. When `clamp_compaction` is set
    /// (the overflow is on the active/parent model whose window backs
    /// compaction), also tighten the live compaction window so mid/post-turn
    /// compaction sizes against the proven-smaller window for the rest of the
    /// session — keeping it consistent with what `/context` now shows.
    async fn record_observed_context_ceiling(
        &mut self,
        model: &str,
        observed: u64,
        clamp_compaction: bool,
    ) {
        {
            let mut state = self.conversation_state.lock().await;
            let key = (self.provider.name().to_string(), model.to_string());
            let ceiling = state
                .observed_context_ceilings
                .entry(key)
                .or_insert(observed);
            *ceiling = (*ceiling).min(observed);
        }
        if clamp_compaction {
            let clamped = self
                .config
                .context_compaction
                .model_context_window
                .map_or(observed, |window| window.min(observed));
            self.config.context_compaction.model_context_window = Some(clamped);
        }
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

    /// Stamp `routing_estimated_savings_usd_micros` on a turn's
    /// metrics just before they are emitted to the TUI / telemetry /
    /// session metrics. No-op for non-routed turns or when either
    /// model lacks a pricing entry in the registry. Called at every
    /// terminal-state site in the main turn loop so the event the
    /// user sees, the cumulative session counter, and the telemetry
    /// stream agree on the savings figure.
    /// Run a cheap-routed turn's work in a scoped subagent (cache isolation): the
    /// main loop never switches off the parent model, so the parent's prompt
    /// cache stays warm and a later escalation never triggers a cold full-prefix
    /// rewrite. The subagent runs the cheap model end-to-end — it can edit/run
    /// (see `SubagentKind::is_write_capable`) and its approvals reach the user —
    /// and its summary becomes the turn's answer. Returns `Some(())` when the
    /// turn was completed here; `None` to fall through to the normal in-loop
    /// cheap turn (e.g. the subagent produced nothing usable). The subagent's
    /// spend is accounted to the subagent cost slot either way.
    #[allow(clippy::too_many_arguments)]
    async fn run_isolated_cheap_turn(
        &self,
        task_title: &str,
        cheap_model: &str,
        parent_model_str: &str,
        conversation: &mut Vec<LlmInputItem>,
        broker: &mut CostBroker,
        total_cost: &mut CostSnapshot,
        user_transcript: TranscriptItem,
        context_compaction: ContextCompactionState,
    ) -> Option<()> {
        let exploration_state = Arc::new(Mutex::new(ExplorationTurnState::from_plan(None)));
        let ctx = ToolExecutionContext {
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
            exploration_state,
            subagents: self.subagents.clone(),
            subagent_catalog: self.subagent_catalog.clone(),
            store: self.store.clone(),
            hooks: self.hooks.clone(),
        };
        let request = SubagentRequest {
            prompt: task_title.to_string(),
            scope: None,
            thoroughness: None,
            system_override: None,
            // Run the exact rung the router chose (weak or mid), not always weak.
            model_override: Some(cheap_model.to_string()),
            tool_filter: None,
        };
        let execution = run_subagent(&ctx, SubagentKind::Routed, request, None).await;

        // The subagent ran and billed: account its spend to the subagent slot
        // regardless of outcome (its model differs from the parent's).
        broker
            .metrics
            .merge_subagent_tool_metrics(&execution.metrics);
        record_subagent_kind_execution(
            &mut broker.metrics,
            SubagentKind::Routed,
            &execution.metrics,
        );
        broker.metrics.model_ledger.record(
            self.provider.name(),
            &execution.model,
            CostOrigin::Subagent,
            &execution.metrics.provider,
        );

        if execution.status != ToolStatus::Success || execution.summary.trim().is_empty() {
            self.log_event(
                "routing_isolation_fallthrough",
                Some(self.turn_id),
                Some(format!(
                    "routed subagent on {cheap_model} did not complete; running the cheap turn in-loop"
                )),
                json!({ "model": cheap_model, "status": execution.status_label }),
            );
            return None;
        }

        // Success: the subagent's summary is the turn's answer. The main loop
        // never ran the parent model, so its prompt cache is untouched.
        broker.metrics.routed_to_cheap = true;
        broker.metrics.routed_to_subagent = true;
        broker.metrics.routing_cheap_main_provider = execution.metrics.provider.clone();
        self.stamp_routing_savings(&mut broker.metrics);

        // Fold the subagent's spend into the turn's headline cost and the
        // session cap basis so an isolated turn doesn't read as ~$0. Out-of-band
        // ONLY: the spend is already in the model ledger (CostOrigin::Subagent)
        // and `metrics.subagent_provider`, so we must NOT re-record the ledger or
        // `broker.metrics.provider` — that would push the `/cost` Σ above the
        // headline. Mirrors the AI-reviewer out-of-band accounting.
        merge_cost(total_cost, &execution.metrics.provider);
        broker.record_out_of_band_session_cost(
            execution.metrics.provider.estimated_usd_micros.unwrap_or(0),
        );

        let summary = execution.summary;
        conversation.push(redact_input_item(
            LlmInputItem::AssistantText(summary.clone()),
            &self.redactor,
        ));
        let message = TranscriptItem::assistant(plan_mode::strip_proposed_plan_blocks(&summary));

        self.telemetry
            .spawn(TelemetryEvent::routing_routed("routed_subagent"));
        let _ = self
            .tx
            .send(AgentEvent::TurnRouted {
                turn_id: self.turn_id,
                from: parent_model_str.to_string(),
                to: cheap_model.to_string(),
                reason: "routed_subagent".to_string(),
                effort: None,
            })
            .await;

        self.publish_terminal_task_state(TaskStateStatus::Completed, None, task_title)
            .await;
        self.persist_turn_state(TurnPersistInput {
            conversation: conversation.as_slice(),
            response_id: None,
            user: user_transcript,
            assistant: message.clone(),
            cost: &*total_cost,
            metrics: &broker.metrics,
            context_compaction,
            token_calibration: broker.calibration.clone(),
        })
        .await;
        let context_estimate = estimate_context(conversation);
        let _ = self
            .tx
            .send(AgentEvent::Completed {
                turn_id: self.turn_id,
                message,
                response_id: None,
                cost: total_cost.clone(),
                metrics: broker.metrics.clone(),
                context_estimate,
                stop_reason: Some(StopReason::EndTurn),
                reasoning_only_stop: false,
                session_cost: Some(broker.session_cost_snapshot()),
            })
            .await;
        self.finish_turn(&broker.metrics).await;
        Some(())
    }

    fn stamp_routing_savings(&self, metrics: &mut TurnMetrics) {
        if !metrics.routed_to_cheap {
            return;
        }
        let net = turn_router::estimate_routing_net_savings(
            self.provider.name(),
            &self.config.model,
            &metrics.routing_cheap_main_provider,
            metrics.routing_judge_usd_micros,
        );
        metrics.routing_estimated_net_savings_usd_micros = net;
        metrics.routing_estimated_savings_usd_micros = turn_router::estimate_routing_savings(
            self.provider.name(),
            &self.config.model,
            &metrics.routing_cheap_main_provider,
        );
        // The "always-strong" baseline: the cheap-tier work re-priced at the
        // parent model's rate. Pairs with the actual spend for an honest meter.
        metrics.routing_strong_baseline_usd_micros = estimate_cost(
            self.provider.name(),
            &self.config.model,
            &metrics.routing_cheap_main_provider,
        )
        .unwrap_or(0);
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
        // Persist errors are captured while the lock is held and used to
        // emit a session event *after* the lock drops so that
        // `append_event`'s synchronous I/O does not extend the async-mutex
        // window beyond what `write_resume_state` / `update_metadata` already
        // require.
        let mut persist_errors: Option<(Option<String>, Option<String>)> = None;
        let calibration_for_global = {
            let mut state = self.conversation_state.lock().await;
            merge_cost(&mut state.cost, cost);
            state.metrics.merge_turn(metrics);
            state.redactions += metrics.redactions;
            state.token_calibration = token_calibration.clone();
            state.set_routing_prior_turn_was_hard(
                metrics.escalated_to_parent || !metrics.routed_to_cheap,
            );
            if let Some(session) = &self.session_log {
                let resume_err = session
                    .write_resume_state(&state.to_resume_state())
                    .err()
                    .map(|e| e.to_string());
                let calibration_for_metadata = state.token_calibration.clone();
                let metadata_err = session
                    .update_metadata(|metadata| {
                        metadata.cost = state.cost.clone();
                        metadata.metrics = state.metrics.clone();
                        metadata.redactions = state.redactions;
                        if mark_resume_available {
                            metadata.resume_available = true;
                        }
                        metadata.mode = load_session_mode(&self.session_mode);
                        metadata.token_calibration = calibration_for_metadata;
                    })
                    .err()
                    .map(|e| e.to_string());
                if resume_err.is_some() || metadata_err.is_some() {
                    persist_errors = Some((resume_err, metadata_err));
                }
            }
            state.token_calibration.clone()
        };
        // Mirror the calibration into the cross-session file so brand-new
        // sessions (no resume metadata yet) seed off a recent ratio rather
        // than the per-provider defaults. Failures are silent — the global
        // file is a warm-start cache, not a source of truth.
        let _ = SessionStore::open(&self.config).save_global_calibration(&calibration_for_global);
        // Record a session event when persistence fails so that bug reports
        // carry concrete evidence without needing a provider call. On Windows
        // this surfaces file-lock failures (Defender/indexer holding the file)
        // that would otherwise silently leave /cost as live-only. Placed
        // outside the conversation_state lock so that append_event's I/O does
        // not block while the async mutex is held.
        if let (Some((resume_err, metadata_err)), Some(session)) =
            (persist_errors, &self.session_log)
        {
            let _ = session.append_event(SessionEvent::from_typed(
                SessionEventKind::Custom {
                    kind: "accounting_persistence_error".to_string(),
                    payload: serde_json::json!({
                        "resume_state_error": resume_err,
                        "metadata_error": metadata_err,
                    }),
                },
                Some(self.turn_id.to_string()),
                Some("accounting persistence failed".to_string()),
            ));
        }
    }

    /// Fold a best-effort partial cost into `total_cost` and the broker's
    /// per-turn metrics for an in-flight round that is about to exit via
    /// the cancel path. Provider streams emit usage payloads only on
    /// [`LlmEvent::Completed`]; a mid-stream cancel never reaches that
    /// arm, so without this step both the cost broker and the persisted
    /// `frames.jsonl` would report `input=0, output=0, cost=0` for the
    /// cancelled turn even though the provider did real work. The
    /// estimate is derived from the request's input byte count plus the
    /// running byte total of streamed assistant text + reasoning, fed
    /// through the per-provider calibration and the pricing registry —
    /// the same machinery [`estimate_cost`] already uses for cost
    /// rendering when a provider stream stays silent on usage. No-op
    /// when the round has done nothing observable yet (cancel landed
    /// before any provider work).
    async fn fold_partial_cancel_cost(
        &self,
        total_cost: &mut CostSnapshot,
        broker: &mut CostBroker,
        request_model: &str,
        request_input_bytes: u64,
        round_output_bytes: u64,
    ) {
        let Some(partial) = partial_cancel_cost(
            self.provider.name(),
            request_model,
            request_input_bytes,
            round_output_bytes,
            &broker.calibration,
        ) else {
            return;
        };
        // Fold into the broker so per-turn `TurnMetrics.provider` and the
        // session-level cost cap state both see the partial spend. The
        // returned `CostCapStatus` is intentionally dropped: a cancelled
        // turn already terminates the round loop, so emitting a warning
        // event here would just race the `AgentEvent::Cancelled` we are
        // about to send.
        let _ = broker.record_provider_cost(
            self.provider.name(),
            request_model,
            CostOrigin::Main,
            &partial,
        );
        merge_cost(total_cost, &partial);
    }

    /// Session-cumulative cost read from conversation state. Valid only AFTER
    /// the turn's cost has been persisted (the `persist_turn_*` calls fold this
    /// turn into `state.cost`), so the snapshot includes the just-finished
    /// turn. Used by the terminal-finish methods that have no `CostBroker`
    /// handle to put a session-cumulative cost on their event for the live
    /// status line.
    async fn persisted_session_cost(&self) -> CostSnapshot {
        self.conversation_state.lock().await.cost.clone()
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
            json!({
                "cost": cost,
                "metrics": metrics,
            }),
        );
        self.telemetry.end_turn();
        self.record_replay(SessionReplayEventKind::ModelCancelled, json!({}));
        let session_cost = self.persisted_session_cost().await;
        let _ = self
            .tx
            .send(AgentEvent::Cancelled {
                turn_id: self.turn_id,
                cost: cost.clone(),
                metrics: metrics.clone(),
                session_cost: Some(session_cost),
            })
            .await;
    }

    /// Assistant text as it should appear in the persisted/displayed
    /// transcript. The structured Plan card owns proposed-plan rendering, so a
    /// `<proposed_plan>` block is stripped while the turn ran in Plan mode.
    /// Outside Plan mode the tag is ordinary prose and survives verbatim.
    fn display_assistant_text(&self, visible: &str) -> String {
        if load_session_mode(&self.session_mode) == SessionMode::Plan {
            plan_mode::strip_proposed_plan_blocks(visible)
        } else {
            visible.to_string()
        }
    }

    /// Fail soft instead of emitting a zero-character answer.
    ///
    /// The repeated-tool-failure guard and the round-budget exhaustion path
    /// used to `return Err(SqueezyError::Agent(reason))`, which surfaces as
    /// `AgentEvent::Failed` and drops every byte the model already produced —
    /// the realworld haiku eval measured whole turns landing at 0 visible
    /// characters this way. This finalizes with the best-effort assistant
    /// text gathered so far (the running preamble plus a short note stating
    /// why the turn stopped) and emits the normal `Completed` event so the
    /// user/eval receives the partial answer rather than nothing.
    ///
    /// `assistant_text` is whatever was flushed from the in-flight stream at
    /// the abort site; it may be empty, in which case only the stop note is
    /// returned. This mirrors the success completion path (conversation push,
    /// `persist_turn_state`, `Completed`) so resume/transcript state stays
    /// consistent.
    #[allow(clippy::too_many_arguments)]
    async fn finish_soft_completion(
        &self,
        stop_note: String,
        visible_assistant_text: String,
        conversation_assistant_text: String,
        conversation: &mut Vec<LlmInputItem>,
        response_id: Option<String>,
        user_transcript: TranscriptItem,
        total_cost: CostSnapshot,
        metrics: &mut TurnMetrics,
        context_compaction: ContextCompactionState,
        token_calibration: squeezy_llm::TokenCalibration,
        stop_reason: Option<StopReason>,
        task_title: &str,
    ) {
        // Compose the visible answer: the model's own text first (if any),
        // then a one-line note explaining the early finish. When the model
        // produced no text the note stands alone so the answer is never empty.
        let trimmed = visible_assistant_text.trim_end();
        let answer = if trimmed.is_empty() {
            stop_note.clone()
        } else {
            format!("{trimmed}\n\n_(stopped early: {stop_note})_")
        };
        if !conversation_assistant_text.is_empty() {
            conversation.push(redact_input_item(
                LlmInputItem::AssistantText(conversation_assistant_text),
                &self.redactor,
            ));
        }
        let message = TranscriptItem::assistant(self.display_assistant_text(&answer));
        self.stamp_routing_savings(metrics);
        // Surface the partial-finish as Completed, not Failed: the user got
        // an answer, just an abbreviated one.
        self.publish_terminal_task_state(
            TaskStateStatus::Completed,
            Some(stop_note.clone()),
            task_title,
        )
        .await;
        self.log_event(
            "soft_completion",
            Some(self.turn_id),
            Some(stop_note.clone()),
            json!({ "reason": stop_note, "assistant_chars": answer.len() }),
        );
        self.persist_turn_state(TurnPersistInput {
            conversation,
            response_id,
            user: user_transcript,
            assistant: message.clone(),
            cost: &total_cost,
            metrics,
            context_compaction,
            token_calibration,
        })
        .await;
        let context_estimate = estimate_context(conversation);
        let _ = self
            .tx
            .send(AgentEvent::Completed {
                turn_id: self.turn_id,
                message,
                response_id: None,
                cost: total_cost,
                metrics: metrics.clone(),
                context_estimate,
                stop_reason,
                reasoning_only_stop: false,
                session_cost: Some(self.persisted_session_cost().await),
            })
            .await;
        self.finish_turn(metrics).await;
    }

    /// Preserve visible assistant text before a hard terminal failure
    /// (`max_tokens`, context overflow, refusal). Failed turns do not go
    /// through `persist_turn_state`, but resume/transcript state should still
    /// retain text the user already saw in the live stream.
    async fn preserve_visible_assistant_before_terminal_failure(
        &self,
        visible_assistant_text: String,
        conversation_assistant_text: String,
        conversation: &mut Vec<LlmInputItem>,
        user_transcript: TranscriptItem,
        context_compaction: ContextCompactionState,
    ) {
        if visible_assistant_text.trim().is_empty() {
            return;
        }
        if !conversation_assistant_text.is_empty() {
            conversation.push(redact_input_item(
                LlmInputItem::AssistantText(conversation_assistant_text),
                &self.redactor,
            ));
        }
        if !active_turn_is_current(&self.active_turn, self.turn_id) {
            return;
        }
        let mut state = self.conversation_state.lock().await;
        state.conversation = conversation.clone();
        state.transcript.push(user_transcript);
        state.transcript.push(TranscriptItem::assistant(
            self.display_assistant_text(&visible_assistant_text),
        ));
        let mut merged_compaction = context_compaction;
        merge_concurrent_pins(&mut merged_compaction, &state.context_compaction.pinned);
        state.context_compaction = merged_compaction;
        if let Some(session) = &self.session_log {
            let _ = session.write_resume_state(&state.to_resume_state());
        }
    }

    /// Mirror the success path's conversation/transcript push for a turn
    /// that was cancelled mid-stream. Without this, the partial assistant
    /// text accumulated from `AgentEvent::AssistantDelta` goes out of
    /// scope when the cancel branch returns — leaving the next turn (and
    /// `/diff`/`/undo`) with no in-conversation evidence that anything
    /// was cancelled.
    ///
    /// The partial text is pushed even when empty (the model may have
    /// been cancelled before producing any visible content) so the
    /// transcript carries a `(cancelled)` marker either way; the
    /// conversation buffer skips the push when the text is empty so we
    /// do not stuff an empty assistant turn into the provider prompt.
    async fn preserve_partial_assistant_on_cancel(
        &self,
        partial_assistant_text: String,
        conversation: &mut Vec<LlmInputItem>,
        user_transcript: TranscriptItem,
        context_compaction: ContextCompactionState,
    ) {
        if !partial_assistant_text.is_empty() {
            conversation.push(redact_input_item(
                LlmInputItem::AssistantText(partial_assistant_text.clone()),
                &self.redactor,
            ));
        }
        let assistant = TranscriptItem::assistant_cancelled(
            self.display_assistant_text(&partial_assistant_text),
        );
        if !active_turn_is_current(&self.active_turn, self.turn_id) {
            return;
        }
        let mut state = self.conversation_state.lock().await;
        state.conversation = conversation.clone();
        // `previous_response_id` is left alone: the provider-side response
        // chain must not jump past a turn we never persisted as completed.
        state.transcript.push(user_transcript);
        state.transcript.push(assistant);
        let mut merged_compaction = context_compaction;
        merge_concurrent_pins(&mut merged_compaction, &state.context_compaction.pinned);
        state.context_compaction = merged_compaction;
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

    fn record_replay_model_completed(
        &self,
        response_id: Option<String>,
        cost: &CostSnapshot,
        stop_reason: Option<&StopReason>,
        reasoning_only_stop: bool,
        retry: Option<Value>,
    ) {
        self.record_replay(
            SessionReplayEventKind::ModelCompleted,
            json!({
                "response_id": response_id,
                "cost": cost,
                "stop_reason": stop_reason,
                "reasoning_only_stop": reasoning_only_stop,
                "retry": retry,
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
    /// Bounded subagent that runs a supplied system-prompt body in
    /// isolation from the parent turn. The body is provided via
    /// `SubagentRequest.system_override` so the subagent's system prompt
    /// is the supplied instructions verbatim, and the user task is passed
    /// through the standard `prompt` field. Dispatched when a `delegate`
    /// call names a disk-loaded custom subagent (see `resolve_custom_agent`)
    /// or for a fork-mode skill body.
    Skill,
    /// Router-initiated cache-isolation worker: runs a cheap-routed turn's work
    /// end-to-end on the cheap model in its own cache namespace, so the main
    /// loop stays pinned to the parent model and the parent's prompt cache is
    /// never cold-rewritten. Write-capable (it does the actual work) and spawned
    /// directly by the turn loop, not by a model tool call.
    Routed,
}

impl SubagentKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delegate => "delegate",
            Self::Explore => "explore",
            Self::DocHelp => "doc_help",
            Self::Plan => "plan",
            Self::Review => "review",
            Self::Skill => "skill",
            Self::Routed => "routed",
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
            // Skill subagents inherit the parent model and run the
            // skill body as their system prompt; they have no role
            // overlay.
            Self::Skill => None,
            // Routed runs the parent's task as-is on the cheap model; no overlay.
            Self::Routed => None,
        }
    }

    /// Whether this is a general-purpose worker that should be able to EDIT and
    /// RUN (write/shell), not just read. `Delegate` is the broad worker the main
    /// model spawns to do scoped work end-to-end, and `Routed` runs a cheap turn
    /// end-to-end for cache isolation — both get the parent's full toolset (minus
    /// subagent-spawn/control tools) and run in the parent's session mode rather
    /// than the read-only + forced-Plan sandbox. The role-scoped kinds
    /// (Explorer/Planner/Reviewer) and DocHelp stay read-only — a reviewer or
    /// planner shouldn't be mutating the tree.
    fn is_write_capable(self) -> bool {
        matches!(self, Self::Delegate | Self::Routed)
    }
}

/// Subagent-spawning and interactive control tools a write-capable subagent
/// must NOT receive: handing it `delegate`/`explore`/… would let subagents
/// recursively spawn subagents, and the interactive control tools have no
/// meaning inside a scoped subagent. Everything else the parent advertises
/// (read, search, edit, shell, network, compiler) is fair game.
const SUBAGENT_EXCLUDED_TOOL_NAMES: &[&str] = &[
    DELEGATE_TOOL_NAME,
    EXPLORE_TOOL_NAME,
    DELEGATE_PLAN_TOOL_NAME,
    DELEGATE_REVIEW_TOOL_NAME,
    DELEGATE_CHAIN_TOOL_NAME,
    REQUEST_USER_INPUT_TOOL_NAME,
    TASK_STATE_TOOL_NAME,
    LOAD_TOOL_SCHEMA_TOOL_NAME,
];

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
    /// Disk-loaded custom subagent catalog. Read by the delegate dispatch to
    /// resolve an explicit `agent:` selection into its system prompt, model,
    /// and declared tools.
    subagent_catalog: Arc<SubagentCatalog>,
    /// Cross-session receipt store, shared with the parent `Agent`. Lets a
    /// subagent seed its dedup index from receipts the parent already
    /// committed (see `SeenToolOutputs::seeded_read_only`). `None` on paths
    /// without a persistent store (in-memory tests, the doc-help and
    /// local-tool one-shot turns).
    store: Option<Arc<SqueezyStore>>,
    all_tool_specs: &'a [AdvertisedTool],
    loaded_tool_schemas: Arc<Mutex<Vec<String>>>,
    exploration_state: Arc<Mutex<ExplorationTurnState>>,
    /// Hook registry shared with the parent `Agent` / `TurnRuntime`.
    /// `None` when no hooks are installed — `run_one_tool` checks this
    /// before building a `HookContext` so the no-hooks path costs zero
    /// allocations.
    hooks: Option<Arc<HookRegistry>>,
}

impl<'a> ToolExecutionContext<'a> {
    /// Session id derived from the session log handle, used by plan-mode
    /// path-scoped write exception (issue 17). `None` when the session
    /// has not yet been assigned an id (pre-first-turn window) or has no
    /// log handle (in-memory test scenarios).
    fn session_id_for_plan_mode(&self) -> Option<String> {
        self.session_log
            .as_ref()
            .map(|handle| handle.session_id().to_string())
    }

    /// Build a sibling `ToolExecutionContext` rooted at `cancel`.
    ///
    /// `handle_subagent_call` derives a child `CancellationToken` from
    /// the parent's token and registers it in `SubagentRegistry` as the
    /// subagent's logical cancel handle. The subagent body must run on
    /// that child token so every nested `child_token()` — for the LLM
    /// stream, downstream tool calls, and any sub-subagents — hangs off
    /// the subagent's own node in the tree. Cancelling the subagent
    /// slot then cascades into the body; cancelling the parent turn
    /// still reaches it through the child relationship.
    fn with_cancel(&self, cancel: CancellationToken) -> ToolExecutionContext<'a> {
        ToolExecutionContext {
            turn_id: self.turn_id,
            origin: self.origin,
            provider: self.provider.clone(),
            tools: self.tools,
            jobs: self.jobs,
            config: self.config,
            telemetry: self.telemetry.clone(),
            redactor: self.redactor.clone(),
            tx: self.tx.clone(),
            cancel,
            approval_ids: self.approval_ids.clone(),
            session_rules: self.session_rules.clone(),
            ai_reviewer_state: self.ai_reviewer_state.clone(),
            session_mode: self.session_mode.clone(),
            session_log: self.session_log.clone(),
            conversation_state: self.conversation_state.clone(),
            task_state: self.task_state.clone(),
            subagents: self.subagents.clone(),
            subagent_catalog: self.subagent_catalog.clone(),
            store: self.store.clone(),
            all_tool_specs: self.all_tool_specs,
            loaded_tool_schemas: self.loaded_tool_schemas.clone(),
            exploration_state: self.exploration_state.clone(),
            hooks: self.hooks.clone(),
        }
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
    context
        .tools
        .set_mcp_elicitation_policy(context.config.permissions.mcp);
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
    hooks: Option<Arc<HookRegistry>>,
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
            hooks: context.hooks.clone(),
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

    let choices: Vec<RequestUserInputChoice> = args
        .choices
        .into_iter()
        .map(|c| RequestUserInputChoice {
            label: c.label,
            value: c.value,
        })
        .collect();

    // The schema advertises an empty/omitted choices array as a request for
    // free-form input. Honour that contract so a model following it does not
    // strand the user in a modal with neither choice rows nor an answer box.
    let allow_freeform = args.allow_freeform || choices.is_empty();

    let request = RequestUserInputRequest {
        question,
        choices,
        allow_freeform,
    };
    // Capture the question contract for post-response validation; the request
    // itself is moved into the event below.
    let offered_values: Vec<String> = request.choices.iter().map(|c| c.value.clone()).collect();

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

    // Validate the response shape against the offered question contract.
    // A driver/UI that replies with a choice_value not in the offered set, or
    // a freeform reply when the question disabled freeform, must surface a
    // typed error so the model sees the violation instead of an opaque
    // success.
    match response.action {
        RequestUserInputAction::Choice => {
            let Some(choice_value) = response.choice_value.as_deref() else {
                return control_tool_result(
                    call,
                    ToolStatus::Error,
                    json!({
                        "ok": false,
                        "error": "request_user_input choice response missing choice_value"
                    }),
                );
            };
            if !offered_values.iter().any(|v| v.as_str() == choice_value) {
                return control_tool_result(
                    call,
                    ToolStatus::Error,
                    json!({
                        "ok": false,
                        "error": "choice_value not in offered choices",
                        "choice_value": choice_value,
                        "offered": offered_values,
                    }),
                );
            }
        }
        RequestUserInputAction::Freeform => {
            if !allow_freeform {
                return control_tool_result(
                    call,
                    ToolStatus::Error,
                    json!({
                        "ok": false,
                        "error": "freeform not allowed for this question",
                    }),
                );
            }
            if response
                .freeform
                .as_deref()
                .map(str::is_empty)
                .unwrap_or(true)
            {
                return control_tool_result(
                    call,
                    ToolStatus::Error,
                    json!({
                        "ok": false,
                        "error": "request_user_input freeform response missing freeform text"
                    }),
                );
            }
        }
        RequestUserInputAction::Cancelled => {}
    }

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
    /// Optional override that replaces the per-kind default system
    /// prompt produced by [`subagent_instructions`]. Used by
    /// [`SubagentKind::Skill`] so a custom subagent's `.md` body (or a
    /// fork-mode skill body) becomes the subagent's system instructions
    /// verbatim; other kinds ignore it.
    system_override: Option<String>,
    /// Explicit model the subagent must run, overriding `subagent_model_for_kind`.
    /// Set by cache-isolation (`SubagentKind::Routed`) so the routed subagent
    /// runs the exact rung the router chose (weak OR mid), not always the weak
    /// tier, and by [`resolve_custom_agent`] so a disk-loaded custom subagent
    /// runs on its declared model. `None` lets the kind's default model
    /// resolution apply.
    model_override: Option<String>,
    /// Optional allow-list of tool names declared by a disk-loaded custom
    /// subagent. When `Some`, [`subagent_allowed_tools`] intersects the
    /// kind's default toolset with these names so the custom agent never
    /// exceeds the parent's read-only delegate surface. `None` keeps the
    /// kind's full default toolset.
    tool_filter: Option<Vec<String>>,
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
    /// Workspace-relative or absolute paths the subagent read or wrote,
    /// extracted from tool-call arguments and deduped in iteration
    /// order. Lets the parent attribute work without reading the
    /// supporting-receipt SHAs.
    files_touched: Vec<String>,
    /// Full assistant + tool trace when the operator opts in via
    /// `subagents.include_transcript = true`. Empty by default so the
    /// parent-visible block stays the synthesized result, not the raw
    /// child loop history.
    transcript: Vec<Value>,
}

/// Bumps the global `subagent_calls` counter and the per-kind bucket so
/// the two stay aligned. The four audited buckets (delegate/explore/
/// plan/review) feed `/cost`-style telemetry; kinds outside that set
/// (e.g. `doc_help`) are intentionally not bucketed so the rollup matches
/// the operator-facing taxonomy.
fn record_subagent_call(metrics: &mut TurnMetrics, kind: SubagentKind) {
    metrics.subagent_calls += 1;
    if let Some(bucket) = metrics.subagent_by_kind.bucket_mut(kind.as_str()) {
        bucket.calls += 1;
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

/// Outcome of a single subagent dispatch produced by
/// [`run_subagent_dispatch`].
///
/// Carries the synthesised `ToolResult` together with the broker-mutation
/// deltas that the parent caller must fold into its `CostBroker` after
/// the dispatch resolves. Separating the work future from the broker
/// mutation lets the parent fan out multiple delegate dispatches via
/// `buffer_unordered` without two concurrent futures racing on
/// `&mut CostBroker`.
struct SubagentDispatchOutcome {
    /// The user-facing result for the parent tool loop.
    result: ToolResult,
    /// Final summary text from the subagent's execution, or empty for
    /// pre-execution failures. `delegate_chain` reads this verbatim when
    /// it substitutes `{previous}` into the next step's prompt, so the
    /// summary stays accessible without re-parsing the `ToolResult`
    /// content JSON.
    summary: String,
    /// Execution metrics from a real subagent run. `Some` only when the
    /// subagent actually ran; `None` for pre-execution failures
    /// (subagents disabled, malformed arguments, lease cap rejection).
    execution_metrics: Option<TurnMetrics>,
    /// Bump the global `subagent_failures` counter post-await.
    global_failure: bool,
    /// Bump the per-kind `bucket.failures` counter post-await. The
    /// historical lease-cap path bumps only the global counter, so this
    /// stays `false` for that branch to preserve telemetry counts.
    bucket_failure: bool,
    /// The provider the subagent ran on (the parent provider — subagents
    /// reuse the parent client). Paired with `model` to key the subagent's
    /// spend in the parent's per-model ledger.
    provider: String,
    /// The model the subagent actually ran on (resolved per kind/role; may
    /// differ from the parent model). Used only when `execution_metrics` is
    /// `Some`; empty on pre-execution failures where no run happened.
    model: String,
}

/// Apply broker-mutation deltas captured by a [`SubagentDispatchOutcome`].
///
/// Runs serially after the concurrent dispatch resolves so two parallel
/// delegate futures never race on `&mut CostBroker`.
fn apply_subagent_dispatch(
    broker: &mut CostBroker,
    kind: SubagentKind,
    outcome: &SubagentDispatchOutcome,
) {
    if let Some(metrics) = outcome.execution_metrics.as_ref() {
        broker.metrics.merge_subagent_tool_metrics(metrics);
        record_subagent_kind_execution(&mut broker.metrics, kind, metrics);
        // Attribute the subagent's whole provider spend to its own
        // `(provider, model)` under the SUBAGENT slot — the subagent may run a
        // different model than the parent (cheap tier for explore/review).
        // `outcome.model` is the *requested* model; if a provider echoes a
        // normalized id via `ServerModel`, the subagent's rounds are priced at
        // that id but the ledger keys this row under the requested alias (the
        // dollars are still correct — only the label may differ from the
        // main-agent rows, which key on the effective model).
        broker.metrics.model_ledger.record(
            &outcome.provider,
            &outcome.model,
            CostOrigin::Subagent,
            &metrics.provider,
        );
    }
    if outcome.global_failure {
        broker.metrics.subagent_failures += 1;
    }
    if outcome.bucket_failure
        && let Some(bucket) = broker.metrics.subagent_by_kind.bucket_mut(kind.as_str())
    {
        bucket.failures += 1;
    }
}

async fn handle_subagent_call(
    context: &ToolExecutionContext<'_>,
    call: &ToolCall,
    kind: SubagentKind,
    broker: &mut CostBroker,
) -> ToolResult {
    record_subagent_call(&mut broker.metrics, kind);
    let outcome = Box::pin(run_subagent_dispatch(context, call, kind)).await;
    apply_subagent_dispatch(broker, kind, &outcome);
    outcome.result
}

/// Resolve an explicit `agent:` selection on a `delegate` call into a
/// disk-loaded custom subagent.
///
/// Returns the dispatch `(kind, request)` to run. When `agent_name` is
/// `None`, or the host `kind` is anything other than `Delegate`, the pair
/// is returned unchanged. When a custom agent is selected it is dispatched
/// through [`SubagentKind::Skill`] with:
///   * `system_override` = the agent's `.md` body,
///   * `model_override` = the agent's declared model normalized via
///     [`resolve_model_alias_owned`], or the incoming per-call
///     `model_override` when the agent pins none, and
///   * `tool_filter` = the agent's declared tools (intersected with the
///     read-only delegate surface downstream in [`subagent_allowed_tools`]).
///
/// An unknown name is an error listing the available custom agent names so
/// the model can correct itself on the next round.
fn resolve_custom_agent(
    catalog: &SubagentCatalog,
    provider_name: &str,
    kind: SubagentKind,
    request: SubagentRequest,
    agent_name: Option<&str>,
) -> Result<(SubagentKind, SubagentRequest), String> {
    let Some(agent_name) = agent_name else {
        return Ok((kind, request));
    };
    // `agent:` only applies to the open-ended `delegate` tool; the planner /
    // reviewer / explore variants have fixed roles and ignore it.
    if kind != SubagentKind::Delegate {
        return Ok((kind, request));
    }
    let Some(definition) = catalog
        .user_provided()
        .find(|entry| entry.name == agent_name)
    else {
        let available: Vec<&str> = catalog
            .user_provided()
            .map(|entry| entry.name.as_str())
            .collect();
        let listing = if available.is_empty() {
            "no custom agents are defined in .squeezy/agents".to_string()
        } else {
            format!("available custom agents: {}", available.join(", "))
        };
        return Err(format!("unknown agent `{agent_name}`; {listing}"));
    };
    // The agent's own pinned model wins; otherwise keep any per-call override
    // the caller passed through `model_override`. Clone so `request` stays
    // whole for the `..request` struct update below (no partial move).
    let model_override = definition
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| resolve_model_alias_owned(provider_name, value.to_string()))
        .or_else(|| request.model_override.clone());
    let tool_filter = if definition.tools.is_empty() {
        None
    } else {
        Some(definition.tools.clone())
    };
    Ok((
        SubagentKind::Skill,
        SubagentRequest {
            system_override: Some(definition.system_prompt.clone()),
            model_override,
            tool_filter,
            ..request
        },
    ))
}

/// Run one subagent dispatch end-to-end *without* touching the broker.
///
/// Identical to the prior body of `handle_subagent_call` minus the
/// counter mutations, which are returned as a [`SubagentDispatchOutcome`]
/// for the caller to apply once the concurrent dispatch resolves. The
/// pre-call `subagent_calls` bump still happens in the caller before this
/// function is awaited so the in-flight counter is always conservative.
async fn run_subagent_dispatch(
    context: &ToolExecutionContext<'_>,
    call: &ToolCall,
    kind: SubagentKind,
) -> SubagentDispatchOutcome {
    if !context.config.subagents.enabled
        || (kind == SubagentKind::Explore && !context.config.subagents.explore_enabled)
    {
        return SubagentDispatchOutcome {
            result: subagent_control_result(
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
                    files_touched: Vec::new(),
                    transcript: Vec::new(),
                },
            ),
            summary: String::new(),
            execution_metrics: None,
            global_failure: true,
            bucket_failure: true,
            provider: context.provider.name().to_string(),
            model: String::new(),
        };
    }
    let request = match parse_subagent_request(call, kind) {
        Ok(request) => request,
        Err(error) => {
            return SubagentDispatchOutcome {
                result: subagent_control_result(
                    call,
                    kind,
                    SubagentExecution {
                        status: ToolStatus::Error,
                        summary: String::new(),
                        status_label: "invalid_request",
                        error: Some(error),
                        metrics: TurnMetrics::default(),
                        supporting_receipts: Vec::new(),
                        model: subagent_model_for_kind(
                            context.provider.name(),
                            context.config,
                            kind,
                        ),
                        structured_output: None,
                        files_touched: Vec::new(),
                        transcript: Vec::new(),
                    },
                ),
                summary: String::new(),
                execution_metrics: None,
                global_failure: true,
                bucket_failure: true,
                provider: context.provider.name().to_string(),
                model: String::new(),
            };
        }
    };
    // An explicit `agent:` on a `delegate` call selects a disk-loaded custom
    // subagent. Resolve it against the catalog: its `.md` body becomes the
    // system prompt, its `model` (when set) the model, and its declared tools
    // are intersected with the read-only delegate surface. Dispatch then runs
    // through the Skill kind, which already executes a `system_override` body.
    let agent_name = call
        .arguments
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (kind, request) = match resolve_custom_agent(
        &context.subagent_catalog,
        context.provider.name(),
        kind,
        request,
        agent_name,
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            return SubagentDispatchOutcome {
                result: subagent_control_result(
                    call,
                    kind,
                    SubagentExecution {
                        status: ToolStatus::Error,
                        summary: String::new(),
                        status_label: "invalid_request",
                        error: Some(error),
                        metrics: TurnMetrics::default(),
                        supporting_receipts: Vec::new(),
                        model: subagent_model_for_kind(
                            context.provider.name(),
                            context.config,
                            kind,
                        ),
                        structured_output: None,
                        files_touched: Vec::new(),
                        transcript: Vec::new(),
                    },
                ),
                summary: String::new(),
                execution_metrics: None,
                global_failure: true,
                bucket_failure: true,
                provider: context.provider.name().to_string(),
                model: String::new(),
            };
        }
    };
    let child_cancel = context.cancel.child_token();
    let lease = match context.subagents.start(
        kind.role().unwrap_or(SubagentRole::Explorer),
        child_cancel.clone(),
        context.config.subagents.max_concurrent.max(1),
        format!("{} starting", kind.as_str()),
    ) {
        Ok(lease) => lease,
        Err(start_error) => {
            let error_message = start_error.as_message();
            log_session_event(
                context.session_log.as_ref(),
                &context.redactor,
                "subagent_rejected",
                Some(context.turn_id),
                Some(format!("{}: {}", kind.as_str(), error_message)),
                json!({
                    "agent": kind.as_str(),
                    "reason": start_error.reason.as_str(),
                    "limit": start_error.limit,
                    "active": start_error.active,
                }),
            );
            // Bump the `failure_seen{kind=tool}` counter so dashboards
            // notice fleets that routinely hit the concurrency cap. The
            // structured `subagent_rejected` session-log event above
            // carries the specific `reason` for offline analysis; the
            // shared telemetry counter just signals "subagents are
            // being refused".
            context
                .telemetry
                .spawn(TelemetryEvent::failure_seen(ErrorKind::Tool));
            let _ = context
                .tx
                .send(AgentEvent::SubagentRejected {
                    turn_id: context.turn_id,
                    agent: kind.as_str().to_string(),
                    reason: start_error.reason,
                    limit: start_error.limit,
                    active: start_error.active,
                })
                .await;
            return SubagentDispatchOutcome {
                result: subagent_control_result(
                    call,
                    kind,
                    SubagentExecution {
                        status: ToolStatus::Denied,
                        summary: String::new(),
                        status_label: "capped",
                        error: Some(error_message),
                        metrics: TurnMetrics::default(),
                        supporting_receipts: Vec::new(),
                        model: subagent_model_for_kind(
                            context.provider.name(),
                            context.config,
                            kind,
                        ),
                        structured_output: None,
                        files_touched: Vec::new(),
                        transcript: Vec::new(),
                    },
                ),
                summary: String::new(),
                execution_metrics: None,
                global_failure: true,
                bucket_failure: false,
                provider: context.provider.name().to_string(),
                model: String::new(),
            };
        }
    };

    let started_prompt = context.redactor.redact(&request.prompt).text;
    let started_prompt_preview = compact_text(&started_prompt, 240);
    let subagent_id = lease.id;
    log_session_event(
        context.session_log.as_ref(),
        &context.redactor,
        "subagent_started",
        Some(context.turn_id),
        Some(format!("{}: {started_prompt_preview}", kind.as_str())),
        json!({
            "agent": kind.as_str(),
            "scope": request.scope,
            "thoroughness": request.thoroughness,
        }),
    );
    if let Some(registry) = context.hooks.as_ref() {
        dispatch_subagent_start(registry, context.turn_id, subagent_id, kind.as_str());
    }
    let _ = context
        .tx
        .send(AgentEvent::SubagentStarted {
            turn_id: context.turn_id,
            id: subagent_id,
            agent: kind.as_str().to_string(),
            prompt: started_prompt,
        })
        .await;

    // Root the subagent body at `child_cancel` so every nested
    // `child_token()` derives from the subagent's registered token.
    // Cancelling either the parent turn (which `child_cancel` inherits
    // from) or the subagent slot directly now cascades through its LLM
    // stream, nested tool calls, and any sub-subagents — a real
    // cancellation tree instead of a flat sibling list under the turn.
    let child_context = context.with_cancel(child_cancel.clone());
    // Emit `ToolProgress` heartbeats from the parent's perspective while
    // the subagent body runs. The subagent's first model round (just
    // reasoning, no tool calls yet) is otherwise invisible to the
    // parent's per-event timeout: `run_subagent`'s drain task only
    // forwards inner `ToolProgress` events, which fire only once the
    // subagent itself launches a tool. On a no-graph variant where the
    // subagent's allowed-tool set is whittled down to glob/grep/read_file
    // and the model spends >60s reasoning about how to substitute for
    // the missing graph tools, the eval driver's 60s `event_timeout`
    // expires and the whole turn is abandoned with $0 cost. Tick at the
    // same `TOOL_PROGRESS_INTERVAL` as a regular tool call so consumers
    // see the explore call behaving like any other long-running tool.
    let subagent_started = Instant::now();
    let progress_call_id = call.call_id.clone();
    let progress_tool_name = call.name.clone();
    let progress_tx = context.tx.clone();
    let progress_turn_id = context.turn_id;
    let subagent_future = run_subagent(&child_context, kind, request, Some(subagent_id));
    tokio::pin!(subagent_future);
    let mut progress_ticker = tokio::time::interval(TOOL_PROGRESS_INTERVAL);
    // `interval` fires immediately on first poll; skip that tick so the
    // heartbeat only fires once the subagent has actually been running.
    progress_ticker.tick().await;
    let execution = loop {
        tokio::select! {
            execution = &mut subagent_future => break execution,
            _ = progress_ticker.tick() => {
                // try_send instead of send().await: heartbeats are advisory
                // and dropping one on a full buffer is benign, but blocking
                // the select! loop on a full mpsc deadlocks the tool —
                // 6-hour Flutter SDK hang was reproduced this way.
                let _ = progress_tx.try_send(AgentEvent::ToolProgress {
                    turn_id: progress_turn_id,
                    call_id: progress_call_id.clone(),
                    tool_name: progress_tool_name.clone(),
                    elapsed_ms: subagent_started.elapsed().as_millis() as u64,
                });
            }
        }
    };
    drop(lease);
    if let Some(registry) = context.hooks.as_ref() {
        dispatch_subagent_stop(
            registry,
            context.turn_id,
            subagent_id,
            kind.as_str(),
            execution.status_label,
        );
    }
    let execution_metrics = execution.metrics.clone();
    let status_is_failure = execution.status != ToolStatus::Success;
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
                    id: subagent_id,
                    agent: kind.as_str().to_string(),
                    summary: execution.summary.clone(),
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
                    id: subagent_id,
                    agent: kind.as_str().to_string(),
                    error,
                    metrics: execution.metrics.clone(),
                })
                .await;
        }
    }

    let summary = execution.summary.clone();
    let model = execution.model.clone();
    SubagentDispatchOutcome {
        result: subagent_control_result(call, kind, execution),
        summary,
        execution_metrics: Some(execution_metrics),
        global_failure: status_is_failure,
        bucket_failure: status_is_failure,
        provider: context.provider.name().to_string(),
        model,
    }
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
    // Optional per-call model override. `delegate_chain` threads each
    // step's `model` through here so it lands on `SubagentRequest.model_override`,
    // which `run_subagent` normalizes through `resolve_model_alias_owned` and
    // runs the subagent on instead of the per-kind default. Blank → None so an
    // empty string never shadows the kind's default model.
    let model = match call.arguments.get("model") {
        Some(Value::Null) | None => None,
        Some(Value::String(value)) if value.trim().is_empty() => None,
        Some(Value::String(value)) => Some(value.trim().to_string()),
        Some(_) => return Err("model must be a string or null".to_string()),
    };
    if !matches!(kind, SubagentKind::Explore) && thoroughness.is_some() {
        return Err(format!("{} does not accept thoroughness", kind.as_str()));
    }
    // Tool-shy models (Qwen3, smaller MoEs) sometimes emit a delegate /
    // explore / doc_help call with no `prompt` field at all on simple
    // conversational turns. The old error message was a raw serde-style
    // line — `"missing required string field: prompt"` — which is
    // grammatically backwards and hard for the model to act on.
    // Returning the missing field, the kind, and an actionable hint
    // gives the next round's retry a concrete recipe.
    let prompt = match kind {
        SubagentKind::Plan => call
            .arguments
            .get("goal")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "plan subagent requires a non-empty `goal` string argument. \
                 Set `goal` to a one-sentence description of what to plan, \
                 or answer the user directly without calling plan."
                    .to_string()
            })?
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
        SubagentKind::Delegate
        | SubagentKind::Explore
        | SubagentKind::DocHelp
        | SubagentKind::Skill
        // Routed is spawned directly by the turn loop, never parsed from a tool
        // call, but the match must stay exhaustive.
        | SubagentKind::Routed => call
            .arguments
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "{kind} subagent requires a non-empty `prompt` string argument. \
                     Set `prompt` to a concrete instruction for the subagent, \
                     or answer the user directly without calling {kind}.",
                    kind = kind.as_str()
                )
            })?
            .to_string(),
    };
    Ok(SubagentRequest {
        prompt,
        scope,
        thoroughness,
        // Tool-call-driven requests never carry a system override here;
        // the delegate dispatch fills it in (along with `tool_filter`) when
        // an explicit custom `agent:` is resolved against the catalog.
        system_override: None,
        // An explicit `model` arg (e.g. a `delegate_chain` step's model)
        // overrides the per-kind default; absent/blank falls back to
        // `subagent_model_for_kind` in `run_subagent`.
        model_override: model,
        tool_filter: None,
    })
}

async fn run_subagent(
    parent: &ToolExecutionContext<'_>,
    kind: SubagentKind,
    request: SubagentRequest,
    activity_id: Option<SubagentId>,
) -> SubagentExecution {
    let mut config = parent.config.clone();
    // Read-only kinds run in the Plan sandbox (edits blocked). Write-capable
    // workers inherit the parent's session mode so they can actually edit/run;
    // their tool calls still go through the parent's permission policy.
    if !kind.is_write_capable() {
        config.session_mode = SessionMode::Plan;
    }
    config.store_responses = false;
    // Plan/Delegate/Review subagents do real agent work and should be sized
    // like the main agent. Inherit the parent's cap; only fall back to
    // `max_summary_tokens` when the parent didn't set one, so users with
    // a strict global ceiling still get that ceiling honored. DocHelp
    // keeps its own floor because its "summary" IS the user-facing
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
    // Override the inherited global reasoning effort with the spawned
    // subagent's role default before the request is built. `run_subagent_rounds`
    // reads `config.reasoning_effort` through `request_reasoning_effort`, which
    // still gates on provider/model capability downstream — so on a
    // non-reasoning provider the field is dropped exactly as the global path
    // would have dropped it.
    config.reasoning_effort = subagent_role_reasoning_effort(kind, config.reasoning_effort);
    // Subagent inherits the parent's per-round result-bytes cap directly.
    // The previous `.min(24_000)` halved the budget for a subagent that
    // already had fewer tool calls to spend.
    // An explicit `model_override` (cache-isolation passes the exact routed
    // rung) wins; otherwise resolve the kind's default model.
    let model = match request.model_override.as_deref() {
        Some(override_model) => {
            resolve_model_alias_owned(parent.provider.name(), override_model.to_string())
        }
        None => subagent_model_for_kind(parent.provider.name(), &config, kind),
    };
    config.model = model.clone();

    let allowed_tools =
        subagent_allowed_tools(parent.all_tool_specs, kind, request.tool_filter.as_deref());
    // DocHelp answers from inlined corpus, so a tool-less call is the intended
    // shape. Other subagent kinds still require at least one read-only tool.
    if allowed_tools.is_empty() && !matches!(kind, SubagentKind::DocHelp) {
        return SubagentExecution {
            status: ToolStatus::Error,
            summary: String::new(),
            status_label: "failed",
            error: Some("no tools are available to the subagent".to_string()),
            metrics: TurnMetrics::default(),
            supporting_receipts: Vec::new(),
            model,
            structured_output: None,
            files_touched: Vec::new(),
            transcript: Vec::new(),
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
    //
    // BUT: `ToolProgress` events from inside the subagent must still reach
    // the parent's `tx`. Without them, a long-running subagent (e.g.
    // explore with `DEFAULT_SUBAGENT_MAX_RUNTIME_SECS = 300`) goes silent
    // from the parent's perspective for longer than the parent's per-event
    // timeout (`event_timeout_seconds`, 60s default in the eval driver),
    // and the parent gives up on the whole turn while the subagent is
    // still alive and billing. The drain loop forwards exactly those
    // heartbeat-shaped events so the parent's timeout window resets each
    // time the subagent reports liveness; everything else is dropped to
    // keep the parent's transcript clean.
    let (hidden_tx, mut hidden_rx) = mpsc::channel::<AgentEvent>(64);
    let parent_tx = parent.tx.clone();
    let parent_turn_id = parent.turn_id;
    let activity_agent = kind.as_str().to_string();
    let drain_handle = tokio::spawn(async move {
        while let Some(event) = hidden_rx.recv().await {
            // ToolProgress heartbeats forward as-is so the parent's
            // per-event timeout window resets each time the subagent
            // reports liveness (dart hang fix; see also the try_send
            // heartbeat above and the subagent-side equivalent).
            if matches!(event, AgentEvent::ToolProgress { .. }) {
                let _ = parent_tx.try_send(event);
                continue;
            }
            // Permission prompts MUST reach the user: a write-capable subagent's
            // edit/shell tool call asks for approval by sending this event with a
            // `decision_tx` oneshot inside it, then awaits the reply. Forward it
            // to the real UI channel (await, don't drop) so the prompt renders
            // and the decision routes straight back to the waiting subagent. Read
            // -only subagents never reach an approval, so this is inert for them.
            if matches!(event, AgentEvent::ApprovalRequested { .. }) {
                let _ = parent_tx.send(event).await;
                continue;
            }
            // Other interesting events surface to the parent transcript
            // as a compact SubagentActivity line so a watching user can
            // see the subagent's tool churn without seeing its raw
            // events.
            let Some(id) = activity_id else {
                // A cache-isolated (Routed) turn carries no activity card: its
                // assistant prose IS the turn's answer, so stream each delta
                // straight to the parent transcript under the parent's turn id,
                // exactly as an in-loop turn would. All other raw events from a
                // card-less subagent stay hidden.
                if let AgentEvent::AssistantDelta { delta, .. } = event {
                    let _ = parent_tx
                        .send(AgentEvent::AssistantDelta {
                            turn_id: parent_turn_id,
                            delta,
                        })
                        .await;
                }
                continue;
            };
            // A completed tool's structured result is forwarded so the parent
            // can render it as a real rail card in the subagent's transcript
            // view; other intermediate events stay hidden to keep the parent's
            // own transcript clean.
            if let AgentEvent::ToolCallCompleted { result, .. } = event {
                let _ = parent_tx.try_send(AgentEvent::SubagentToolResult {
                    turn_id: parent_turn_id,
                    id,
                    agent: activity_agent.clone(),
                    result,
                });
            }
        }
    });
    let local_jobs = JobRegistry::new();
    let local_task_state = Arc::new(Mutex::new(None));
    let local_loaded_schemas = Arc::new(Mutex::new(Vec::new()));
    let local_mode = Arc::new(AtomicU8::new(config.session_mode.to_u8()));
    let local_exploration = Arc::new(Mutex::new(ExplorationTurnState::from_plan(None)));
    let mut seen_outputs = SeenToolOutputs::seeded_read_only(parent.store.clone());

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
    execution.files_touched = collect_files_touched(&execution.supporting_receipts);
    if config.subagents.include_transcript {
        execution.transcript = subagent_transcript(&conversation);
    }
    execution
}

/// Filters the subagent's supporting receipts to the read/edit/write
/// tools and lifts each receipt's `path` field into a deduped, order-
/// preserving list. Skips receipts without a `path` (e.g. `shell`,
/// `websearch`) and receipts whose tool was denied — they don't
/// represent files the subagent actually inspected.
fn collect_files_touched(supporting_receipts: &[Value]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut paths = Vec::new();
    for receipt in supporting_receipts {
        let tool = receipt
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(
            tool,
            "read_file"
                | "read_slice"
                | "write_file"
                | "apply_patch"
                | "glob"
                | "grep"
                | "reference_search"
                | "repo_map"
                | "hierarchy"
                | "diff_context"
        ) {
            continue;
        }
        if receipt.get("status").and_then(Value::as_str) == Some("denied") {
            continue;
        }
        let Some(path) = receipt.get("path").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(path.to_string()) {
            paths.push(path.to_string());
        }
    }
    paths
}

/// Serializes the subagent's tool-using conversation into a compact
/// array of `{role, ...}` records so an operator who opts in via
/// `subagents.include_transcript = true` can replay the child's loop.
/// Stays in the parent-visible JSON instead of a separate file so it
/// can be diffed against the synthesized summary in one place.
fn subagent_transcript(conversation: &[LlmInputItem]) -> Vec<Value> {
    conversation
        .iter()
        .map(|item| match item {
            LlmInputItem::UserText(text) => json!({ "role": "user", "text": text }),
            LlmInputItem::AssistantText(text) => json!({ "role": "assistant", "text": text }),
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => json!({
                "role": "tool_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            }),
            LlmInputItem::FunctionCallOutput {
                call_id, output, ..
            } => json!({
                "role": "tool_result",
                "call_id": call_id,
                "output": output,
            }),
            other => json!({ "role": "other", "kind": format!("{other:?}") }),
        })
        .collect()
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
        return Box::pin(run_subagent_rounds(
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
        ))
        .await;
    };
    let loop_model = model.clone();
    let timed = tokio::time::timeout(
        budget,
        Box::pin(run_subagent_rounds(
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
        )),
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
                files_touched: Vec::new(),
                transcript: Vec::new(),
            }
        }
    }
}

/// Stable prompt-cache affinity key for a single subagent invocation.
///
/// Distinct per `(session, turn)` so two subagents — or the same subagent on a
/// later turn — never share a key (which would mix unrelated prefixes). All
/// rounds of one invocation reuse this key so the provider keeps the growing
/// instructions + tools + history prefix warm across the round loop. Returns
/// `None` when no session log is attached (in-memory test contexts), in which
/// case caching falls back to disabled.
fn subagent_prompt_cache_key(parent: &ToolExecutionContext<'_>) -> Option<String> {
    parent
        .session_id_for_plan_mode()
        .map(|session_id| format!("squeezy::sub::{session_id}::{}", parent.turn_id))
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
    // Subagent rounds re-send the same instructions + tool schemas and a
    // monotonically growing conversation, so the request prefix is large and
    // stable across rounds — the prefix-cache sweet spot. Anchor a key that is
    // constant for this subagent invocation (so all its rounds share a cache
    // prefix) but distinct per turn/session (so unrelated invocations never
    // collide). `Short` retention rides the provider 5m window, which covers a
    // subagent's bounded round loop without paying for a 1h write. The helper
    // returns `None` on providers without `prompt_caching`, leaving them
    // unchanged.
    let subagent_cache_key = subagent_prompt_cache_key(parent);
    let mut context_overflow_retry_used = false;
    let mut context_compaction = ContextCompactionState::default();
    'rounds: for round in 0..config.subagents.max_model_rounds {
        let request_model: Arc<str> = Arc::from(config.model.as_str());
        let mut effective_model = Arc::clone(&request_model);
        // P1.3 fail-soft subagent input-token guard. Reuses the EXISTING
        // `max_round_input_tokens` ceiling (the same pre-flight gate the
        // parent loop applies) instead of inventing a new cap. When the
        // ceiling is unset (`None`, the default) `round_input_gate_status`
        // short-circuits and this is a no-op, so default behaviour is
        // unchanged. When a scenario/eval sets the ceiling, a subagent whose
        // assembled request would exceed it STOPS here and returns the
        // best-effort answer it has already gathered rather than running its
        // round loop out to millions of input tokens (measured: 1.2M–10.5M
        // input tokens on tasks the parent solves in ~1M). This bounds the
        // documented runaway without touching the otherwise-unbounded
        // `subagents.*` caps in squeezy-core.
        if round > 0
            && let Some(status) = round_input_gate_status(
                config.max_round_input_tokens,
                estimate_context(conversation).estimated_tokens,
                parent.provider.name(),
                &request_model,
                CostBroker::projected_output_tokens(
                    config.max_output_tokens,
                    squeezy_llm::model_info_for(parent.provider.name(), &request_model)
                        .and_then(|info| info.limits.map(|limits| limits.max_output_tokens)),
                ),
            )
        {
            let chunk = assistant_stream.finish();
            if !chunk.text.is_empty() {
                assistant_message.push_str(&chunk.text);
            }
            broker.metrics.redactions += assistant_stream.total_redactions();
            tracing::debug!(
                target: "squeezy_agent::subagent_input_gate",
                round,
                estimated_input_tokens = status.estimated_input_tokens,
                limit_tokens = status.limit_tokens,
                "subagent stopped on round-input ceiling; returning best-effort result",
            );
            return successful_subagent_execution(
                std::mem::take(assistant_message),
                broker.metrics.clone(),
                std::mem::take(supporting_receipts),
                model,
                config,
            );
        }
        let cache = CacheSpec::for_prefix_reuse(
            parent.provider.name(),
            &request_model,
            subagent_cache_key.clone(),
            CacheRetention::Short,
        );
        let llm_request = LlmRequest {
            model: Arc::clone(&request_model),
            instructions: Arc::from(instructions),
            input: Arc::from(conversation.as_slice()),
            max_output_tokens: config.max_output_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
            seed: config.seed,
            stop: config.stop.clone(),
            frequency_penalty: config.frequency_penalty,
            presence_penalty: config.presence_penalty,
            response_verbosity: request_response_verbosity(config, parent.provider.name()),
            reasoning_effort: request_reasoning_effort(config, parent.provider.name()),
            previous_response_id: None,
            cache_key: None,
            cache,
            tools: Arc::from(tool_specs),
            store: false,
            tool_choice: effective_tool_choice(config.tool_choice.as_deref(), round),
            output_schema: None,
            // G3: subagents run their own multi-round tool loop and
            // re-bill the prefix each round, so they get the same
            // operator-controlled batching opt-in. `None` keeps the
            // provider default.
            parallel_tool_calls: config.parallel_tool_calls,
            beta_headers: request_beta_headers(config, parent.provider.name()),
            ..LlmRequest::default()
        };
        let mut stream = parent
            .provider
            .stream_response(llm_request, parent.cancel.child_token());
        let mut tool_calls = Vec::new();
        let mut completed = false;
        let mut context_overflow_seen = false;
        // Accumulate model-refusal text so it can be surfaced in the error
        // when the stream closes with `StopReason::Refusal` and no tool calls.
        let mut refusal_text = String::new();
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
                    if context_overflow_seen
                        && !context_overflow_retry_used
                        && compact_conversation(
                            conversation,
                            &mut context_compaction,
                            &[],
                            None,
                            None,
                            config,
                            ContextCompactionTrigger::Auto,
                            true,
                            0,
                        )
                        .is_some()
                    {
                        context_overflow_retry_used = true;
                        continue 'rounds;
                    }
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
                        files_touched: Vec::new(),
                        transcript: Vec::new(),
                    };
                }
            };
            match event {
                LlmEvent::Started => {}
                LlmEvent::TextDelta(delta) => {
                    let chunk = assistant_stream.push(&delta);
                    if !chunk.text.is_empty() {
                        assistant_message.push_str(&chunk.text);
                        // Stream the redacted prose up to the parent so a cache-
                        // isolated (Routed) turn renders live, exactly like an
                        // in-loop turn. The drain forwards this only for the
                        // isolated turn (which has no activity card); surfaced
                        // subagents drop it behind their compact activity line.
                        // Best-effort `try_send` never stalls the stream on drain
                        // backpressure — the terminal `Completed` replaces the
                        // pending text in full, so a dropped delta costs only a
                        // little live smoothness, never correctness.
                        let _ = hidden_tx.try_send(AgentEvent::AssistantDelta {
                            turn_id: parent.turn_id,
                            delta: chunk.text,
                        });
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
                LlmEvent::Completed {
                    mut cost,
                    stop_reason,
                    ..
                } => {
                    if matches!(stop_reason, Some(StopReason::ContextWindowExceeded)) {
                        if !context_overflow_retry_used
                            && compact_conversation(
                                conversation,
                                &mut context_compaction,
                                &[],
                                None,
                                None,
                                config,
                                ContextCompactionTrigger::Auto,
                                true,
                                0,
                            )
                            .is_some()
                        {
                            context_overflow_retry_used = true;
                            continue 'rounds;
                        }
                        broker.metrics.redactions += assistant_stream.total_redactions();
                        return SubagentExecution {
                            status: ToolStatus::Error,
                            summary: String::new(),
                            status_label: "context_overflow",
                            error: Some(
                                "subagent model reported the context window was exceeded"
                                    .to_string(),
                            ),
                            metrics: broker.metrics.clone(),
                            supporting_receipts: std::mem::take(supporting_receipts),
                            model,
                            structured_output: None,
                            files_touched: Vec::new(),
                            transcript: Vec::new(),
                        };
                    }
                    // Surface model refusals so the parent can see *why* the
                    // subagent stopped instead of receiving an empty summary.
                    // Refusal prose arrives on `LlmEvent::Refusal` deltas
                    // accumulated in `refusal_text`; the `StopReason::Refusal`
                    // terminal here gates the early return.
                    //
                    // The `tool_calls.is_empty()` gate is intentional: if the
                    // provider asked for one or more tool calls and *also*
                    // signalled `Refusal`, that contradiction is resolved by
                    // executing the requested tools rather than abandoning the
                    // round, matching `TurnRuntime::run`'s behaviour.
                    if matches!(stop_reason, Some(StopReason::Refusal)) && tool_calls.is_empty() {
                        // Fold the final round's cost before returning so the
                        // parent's subagent metrics do not silently report zero
                        // cost for this round.
                        if cost.estimated_usd_micros.is_none() {
                            cost.estimated_usd_micros =
                                estimate_cost(parent.provider.name(), &effective_model, &cost);
                        }
                        broker.metrics.record_provider(&cost);
                        // Flush the assistant stream before reading
                        // `assistant_message`. `StreamRedactor::push` buffers
                        // small deltas (sub-1KiB) so a complete refusal whose
                        // prose arrived via `TextDelta` may still be sitting
                        // in the buffer at this point; without the flush, the
                        // Anthropic-style fallback below sees an empty
                        // `assistant_message`.
                        let tail = assistant_stream.finish();
                        if !tail.text.is_empty() {
                            assistant_message.push_str(&tail.text);
                        }
                        broker.metrics.redactions += assistant_stream.total_redactions();
                        // Some providers (e.g. Anthropic) emit refusal text as
                        // ordinary `TextDelta` rather than `Refusal` deltas;
                        // fall back to `assistant_message` when the dedicated
                        // refusal buffer is empty so the summary is never blank.
                        let refusal_prose: &str = if refusal_text.is_empty() {
                            assistant_message.as_str()
                        } else {
                            refusal_text.as_str()
                        };
                        let refusal_compact = compact_text(refusal_prose, 512);
                        let detail = if refusal_prose.is_empty() {
                            "subagent model refused the request".to_string()
                        } else {
                            format!("subagent model refused: {refusal_compact}")
                        };
                        return SubagentExecution {
                            status: ToolStatus::Error,
                            summary: refusal_compact,
                            status_label: "refusal",
                            error: Some(detail),
                            metrics: broker.metrics.clone(),
                            supporting_receipts: std::mem::take(supporting_receipts),
                            model,
                            structured_output: None,
                            files_touched: Vec::new(),
                            transcript: Vec::new(),
                        };
                    }
                    if cost.estimated_usd_micros.is_none() {
                        cost.estimated_usd_micros =
                            estimate_cost(parent.provider.name(), &effective_model, &cost);
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
                        files_touched: Vec::new(),
                        transcript: Vec::new(),
                    };
                }
                LlmEvent::ContextOverflow { .. } => {
                    context_overflow_seen = true;
                }
                LlmEvent::ServerModel(model) => {
                    effective_model = Arc::from(model);
                }
                // Accumulate refusal text so it can surface in the subagent
                // error when the stream terminates with StopReason::Refusal.
                LlmEvent::Refusal { content } => {
                    refusal_text.push_str(&content);
                }
                // `Citation` sources and `ToolCallDelta` incremental args
                // have no sink in the subagent accumulator; `ToolCallDelta`
                // is superseded by the canonical `ToolCall` event. Named
                // explicitly so the wildcard stays reserved for genuinely
                // unknown future variants.
                LlmEvent::Citation { .. } | LlmEvent::ToolCallDelta { .. } => {}
                // `LlmEvent` is `#[non_exhaustive]`; unknown future variants
                // flow past without disturbing the subagent round — they
                // get a dedicated arm once consumers are taught about them.
                _ => {}
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
                Box::pin(execute_tool_calls(
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
                        subagent_catalog: parent.subagent_catalog.clone(),
                        store: parent.store.clone(),
                        hooks: parent.hooks.clone(),
                    },
                    broker,
                ))
                .await,
            );
        }
        let results = seen_outputs.prepare_results(results);
        let results = pack_tool_results(results, config.max_tool_result_bytes_per_round);
        // Look up each result's originating call by `call_id` so the
        // supporting receipt can carry a `path` field for the parent's
        // `files_touched` summary. Lookup is a linear scan over the
        // round's tool calls (always small — bounded by the model's
        // parallel-tool-calls cap), so the extra cost is negligible.
        for pending in &results {
            broker.record_model_result(&pending.result);
            if supporting_receipts.len() < 12 {
                let path = tool_calls
                    .iter()
                    .find(|call| call.call_id == pending.result.call_id)
                    .and_then(subagent_tool_call_path);
                supporting_receipts.push(subagent_supporting_receipt(
                    &pending.result,
                    path.as_deref(),
                ));
            }
        }
        // Index this round's outputs so a later round collapses a repeat
        // read/grep into a receipt stub instead of re-billing the bytes,
        // and so dedup also fires against the parent receipts preloaded at
        // construction. Read-only seed (`seeded_read_only`), so this only
        // updates the in-memory index — it never writes to the store.
        seen_outputs.remember_results(&results);
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
                content_parts: None,
                is_error: tool_status_is_model_error(pending.result.status),
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
        files_touched: Vec::new(),
        transcript: Vec::new(),
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
        files_touched: Vec::new(),
        transcript: Vec::new(),
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
    let provider = &execution.metrics.provider;
    let mut content = json!({
        "ok": execution.status == ToolStatus::Success,
        "agent": kind.as_str(),
        "status": execution.status_label,
        "summary": execution.summary,
        "model": execution.model,
        "supporting_receipts": execution.supporting_receipts,
        "files_touched": execution.files_touched,
        "cost": provider,
        // Cache breakdown promoted to a top-level block so the parent
        // can answer "how much of this subagent's input was a cache
        // hit?" without reaching into the nested `cost` map.
        "cache": {
            "input_tokens": provider.input_tokens,
            "output_tokens": provider.output_tokens,
            "cached_input_tokens": provider.cached_input_tokens,
            "cache_write_input_tokens": provider.cache_write_input_tokens,
        },
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
    if !execution.transcript.is_empty() {
        content["transcript"] = json!(execution.transcript);
    }
    control_tool_result(call, execution.status, content)
}

/// One parsed step inside a `delegate_chain` invocation. Mirrors the
/// shape of the advertised schema in [`delegate_chain_advertised_tool`].
///
/// `model` is the optional per-step override. When set, the dispatcher
/// threads it into the step's `delegate` sub-call, where it round-trips
/// through `parse_subagent_request` → `SubagentRequest.model_override` →
/// `run_subagent` (normalized via `resolve_model_alias_owned`); when
/// `None` the step falls back to the parent's delegate model from
/// `subagent_model_for_kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DelegateChainStep {
    prompt: String,
    scope: Option<String>,
    model: Option<String>,
}

/// Parse the `steps` array of a `delegate_chain` call into typed
/// [`DelegateChainStep`]s. Returns an actionable error message that
/// surfaces to the model when the contract is violated (missing/empty
/// `steps`, non-string fields, more than [`DELEGATE_CHAIN_MAX_STEPS`]).
///
/// Validation runs before any subagent leases are taken so a malformed
/// chain never consumes the per-kind concurrency budget or bumps
/// subagent counters mid-way through the chain.
fn parse_delegate_chain_steps(call: &ToolCall) -> Result<Vec<DelegateChainStep>, String> {
    let steps_value = call.arguments.get("steps").ok_or_else(|| {
        "delegate_chain requires a `steps` array of `{prompt, model?, scope?}` objects.".to_string()
    })?;
    let steps_array = steps_value.as_array().ok_or_else(|| {
        "delegate_chain `steps` must be an array of `{prompt, model?, scope?}` objects.".to_string()
    })?;
    if steps_array.is_empty() {
        return Err("delegate_chain `steps` must contain at least one step.".to_string());
    }
    if steps_array.len() > DELEGATE_CHAIN_MAX_STEPS {
        return Err(format!(
            "delegate_chain `steps` may not exceed {DELEGATE_CHAIN_MAX_STEPS} steps, got {len}.",
            len = steps_array.len()
        ));
    }
    let mut steps = Vec::with_capacity(steps_array.len());
    for (idx, raw) in steps_array.iter().enumerate() {
        let object = raw.as_object().ok_or_else(|| {
            format!(
                "delegate_chain step {idx} must be a JSON object with a required `prompt` field."
            )
        })?;
        let prompt = object
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    "delegate_chain step {idx} requires a non-empty string `prompt`. The prompt may include `{placeholder}` to substitute the prior step's summary.",
                    placeholder = DELEGATE_CHAIN_PREVIOUS_PLACEHOLDER,
                )
            })?
            .to_string();
        let scope = match object.get("scope") {
            Some(Value::Null) | None => None,
            Some(Value::String(value)) if value.trim().is_empty() => None,
            Some(Value::String(value)) => Some(value.trim().to_string()),
            Some(_) => {
                return Err(format!(
                    "delegate_chain step {idx} `scope` must be a string or null."
                ));
            }
        };
        let model = match object.get("model") {
            Some(Value::Null) | None => None,
            Some(Value::String(value)) if value.trim().is_empty() => None,
            Some(Value::String(value)) => Some(value.trim().to_string()),
            Some(_) => {
                return Err(format!(
                    "delegate_chain step {idx} `model` must be a string or null."
                ));
            }
        };
        steps.push(DelegateChainStep {
            prompt,
            scope,
            model,
        });
    }
    Ok(steps)
}

/// Substitute every literal occurrence of [`DELEGATE_CHAIN_PREVIOUS_PLACEHOLDER`]
/// in `template` with `previous`.
///
/// Done verbatim — no regex, no formatting — so a step that does not
/// mention `{previous}` stays byte-identical and a step that mentions it
/// multiple times sees every instance replaced. The first step's
/// `previous` is the empty string for the leading step, so the
/// placeholder collapses to nothing instead of leaving a stray literal.
fn chain_substitute_previous(template: &str, previous: &str) -> String {
    template.replace(DELEGATE_CHAIN_PREVIOUS_PLACEHOLDER, previous)
}

/// Roll one step's [`TurnMetrics`] into a chain-wide aggregate so the
/// chain's synthesised `subagent_control_result` reports total tool /
/// I/O / cost across every step. The per-step metrics are already merged
/// into the parent broker via `apply_subagent_dispatch`; this aggregate
/// is purely for the chain's own JSON payload.
fn chain_accumulate_metrics(total: &mut TurnMetrics, step: &TurnMetrics) {
    total.tool_calls += step.tool_calls;
    total.tool_successes += step.tool_successes;
    total.tool_errors += step.tool_errors;
    total.tool_denials += step.tool_denials;
    total.tool_cancellations += step.tool_cancellations;
    total.files_scanned += step.files_scanned;
    total.bytes_read += step.bytes_read;
    total.matches_returned += step.matches_returned;
    total.model_output_bytes += step.model_output_bytes;
    total.receipt_stub_hits += step.receipt_stub_hits;
    total.negative_receipt_hits += step.negative_receipt_hits;
    total.spill_writes += step.spill_writes;
    total.spill_reads += step.spill_reads;
    total.budget_denials += step.budget_denials;
    total.redactions += step.redactions;
    total.record_provider(&step.provider);
}

/// Execute a `delegate_chain` call sequentially.
///
/// Each step is dispatched through [`run_subagent_dispatch`] as a
/// `Delegate` subagent. The chain threads `{previous}` substitution
/// between steps, aborts on the first non-success step, and synthesises
/// an aggregate [`SubagentExecution`] so the parent's tool loop receives
/// a single `subagent_control_result` describing the full chain.
///
/// Broker mutations are applied serially per step (chain runs in the
/// validation loop, not the concurrent delegate batch) so the broker's
/// per-kind bucket counts every chained subagent invocation even when
/// the chain aborts mid-way.
async fn handle_delegate_chain_call(
    context: &ToolExecutionContext<'_>,
    call: &ToolCall,
    broker: &mut CostBroker,
) -> ToolResult {
    let steps = match parse_delegate_chain_steps(call) {
        Ok(steps) => steps,
        Err(error) => {
            // Mirror the `invalid_request` shape from `run_subagent_dispatch`
            // so the model sees the same envelope as a malformed `delegate`
            // call. No broker mutations on this path — the parse failed
            // before any subagent was started.
            return subagent_control_result(
                call,
                SubagentKind::Delegate,
                SubagentExecution {
                    status: ToolStatus::Error,
                    summary: String::new(),
                    status_label: "invalid_request",
                    error: Some(error),
                    metrics: TurnMetrics::default(),
                    supporting_receipts: Vec::new(),
                    model: subagent_model_for_kind(
                        context.provider.name(),
                        context.config,
                        SubagentKind::Delegate,
                    ),
                    structured_output: None,
                    files_touched: Vec::new(),
                    transcript: Vec::new(),
                },
            );
        }
    };

    let mut previous_summary = String::new();
    let mut combined_metrics = TurnMetrics::default();
    let mut combined_receipts: Vec<Value> = Vec::new();
    let mut combined_files: Vec<String> = Vec::new();
    let mut step_payloads: Vec<Value> = Vec::with_capacity(steps.len());
    let mut chain_status = ToolStatus::Success;
    let mut chain_status_label: &'static str = "success";
    let mut chain_error: Option<String> = None;
    let mut last_model = subagent_model_for_kind(
        context.provider.name(),
        context.config,
        SubagentKind::Delegate,
    );

    for (step_idx, step) in steps.iter().enumerate() {
        if context.cancel.is_cancelled() {
            chain_status = ToolStatus::Cancelled;
            chain_status_label = "cancelled";
            break;
        }
        let substituted = chain_substitute_previous(&step.prompt, &previous_summary);
        let mut step_args = json!({ "prompt": substituted });
        if let Some(scope) = &step.scope {
            step_args["scope"] = Value::String(scope.clone());
        }
        // Thread the per-step model override into the sub-call so it
        // round-trips through `parse_subagent_request` →
        // `SubagentRequest.model_override` → `run_subagent` and the step
        // actually runs on its requested model; omitted falls back to the
        // parent's delegate model.
        if let Some(model) = &step.model {
            step_args["model"] = Value::String(model.clone());
        }
        let step_call = ToolCall {
            call_id: format!("{}#step_{step_idx}", call.call_id),
            name: DELEGATE_TOOL_NAME.to_string(),
            arguments: step_args,
        };
        record_subagent_call(&mut broker.metrics, SubagentKind::Delegate);
        let outcome = run_subagent_dispatch(context, &step_call, SubagentKind::Delegate).await;
        apply_subagent_dispatch(broker, SubagentKind::Delegate, &outcome);
        if let Some(metrics) = outcome.execution_metrics.as_ref() {
            chain_accumulate_metrics(&mut combined_metrics, metrics);
        }
        if let Some(receipts) = outcome.result.content.get("supporting_receipts").cloned()
            && let Value::Array(items) = receipts
        {
            combined_receipts.extend(items);
        }
        if let Some(files) = outcome
            .result
            .content
            .get("files_touched")
            .and_then(Value::as_array)
        {
            for entry in files {
                if let Some(path) = entry.as_str()
                    && !combined_files.iter().any(|existing| existing == path)
                {
                    combined_files.push(path.to_string());
                }
            }
        }
        if let Some(model) = outcome.result.content.get("model").and_then(Value::as_str) {
            last_model = model.to_string();
        }

        step_payloads.push(json!({
            "step": step_idx,
            "prompt": substituted,
            "summary": outcome.summary,
            "status": tool_status_label(outcome.result.status),
            "model_hint": step.model,
        }));

        previous_summary = outcome.summary.clone();

        if outcome.result.status != ToolStatus::Success {
            chain_status = outcome.result.status;
            chain_status_label = "chain_aborted";
            chain_error = outcome
                .result
                .content
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(format!("step {step_idx} did not complete successfully")));
            break;
        }
    }

    let execution = SubagentExecution {
        status: chain_status,
        summary: previous_summary,
        status_label: chain_status_label,
        error: chain_error,
        metrics: combined_metrics,
        supporting_receipts: combined_receipts,
        model: last_model,
        structured_output: Some(json!({ "chain_steps": step_payloads })),
        files_touched: combined_files,
        transcript: Vec::new(),
    };
    subagent_control_result(call, SubagentKind::Delegate, execution)
}

fn subagent_supporting_receipt(result: &ToolResult, path: Option<&str>) -> Value {
    let mut value = json!({
        "tool": result.tool_name,
        "status": tool_status_label(result.status),
        "output_sha256": result.receipt.output_sha256,
        "content_sha256": result.receipt.content_sha256,
        "output_bytes": result.cost_hint.output_bytes,
        "truncated": result.cost_hint.truncated,
    });
    if let Some(path) = path
        && let Value::Object(map) = &mut value
    {
        map.insert("path".to_string(), Value::String(path.to_string()));
    }
    value
}

/// Pulls the most-likely file path out of a tool call's arguments so we
/// can attribute the subagent's reads/writes to concrete files without
/// digging into receipt SHAs. Covers the read/edit/search tools the
/// subagent is allowed to call; unknown shapes return `None` and the
/// supporting receipt is recorded without a `path` field.
fn subagent_tool_call_path(call: &ToolCall) -> Option<String> {
    let arg_str = |key: &str| {
        call.arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    match call.name.as_str() {
        "read_file" | "read_slice" | "write_file" | "repo_map" | "hierarchy" | "diff_context" => {
            arg_str("path")
        }
        "grep" | "reference_search" => arg_str("path").or_else(|| arg_str("file")),
        "glob" => arg_str("path"),
        "apply_patch" => call
            .arguments
            .get("patches")
            .and_then(Value::as_array)
            .and_then(|patches| patches.first())
            .and_then(|patch| patch.get("path"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        _ => None,
    }
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

/// Collect the successful in-place file mutations from a just-committed
/// tool round so [`mask_expired_reads_after_edits`] can stub the now-stale
/// earlier reads of those files (cost-reduction idea M2). Only
/// `ToolStatus::Success` edits are returned — errored, denied, stale, and
/// cancelled edits leave the trajectory untouched, so the prior reads stay
/// authoritative.
///
/// `search`/`replace` patches expose the changed span directly: the
/// `search` text is exactly the pre-edit bytes that no longer exist in the
/// file, so masking is scoped to that span and surrounding context
/// survives. `create_file`/`delete_file`/`move_file` operations are
/// skipped — a create has no prior read to expire, and delete/move don't
/// produce a stale *in-file* snapshot of surviving content. `write_file`
/// is a full-file overwrite with no sub-span, recorded as `whole_file`.
fn collect_successful_edits(
    tool_calls: &[ToolCall],
    outputs_with_status: &[(LlmInputItem, String, ToolStatus)],
) -> Vec<SuccessfulEdit> {
    let mut edits: Vec<SuccessfulEdit> = Vec::new();
    for (item, tool_name, status) in outputs_with_status {
        if *status != ToolStatus::Success {
            continue;
        }
        let LlmInputItem::FunctionCallOutput { call_id, .. } = item else {
            continue;
        };
        let Some(call) = tool_calls.iter().find(|call| &call.call_id == call_id) else {
            continue;
        };
        match tool_name.as_str() {
            "write_file" => {
                if let Some(path) = call
                    .arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    edits.push(SuccessfulEdit {
                        path: path.to_string(),
                        changed_spans: Vec::new(),
                        whole_file: true,
                    });
                }
            }
            "apply_patch" => collect_apply_patch_edits(&call.arguments, &mut edits),
            _ => {}
        }
    }
    edits
}

/// Extract `(path, search)` pairs from an `apply_patch` call's
/// `patches`/`operations` arrays. The `search` string is the changed span
/// the lineage-masking pass splices out of stale reads.
fn collect_apply_patch_edits(arguments: &Value, edits: &mut Vec<SuccessfulEdit>) {
    let push_search_replace = |edits: &mut Vec<SuccessfulEdit>, entry: &Value| {
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let search = entry.get("search").and_then(Value::as_str);
        if let (Some(path), Some(search)) = (path, search)
            && !search.is_empty()
        {
            edits.push(SuccessfulEdit {
                path: path.to_string(),
                changed_spans: vec![search.to_string()],
                whole_file: false,
            });
        }
    };
    if let Some(patches) = arguments.get("patches").and_then(Value::as_array) {
        for patch in patches {
            push_search_replace(edits, patch);
        }
    }
    if let Some(operations) = arguments.get("operations").and_then(Value::as_array) {
        for op in operations {
            // Only `search_replace` ops expose a `search` span; create /
            // delete / move ops are tagged `kind` and skipped here.
            if op.get("kind").and_then(Value::as_str) == Some("search_replace") {
                push_search_replace(edits, op);
            }
        }
    }
}

// Predictive escalation watches the first few tool results for one
// broad result that spans many files, not a long sequence of
// one-file reads. The normal tool-call ceiling handles sequential
// call sprawl.
const ROUTING_DIVERSITY_RESULT_WINDOW: u64 = 3;
const ROUTING_DIVERSITY_DISTINCT_PATHS: usize = 8;

fn collect_tool_round_paths(
    calls: &[ToolCall],
    results: &[PendingToolResult],
    remaining_window: u64,
    paths: &mut BTreeSet<String>,
) -> u64 {
    let mut observed = 0u64;
    for pending in results {
        if observed >= remaining_window {
            break;
        }
        if let Some(call) = calls
            .iter()
            .find(|call| call.call_id == pending.result.call_id)
        {
            collect_path_like_values(&call.arguments, paths);
        }
        collect_path_like_values(&pending.result.content, paths);
        observed += 1;
    }
    observed
}

fn collect_path_like_values(value: &Value, paths: &mut BTreeSet<String>) {
    collect_path_like_values_with_key(None, value, paths);
}

fn collect_path_like_values_with_key(
    parent_key: Option<&str>,
    value: &Value,
    paths: &mut BTreeSet<String>,
) {
    match value {
        Value::String(text) if looks_path_like(text, parent_key.is_some_and(is_path_key)) => {
            paths.insert(text.to_string());
        }
        Value::Array(items) => {
            for item in items {
                collect_path_like_values_with_key(parent_key, item, paths);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                collect_path_like_values_with_key(Some(key.as_str()), value, paths);
            }
        }
        _ => {}
    }
}

fn is_path_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "path"
            | "paths"
            | "filepath"
            | "filepaths"
            | "filename"
            | "filenames"
            | "file"
            | "files"
            | "sourcepath"
            | "targetpath"
            | "oldpath"
            | "newpath"
            | "frompath"
            | "topath"
            | "relativepath"
            | "absolutepath"
            | "workspacepath"
    )
}

fn looks_path_like(text: &str, allow_bare_file: bool) -> bool {
    let trimmed = text.trim();
    if trimmed.len() < 3 || trimmed.contains('\n') {
        return false;
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return false;
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return true;
    }
    if allow_bare_file
        && (trimmed.starts_with('.') || Path::new(trimmed).extension().is_some())
        && !trimmed.chars().any(char::is_whitespace)
    {
        return true;
    }
    false
}

/// Render a `<skill_warnings>` block listing each activated skill
/// whose `manifest.tool_deps` declares a tool or MCP server that is
/// not available in the current registry. The block tells the model
/// to refuse the skill rather than invent fallbacks, mirroring the
/// "be explicit about missing dependencies" guidance already embedded
/// in the active-skills prompt block.
fn format_skill_tool_dep_warnings(
    missing: &std::collections::BTreeMap<String, Vec<String>>,
) -> String {
    let mut body = String::from(
        "<skill_warnings>\nOne or more activated skills declare tool dependencies that are not available in this session. Refuse to follow the dependent skill's instructions rather than improvising substitutes.\n",
    );
    for (skill, deps) in missing {
        let deps_xml = deps
            .iter()
            .map(|dep| format!("<dep>{}</dep>", squeezy_skills::xml_escape(dep)))
            .collect::<Vec<_>>()
            .join("");
        body.push_str(&format!(
            "<skill name=\"{}\">\n<missing_tool_deps>{deps_xml}</missing_tool_deps>\n</skill>\n",
            squeezy_skills::xml_escape(skill),
        ));
    }
    body.push_str("</skill_warnings>");
    body
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
            "You are Squeezy's doc-help subagent. Answer the user's Squeezy help question using ONLY the inlined bundled doc corpus and config snapshot in the user prompt. No tools available; the corpus is already in context.\n\nOutput valid GitHub-flavored Markdown. The answer is rendered directly in a terminal, so malformed Markdown shows raw symbols to the user:\n- Put a blank line between every block (heading, paragraph, list, code block).\n- Fenced code blocks must sit on their own lines: a line that is exactly ```lang (e.g. ```bash, ```toml), then the code, then a closing ``` alone on its line. Never open a fence mid-sentence and never leave one unclosed.\n- Use `inline code` (single backticks) for commands, flags, file names, and config keys; use **bold** only for key terms.\n- Use `-` bullets for steps, one item per line.\n\nFormat rules:\n- Answer in 100–200 words maximum (concise by default; a follow-up question can get more detail).\n- Use bullet points for step-by-step procedures.\n- Do not dump config TOML unless the question is specifically about configuration values.\n- Cite bundled doc paths inline using the PATH labels (e.g. `docs/external/PROVIDERS.md`).\n- If a \"Recent session context\" section is present, use it only when it is clearly relevant to the question (e.g. interpreting \"why did that fail?\"); otherwise ignore it entirely and answer from the docs.\n- If the inlined corpus does not cover the question, say exactly: \"Not covered in local docs.\" then point to https://squeezyagent.com/docs/ and suggest a related `/help <topic>` if one exists.\n- Do not mention internal agent mechanics, do not invent file paths, do not ask follow-up questions.".to_string()
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
        SubagentKind::Skill => request.system_override.clone().unwrap_or_else(|| {
            "You are a Squeezy fork-mode skill subagent invoked without an explicit instruction body. Treat the user prompt as the entire task and return a concise summary for the parent agent. Do not modify files or run shell commands.".to_string()
        }),
        SubagentKind::Routed => {
            "You are Squeezy running this turn on a fast, cost-efficient model. Carry out the user's request fully using your tools — read, edit files, and run commands as needed (edits/commands prompt for approval where the user's policy requires it). When you are done, give the user a clear, direct answer to what they asked, exactly as the main assistant would; do not describe yourself as a subagent or dump raw tool output. If the task turns out to need deeper architectural reasoning than you can do well, say so plainly.".to_string()
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
        (SubagentKind::Explore, _) => config
            .subagents
            .explore_model
            .clone()
            .map(|model| resolve_model_alias_owned(provider, model))
            .unwrap_or_else(|| cheap_model_for(provider, config).unwrap_or(parent_model.clone())),
        // `/help` is user-facing, so DocHelp defaults to the session's
        // configured main/parent model for parent-grade answers. The
        // `subagents.doc_help_model` knob overrides this: `"cheap"` drops to
        // the provider's small-fast tier (falling back to parent when none is
        // known), an explicit id resolves through the alias table, and
        // `None`/`"auto"` keep the parent model.
        (SubagentKind::DocHelp, _) => match config
            .subagents
            .doc_help_model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) if value.eq_ignore_ascii_case("cheap") => cheap_model_for(provider, config)
                .filter(|m| !m.is_empty())
                .unwrap_or(parent_model),
            Some(value) if value.eq_ignore_ascii_case("auto") => parent_model,
            Some(explicit) => resolve_model_alias_owned(provider, explicit.to_string()),
            None => parent_model,
        },
        // Skill subagents run the skill author's own instructions on
        // the parent model so the body's expectations about capability
        // hold — falling to a cheap tier here would change behavior
        // silently for any skill that relies on planner-grade output.
        (SubagentKind::Skill, _) => parent_model,
        // Cache-isolation worker runs the cheap tier (that's the whole point —
        // do the cheap work off the parent's cache); fall back to parent only
        // when no cheap tier exists.
        (SubagentKind::Routed, _) => cheap_model_for(provider, config).unwrap_or(parent_model),
        (_, RoleModelPolicy::Parent) => parent_model,
        (_, RoleModelPolicy::Cheap) => cheap_model_for(provider, config).unwrap_or(parent_model),
    }
}

/// Resolves the cheap-tier model for `provider`, honoring an explicit
/// `[model].small_fast_model` config override before falling back to the
/// per-provider built-in (Anthropic Haiku, OpenAI Nano, Gemini Flash Lite,
/// etc.). Returns `None` when the provider has no curated cheap tier; the
/// caller falls back to the parent model in that case. The Ollama default
/// (`qwen3-coder`) is the only model a local Ollama install is guaranteed
/// to have, so it is returned verbatim rather than pretending a separate
/// cheap tier exists.
pub(crate) fn cheap_model_for(provider: &str, config: &AppConfig) -> Option<String> {
    // Per-provider cheap (reroute target) model wins (routing never crosses
    // providers), then the legacy global override, then the per-provider built-in.
    if let Some(model) = config
        .providers
        .get(provider)
        .and_then(|p| p.cheap_model.clone())
        .filter(|m| !m.trim().is_empty())
    {
        return Some(resolve_model_alias_owned(provider, model));
    }
    if let Some(model) = config.small_fast_model.clone() {
        return Some(resolve_model_alias_owned(provider, model));
    }
    // Built-in default: the per-provider mini tier (not the nano `small_fast`
    // tier). A notch up judges and handles easy turns far more reliably; this
    // mirrors `default_cheap_model` in squeezy-core so the config UI agrees.
    if let Some(model) = squeezy_core::judge_model_for_provider(provider) {
        return Some(resolve_model_alias_owned(provider, model.to_string()));
    }
    match provider {
        "ollama" => Some(DEFAULT_OLLAMA_MODEL.to_string()),
        _ => None,
    }
}

fn resolve_model_alias_owned(provider: &str, model: String) -> String {
    squeezy_core::resolve_model_alias(provider, &model)
        .unwrap_or(&model)
        .to_string()
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
    tool_filter: Option<&[String]>,
) -> Vec<AdvertisedTool> {
    // Write-capable workers get the parent's FULL toolset (read/search/edit/
    // shell/network/…) minus the subagent-spawn + interactive control tools, so
    // they can actually do the work end-to-end. Their tool calls still flow
    // through the same permission/approval path as the main loop (approvals are
    // forwarded out of the subagent), and writes still require the parent's
    // permission policy to allow them.
    if kind.is_write_capable() {
        return all_tool_specs
            .iter()
            .filter(|tool| !SUBAGENT_EXCLUDED_TOOL_NAMES.contains(&tool.spec.name.as_str()))
            .cloned()
            .collect();
    }
    let mut names: BTreeSet<&str> = match kind {
        // Delegate/Routed are write-capable and returned above; these arms are
        // unreachable but keep the match exhaustive.
        SubagentKind::Delegate | SubagentKind::Routed => {
            DELEGATE_SUBAGENT_TOOL_NAMES.iter().copied().collect()
        }
        SubagentKind::Explore => EXPLORE_SUBAGENT_TOOL_NAMES.iter().copied().collect(),
        SubagentKind::DocHelp => DOC_HELP_SUBAGENT_TOOL_NAMES.iter().copied().collect(),
        // Skill subagents reuse the Delegate read-only research toolset
        // — fork-mode skill authors expect the same `read_file`, grep,
        // graph, and `plan_patch` surfaces the Delegate kind offers.
        SubagentKind::Skill => DELEGATE_SUBAGENT_TOOL_NAMES.iter().copied().collect(),
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
    // A disk-loaded custom subagent (dispatched through the Skill kind) may
    // declare its own `tools:` allow-list. Intersect it with the kind's
    // default set so the custom agent can only narrow, never widen, the
    // parent's read-only delegate surface — names it declares that aren't in
    // the default set are silently dropped.
    if let Some(filter) = tool_filter {
        let declared: BTreeSet<&str> = filter.iter().map(String::as_str).collect();
        names.retain(|name| declared.contains(name));
    }
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

fn tool_status_is_model_error(status: ToolStatus) -> bool {
    !matches!(status, ToolStatus::Success)
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
        web_call_stats: None,
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
            let limit = repeated_tool_failure_limit(result);
            let count = self.failure_counts.entry(key).or_default();
            *count += 1;
            if *count >= limit {
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

fn repeated_tool_failure_limit(result: &ToolResult) -> usize {
    if is_recoverable_web_lookup_failure(result) {
        3
    } else {
        2
    }
}

fn is_recoverable_web_lookup_failure(result: &ToolResult) -> bool {
    if result.tool_name != "webfetch" && result.tool_name != "websearch" {
        return false;
    }
    let detail = tool_failure_detail(result);
    !detail.contains("invalid tool arguments")
        && (detail.contains("HTTP status")
            || detail.contains("request failed")
            || detail.contains("timed out")
            || detail.contains("unsupported content type")
            || detail.contains("redirect"))
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

fn append_deferred_visible_assistant_text(
    deferred: &mut String,
    text: &str,
    strip_plan: bool,
) -> usize {
    let display_text = if strip_plan {
        plan_mode::strip_proposed_plan_blocks(text)
    } else {
        text.to_string()
    };
    if display_text.trim().is_empty() {
        return 0;
    }
    if !deferred.is_empty() {
        deferred.push_str("\n\n");
    }
    let visible = display_text.trim();
    deferred.push_str(visible);
    visible.chars().count()
}

fn merge_retried_visible_assistant_text(
    deferred: &mut String,
    final_text: &str,
    strip_plan: bool,
) -> String {
    let prior = std::mem::take(deferred);
    let final_display = if strip_plan {
        plan_mode::strip_proposed_plan_blocks(final_text)
    } else {
        final_text.to_string()
    };
    if prior.trim().is_empty() {
        return final_display;
    }
    if final_display.trim().is_empty() || assistant_text_is_retry_ack(&final_display) {
        return prior;
    }
    format!("{}\n\n{}", prior.trim_end(), final_display.trim_start())
}

fn assistant_text_is_retry_ack(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    // Bare `DONE` confirmation from the G2 "reply DONE if complete" nudge,
    // tolerant of trailing/wrapping punctuation, quotes, or markdown
    // emphasis ("`DONE`", "**Done.**"). Only an *essentially empty*
    // confirmation collapses to the prior answer; if the model added real
    // content alongside it, that content is merged (G1 never drops text).
    // Note: `?` is deliberately NOT trimmed — "Done?" is the model asking,
    // not confirming, so it must not collapse to the prior answer.
    let bare = lower.trim_matches(|c: char| {
        matches!(
            c,
            '.' | '!' | ' ' | '\t' | '\n' | '\r' | '"' | '\'' | '`' | '*' | '_'
        )
    });
    if bare == "done" {
        return true;
    }
    // Beyond the bare token, only an explicit AND essentially content-free
    // completeness confirmation collapses to the prior answer. A response
    // that adds real content — even one that opens "the previous response
    // is ..." but then negates it or supplies the missing content (e.g.
    // "the previous response is incomplete; the missing file is foo.rs") —
    // must be MERGED (appended), never dropped. So: short, affirms
    // completeness, and carries no negation/continuation signal.
    let chars = lower.chars().count();
    if chars > 120
        || lower.contains("incomplete")
        || lower.contains("not complete")
        || lower.contains("missing")
    {
        return false;
    }
    const COMPLETE_AFFIRMATIONS: &[&str] = &[
        "is the complete answer",
        "was the complete answer",
        "previous response is complete",
        "previous output is complete",
        "previous answer is complete",
        "already complete",
        "nothing to add",
        "no changes needed",
    ];
    COMPLETE_AFFIRMATIONS
        .iter()
        .any(|phrase| lower.contains(phrase))
}

/// Phrases that turn an "intent" verb into an *offer* rather than a
/// commitment to act now. "Let me know if you'd like me to check the
/// other files" parses structurally like "let me ... check" but is a
/// closing offer, not abandoned work. Excluding these (when they appear
/// in the final clause) removes the dominant strong-model false-positive
/// class for [`assistant_text_has_unresolved_intent`].
///
/// Kept tight to phrases that are *structurally* a trailing offer. Looser
/// markers like "happy to" / "feel free to" were dropped: they can sit in
/// front of a genuine stall ("I'm happy to fix this — let me edit it now")
/// and would wrongly suppress it.
const STALL_OFFER_MARKERS: &[&str] = &[
    "let me know",
    "if you'd like",
    "if you would like",
    "if you want",
    "if you'd prefer",
    "would you like",
    "do you want",
];

/// Return the trailing sentence/clause of an already-lowercased,
/// already-trimmed message. A stalled model ends *on* an intent ("Now
/// let me search the codebase."); a complete answer ends *on* a
/// conclusion. Anchoring the intent check to this final clause — rather
/// than scanning the whole body — is the model-agnostic discriminator
/// that keeps a strong model's mid-answer "let me check: yes, the bug is
/// in foo.rs. The fix is ..." from reading as an unresolved promise.
fn assistant_final_clause(lower_trimmed: &str) -> &str {
    // Drop trailing sentence punctuation / dangling separators so
    // "...the bug. let me fix it." and "...let me fix it:" both expose
    // the real final clause. A trailing ':' or '...' is itself an "about
    // to act" signal, so we keep the clause that precedes it.
    let core = lower_trimmed.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | '!' | '?' | ':' | ';' | ' ' | '\t' | '\n' | '\r' | '"' | '\'' | ')'
        )
    });
    if core.is_empty() {
        return lower_trimmed;
    }
    // Split on the rightmost *sentence* boundary: a terminator (`.!?`)
    // immediately followed by whitespace, or a bare newline. We do NOT
    // split on a bare `.`, so dotted tokens ("src/lib.rs", "v1.2") stay
    // intact — splitting there would drop the intent that precedes them.
    // ASCII terminators/whitespace are single-byte and never collide with
    // UTF-8 continuation bytes, so the byte scan is boundary-safe.
    let bytes = core.as_bytes();
    let mut idx = core.len();
    while idx >= 1 {
        idx -= 1;
        let c = bytes[idx];
        if c == b'\n' {
            return core[idx..].trim();
        }
        if idx >= 1
            && (c == b' ' || c == b'\t' || c == b'\r')
            && matches!(bytes[idx - 1], b'.' | b'!' | b'?')
        {
            return core[idx..].trim();
        }
    }
    core.trim()
}

/// Heuristic: does the assistant's FINAL clause announce follow-up tool
/// work the model never delivered (the "promised action then stopped"
/// stall)?
///
/// True when ALL of the following hold:
///   1. The message is non-empty visible text (not just whitespace).
///   2. It is not plan-mode output (`<proposed_plan>`) or an explicit
///      final-answer marker (`final answer:`, `in summary:`, ...).
///   3. The FINAL clause contains an intent phrase (`let me`, `i'll`,
///      `going to`, ...) followed shortly by an action verb that maps to
///      a tool (`scan`, `read`, `search`, ...), and is not an *offer*
///      ("let me know if you'd like me to ...").
///
/// This is deliberately model-agnostic — the same rule for strong and
/// weak models. It is NOT relied on to be perfect: callers pair it with
/// the carried-visible-output invariant (already-shown text is never
/// dropped) and a "confirm-or-continue" nudge (a model that was actually
/// done just confirms), so a residual false positive costs at most one
/// bounded recovery round and can neither drop text nor force an unwanted
/// action.
///
/// The tradeoff is intentional and asymmetric. Final-clause anchoring
/// trades *recall* for *precision*: a genuine stall whose announced
/// action is not the last clause (e.g. "Let me search.\nThanks!") is
/// missed. We accept that — under-firing only means a weak model that was
/// already failing gets no extra recovery round; it never hurts a model
/// that succeeded. Over-firing is what hurt strong models (the spurious
/// retry that drove unrequested edits), so precision is what matters here.
pub fn assistant_text_has_unresolved_intent(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    // Plan-mode output: a `<proposed_plan>` block is the expected
    // end-of-turn shape; not a chatty-stop bug.
    if lower.contains("<proposed_plan>") {
        return false;
    }
    // Final-answer markers anywhere: model is signaling "this is my answer".
    const FINAL_MARKERS: &[&str] = &[
        "final answer:",
        "here is the answer:",
        "in summary:",
        "to summarize:",
    ];
    if FINAL_MARKERS.iter().any(|m| lower.contains(m)) {
        return false;
    }
    let clause = assistant_final_clause(&lower);
    // Offer idioms in the final clause are closings, not abandoned work.
    if STALL_OFFER_MARKERS.iter().any(|m| clause.contains(m)) {
        return false;
    }
    const INTENT_PATTERNS: &[&str] = &[
        "let me ",
        "let's ",
        "i'll ",
        "i will ",
        "now i'll ",
        "now i ",
        "next i'll ",
        "next i ",
        "next, i ",
        "i need to ",
        "i can ",
        "first, i ",
        "going to ",
        "i'm going to ",
        "we should ",
    ];
    const ACTION_PATTERNS: &[&str] = &[
        "scan ",
        "search ",
        "explore ",
        "find ",
        "read ",
        "look ",
        "check ",
        "inspect ",
        "grep ",
        "map ",
        "list ",
        "open ",
        "fetch ",
        "load ",
        "fix ",
        "edit ",
        "modify ",
        "write ",
        "create ",
        "rename ",
        "investigate ",
        "trace ",
        "follow ",
        "delegate ",
        "run ",
    ];
    for intent in INTENT_PATTERNS {
        if let Some(idx) = clause.find(intent) {
            let tail_start = idx + intent.len();
            let mut tail_end = (tail_start + 40).min(clause.len());
            while tail_end > tail_start && !clause.is_char_boundary(tail_end) {
                tail_end -= 1;
            }
            let tail = &clause[tail_start..tail_end];
            if ACTION_PATTERNS.iter().any(|action| tail.contains(action)) {
                return true;
            }
        }
    }
    false
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

/// Fan out a `HookPayload::PreToolUse` to every registered handler.
///
/// Returns the first handler-supplied deny message (in registration
/// order) so the caller can short-circuit the tool execution with
/// `ToolStatus::Denied`. Mutation replies are observational at this
/// site — argument rewrites are not applied. Returns `None` when no
/// registry is configured, when the registry is empty, or when every
/// handler returned `allow=true`, so the no-hooks path costs zero
/// allocations.
fn dispatch_pre_tool_use(context: &ToolExecutionContext<'_>, call: &ToolCall) -> Option<String> {
    let registry = context.hooks.as_ref()?;
    if registry.is_empty() {
        return None;
    }
    let results = registry.dispatch(HookPayload::PreToolUse {
        turn_id: context.turn_id.to_string(),
        tool_name: call.name.clone(),
        call_id: call.call_id.clone(),
    });
    let mut deny_message: Option<String> = None;
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
        if !result.allow && deny_message.is_none() {
            let reason = result
                .message
                .clone()
                .unwrap_or_else(|| "tool call denied by PreToolUse hook".to_string());
            tracing::info!(
                target: "squeezy::hooks",
                turn_id = %context.turn_id,
                tool_name = %call.name,
                call_id = %call.call_id,
                handler_index = idx,
                message = %reason,
                "PreToolUse handler denied tool call"
            );
            deny_message = Some(reason);
        }
    }
    deny_message
}

/// Fan out a `HookPayload::PostToolUse` after a tool result is
/// available. When the tool reported a non-success status, also
/// fans out a [`HookPayload::PostToolUseFailure`] so failure-only
/// handlers can filter on the discriminant without re-parsing
/// `status`.
fn dispatch_post_tool_use(context: &ToolExecutionContext<'_>, result: &ToolResult) {
    let Some(registry) = context.hooks.as_ref() else {
        return;
    };
    if registry.is_empty() {
        return;
    }
    let status_label = tool_status_str(result.status).to_string();
    let results = registry.dispatch(HookPayload::PostToolUse {
        turn_id: context.turn_id.to_string(),
        tool_name: result.tool_name.clone(),
        call_id: result.call_id.clone(),
        status: status_label.clone(),
    });
    log_tool_observational_results(
        "PostToolUse",
        context.turn_id,
        &result.tool_name,
        &result.call_id,
        &results,
    );
    if !matches!(result.status, ToolStatus::Success) {
        let error_message = result
            .content
            .get("reason")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or_else(|| {
                result
                    .content
                    .get("error")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            });
        let failure_results = registry.dispatch(HookPayload::PostToolUseFailure {
            turn_id: context.turn_id.to_string(),
            tool_name: result.tool_name.clone(),
            call_id: result.call_id.clone(),
            status: status_label,
            error: error_message,
        });
        log_tool_observational_results(
            "PostToolUseFailure",
            context.turn_id,
            &result.tool_name,
            &result.call_id,
            &failure_results,
        );
    }
}

/// Fan out a `HookPayload::PostTool` once each tool output is appended
/// to the conversation. Companion to `PostToolUse` — that one fires
/// when the tool result is computed, this one fires after the result
/// has been committed to the conversation the model will see next
/// round.
fn dispatch_post_tool(
    registry: &HookRegistry,
    turn_id: TurnId,
    tool_name: &str,
    call_id: &str,
    status: ToolStatus,
) {
    if registry.is_empty() {
        return;
    }
    let results = registry.dispatch(HookPayload::PostTool {
        turn_id: turn_id.to_string(),
        tool_name: tool_name.to_string(),
        call_id: call_id.to_string(),
        status: tool_status_str(status).to_string(),
    });
    log_tool_observational_results("PostTool", turn_id, tool_name, call_id, &results);
}

/// Fan out a `HookPayload::PermissionRequest` before the permission
/// engine renders a verdict. Returns the first handler-supplied deny
/// message (in registration order) so the caller can short-circuit
/// normal policy evaluation with `ApprovalDecision::Denied`, matching
/// the enforcement contract documented on
/// [`HookEvent::is_enforcement_capable`]. Returns `None` when the
/// registry is empty or every handler returned `allow=true`.
fn dispatch_permission_request(
    registry: &HookRegistry,
    turn_id: TurnId,
    call: &ToolCall,
    request: &PermissionRequest,
) -> Option<String> {
    if registry.is_empty() {
        return None;
    }
    let results = registry.dispatch(HookPayload::PermissionRequest {
        capability: request.capability.as_str().to_string(),
        tool_name: call.name.clone(),
        turn_id: turn_id.to_string(),
        call_id: call.call_id.clone(),
        target: Some(request.target.clone()).filter(|value| !value.is_empty()),
    });
    let mut deny_message: Option<String> = None;
    for (idx, result) in results.iter().enumerate() {
        if !result.allow && deny_message.is_none() {
            let reason = result
                .message
                .clone()
                .unwrap_or_else(|| "permission request denied by hook".to_string());
            tracing::info!(
                target: "squeezy::hooks",
                turn_id = %turn_id,
                tool_name = %call.name,
                call_id = %call.call_id,
                handler_index = idx,
                message = %reason,
                "PermissionRequest handler denied permission"
            );
            deny_message = Some(reason);
        }
    }
    log_tool_observational_results(
        "PermissionRequest",
        turn_id,
        &call.name,
        &call.call_id,
        &results,
    );
    deny_message
}

/// Fan out a `HookPayload::PermissionDenied` whenever the verdict
/// resolved as deny. Fires regardless of whether the deny came from
/// the policy evaluator, the AI reviewer, a user-clicked deny, or
/// a persistent-deny rule install.
fn dispatch_permission_denied(
    registry: &HookRegistry,
    turn_id: TurnId,
    call: &ToolCall,
    request: &PermissionRequest,
    reason: &str,
) {
    if registry.is_empty() {
        return;
    }
    let results = registry.dispatch(HookPayload::PermissionDenied {
        capability: request.capability.as_str().to_string(),
        tool_name: call.name.clone(),
        turn_id: turn_id.to_string(),
        call_id: call.call_id.clone(),
        target: Some(request.target.clone()).filter(|value| !value.is_empty()),
        reason: reason.to_string(),
    });
    log_tool_observational_results(
        "PermissionDenied",
        turn_id,
        &call.name,
        &call.call_id,
        &results,
    );
}

/// Fan out a `HookPayload::SubagentStart` when the subagent registry
/// hands out a fresh lease.
fn dispatch_subagent_start(
    registry: &HookRegistry,
    parent_turn_id: TurnId,
    subagent_id: u64,
    kind: &str,
) {
    if registry.is_empty() {
        return;
    }
    let results = registry.dispatch(HookPayload::SubagentStart {
        subagent_id: subagent_id.to_string(),
        kind: kind.to_string(),
        parent_turn_id: parent_turn_id.to_string(),
    });
    log_subagent_observational_results(
        "SubagentStart",
        parent_turn_id,
        subagent_id,
        kind,
        &results,
    );
}

/// Fan out a `HookPayload::SubagentStop` after the subagent finishes
/// (success or failure). `status_label` reuses the same vocabulary
/// the parent agent surfaces on `AgentEvent::SubagentCompleted` /
/// `AgentEvent::SubagentFailed`.
fn dispatch_subagent_stop(
    registry: &HookRegistry,
    parent_turn_id: TurnId,
    subagent_id: u64,
    kind: &str,
    status_label: &str,
) {
    if registry.is_empty() {
        return;
    }
    let results = registry.dispatch(HookPayload::SubagentStop {
        subagent_id: subagent_id.to_string(),
        kind: kind.to_string(),
        parent_turn_id: parent_turn_id.to_string(),
        status: status_label.to_string(),
    });
    log_subagent_observational_results("SubagentStop", parent_turn_id, subagent_id, kind, &results);
}

fn log_observational_results(event: &'static str, turn_id: TurnId, results: &[HookResult]) {
    for (idx, result) in results.iter().enumerate() {
        if let Some(mutate) = result.mutate.as_ref() {
            tracing::debug!(
                target: "squeezy::hooks",
                turn_id = %turn_id,
                handler_index = idx,
                event,
                %mutate,
                "handler proposed a mutation (not yet applied)"
            );
        }
        if !result.allow {
            tracing::debug!(
                target: "squeezy::hooks",
                turn_id = %turn_id,
                handler_index = idx,
                event,
                message = result.message.as_deref().unwrap_or(""),
                "handler returned allow=false (not yet enforced)"
            );
        }
    }
}

fn log_tool_observational_results(
    event: &'static str,
    turn_id: TurnId,
    tool_name: &str,
    call_id: &str,
    results: &[HookResult],
) {
    for (idx, result) in results.iter().enumerate() {
        if let Some(mutate) = result.mutate.as_ref() {
            tracing::debug!(
                target: "squeezy::hooks",
                turn_id = %turn_id,
                tool_name = %tool_name,
                call_id = %call_id,
                handler_index = idx,
                event,
                %mutate,
                "handler proposed a mutation (not yet applied)"
            );
        }
        if !result.allow {
            tracing::debug!(
                target: "squeezy::hooks",
                turn_id = %turn_id,
                tool_name = %tool_name,
                call_id = %call_id,
                handler_index = idx,
                event,
                message = result.message.as_deref().unwrap_or(""),
                "handler returned allow=false (not yet enforced)"
            );
        }
    }
}

fn log_subagent_observational_results(
    event: &'static str,
    parent_turn_id: TurnId,
    subagent_id: u64,
    kind: &str,
    results: &[HookResult],
) {
    for (idx, result) in results.iter().enumerate() {
        if let Some(mutate) = result.mutate.as_ref() {
            tracing::debug!(
                target: "squeezy::hooks",
                parent_turn_id = %parent_turn_id,
                subagent_id,
                kind,
                handler_index = idx,
                event,
                %mutate,
                "handler proposed a mutation (not yet applied)"
            );
        }
        if !result.allow {
            tracing::debug!(
                target: "squeezy::hooks",
                parent_turn_id = %parent_turn_id,
                subagent_id,
                kind,
                handler_index = idx,
                event,
                message = result.message.as_deref().unwrap_or(""),
                "handler returned allow=false (not yet enforced)"
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
    // before the tool registry takes ownership of the call. A handler
    // returning `allow=false` short-circuits the execution with
    // `ToolStatus::Denied`; the handler-supplied message becomes the
    // denial reason surfaced to the model. Mutation replies remain
    // observational for now.
    let result = if let Some(reason) = dispatch_pre_tool_use(&context, &call_for_telemetry) {
        log_session_event(
            context.session_log.as_ref(),
            &context.redactor,
            "pretooluse_hook_denied",
            Some(context.turn_id),
            Some(format!(
                "PreToolUse hook denied {} ({})",
                call_for_telemetry.name, reason
            )),
            json!({
                "tool_name": call_for_telemetry.name,
                "call_id": call_for_telemetry.call_id,
                "reason": reason,
            }),
        );
        ToolResult::denied(&call_for_telemetry, reason)
    } else {
        let tool_cancel = tracked_job
            .as_ref()
            .map(|(_, cancel)| cancel.clone())
            .unwrap_or_else(|| context.cancel.clone());
        let retry_cancel = context.cancel.clone();
        let retry_tool_cancel = tool_cancel.clone();
        let retry_context = context.clone();
        let retry_call_for_executor = call_for_telemetry.clone();
        let retry_progress_call_id = progress_call_id.clone();
        let retry_progress_tool_name = progress_tool_name.clone();
        let initial = run_tool_exec_with_progress(
            &context,
            call,
            tool_cancel,
            ToolExecutionOptions { shell_ask_approver },
            &progress_call_id,
            &progress_tool_name,
            started,
        )
        .await;
        // Graph cold-start: when the tool registry returns the
        // `fallback.status = "graph_indexing"` sentinel introduced in
        // `fddd56e7`, retry once after a short wait. The underlying
        // dispatcher already waits up to `GRAPH_READY_WAIT` on the
        // first attempt; this retry covers the narrow window where the
        // indexer finishes a fraction of a second after that wait
        // closed. The cap of one retry keeps the agent from looping
        // when the indexer is genuinely backlogged — the second result,
        // whatever it is, is surfaced to the model as-is.
        maybe_retry_graph_indexing(
            initial,
            &retry_cancel,
            GRAPH_INDEXING_RETRY_WAIT,
            || async move {
                run_tool_exec_with_progress(
                    &retry_context,
                    retry_call_for_executor,
                    retry_tool_cancel,
                    ToolExecutionOptions {
                        shell_ask_approver: None,
                    },
                    &retry_progress_call_id,
                    &retry_progress_tool_name,
                    started,
                )
                .await
            },
        )
        .await
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

/// Wait between the first attempt and a single transparent retry when a
/// graph tool returns `fallback.status = "graph_indexing"`. The
/// underlying tool registry already burns up to `GRAPH_READY_WAIT`
/// waiting for the cold-start indexer; this short follow-up sleep
/// covers the common case where the indexer finishes a fraction of a
/// second after the first attempt's wait window closes. Total agent-
/// side wait per call is bounded by one sleep here.
const GRAPH_INDEXING_RETRY_WAIT: Duration = Duration::from_millis(500);

/// Tool names whose results are produced by the graph dispatcher and
/// therefore can carry the `fallback.status = "graph_indexing"` signal
/// the retry honours. Mirrors the match arm in
/// `ToolRegistry::execute_for_group_with_options`.
const GRAPH_RETRYABLE_TOOL_NAMES: &[&str] = &[
    "repo_map",
    "decl_search",
    "definition_search",
    "reference_search",
    "upstream_flow",
    "downstream_flow",
    "hierarchy",
    "read_slice",
    "symbol_context",
];

/// Detect the transient cold-start signal emitted by
/// `graph_unavailable_result(_, still_indexing = true)`:
///
/// ```json
/// { "graph_available": false,
///   "fallback": { "status": "graph_indexing", "retryable": true },
///   ... }
/// ```
///
/// Only graph-tool results are eligible — the tool-name gate is what
/// keeps an unrelated tool whose payload happens to contain the same
/// keys from being retried. A `ToolStatus::Success` is required because
/// the graph dispatcher always wraps the fallback in `Success` (the
/// fallback IS the success payload from the model's perspective until
/// the agent decides to retry).
fn is_graph_indexing_retryable_fallback(result: &ToolResult) -> bool {
    if result.status != ToolStatus::Success {
        return false;
    }
    if !GRAPH_RETRYABLE_TOOL_NAMES.contains(&result.tool_name.as_str()) {
        return false;
    }
    let Some(fallback) = result.content.get("fallback") else {
        return false;
    };
    let status = fallback
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let retryable = fallback
        .get("retryable")
        .and_then(Value::as_bool)
        .unwrap_or_default();
    status == "graph_indexing" && retryable
}

/// Apply the transparent retry policy for graph cold-start. Given an
/// `initial` tool result, this:
///
/// 1. Returns `initial` unchanged when it does not look like a
///    `graph_indexing` retryable fallback (most calls), when the turn
///    was already cancelled, or when sleeping for `wait` would block
///    progress past a fresh cancel signal.
/// 2. Otherwise sleeps for `wait`, then invokes `executor` to retry the
///    same call once. The retry's outcome (success, another fallback,
///    or an error) is what the caller sees — there is no third attempt.
///
/// Extracted from `run_one_tool` so the orchestration is testable in
/// isolation without standing up a real `ToolRegistry` / tool I/O.
async fn maybe_retry_graph_indexing<F, Fut>(
    initial: ToolResult,
    cancel: &CancellationToken,
    wait: Duration,
    executor: F,
) -> ToolResult
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ToolResult>,
{
    if cancel.is_cancelled() || !is_graph_indexing_retryable_fallback(&initial) {
        return initial;
    }
    tokio::time::sleep(wait).await;
    if cancel.is_cancelled() {
        return initial;
    }
    executor().await
}

/// Execute one tool call against the registry while emitting periodic
/// `AgentEvent::ToolProgress` heartbeats. Factored out of `run_one_tool`
/// so the transparent `graph_indexing` retry can invoke the same
/// execution loop twice without duplicating the progress-ticker dance.
async fn run_tool_exec_with_progress(
    context: &ToolExecutionContext<'_>,
    call: ToolCall,
    cancel: CancellationToken,
    options: ToolExecutionOptions,
    progress_call_id: &str,
    progress_tool_name: &str,
    started: Instant,
) -> ToolResult {
    let exec_future = context.tools.execute_for_group_with_options(
        call,
        cancel,
        context.turn_id.to_string(),
        options,
    );
    tokio::pin!(exec_future);
    let mut progress_ticker = tokio::time::interval(TOOL_PROGRESS_INTERVAL);
    // `interval` fires immediately on first poll; skip that tick so the
    // heartbeat only fires once the tool has actually been running.
    progress_ticker.tick().await;
    loop {
        tokio::select! {
            r = &mut exec_future => break r,
            _ = progress_ticker.tick() => {
                // See subagent heartbeat above for rationale: try_send so a
                // full mpsc buffer can never block the select! loop and
                // deadlock the running tool.
                let _ = context.tx.try_send(AgentEvent::ToolProgress {
                    turn_id: context.turn_id,
                    call_id: progress_call_id.to_string(),
                    tool_name: progress_tool_name.to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
            }
        }
    }
}

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
                session_cost: Some(broker.session_cost_snapshot()),
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
        web_call_stats: None,
    }
}

fn emit_tool_telemetry(
    config: &AppConfig,
    telemetry: &TelemetryClient,
    turn_id: TurnId,
    tool_sequence: u64,
    _call: &ToolCall,
    result: &ToolResult,
    duration: Duration,
) {
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
    }));
    // Fire web-request telemetry for websearch/webfetch results.
    if let Some(web_stats) = &result.web_call_stats {
        telemetry.spawn(TelemetryEvent::web_request(WebRequestReport {
            provider_token: web_stats.provider_token.clone(),
            status_token: web_stats.status_token.clone(),
            ssrf_blocked: web_stats.ssrf_blocked,
            redirect_blocked: web_stats.redirect_blocked,
            response_byte_bucket: web_stats.response_byte_bucket.clone(),
            duration_ms: web_stats.duration_ms,
        }));
    }
    // Fire implicit-skill-activation telemetry for shell results that
    // detected a skill context via `detect_for_command`.
    if result.tool_name == "shell" && result.content.get("implicit_skill_activation").is_some() {
        let source_token = result
            .content
            .get("implicit_skill_activation")
            .and_then(|v| v.get("skill_source"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        telemetry.spawn(TelemetryEvent::skill_activated(SkillActivationReport {
            total: 1,
            included: 1,
            dropped: 0,
            body_truncated: 0,
            preamble_emitted: false,
            preamble_omitted_count: 0,
            explicit_count: 0,
            trigger_count: 0,
            implicit_shell_count: 1,
            source_counts: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(source_token.to_string(), 1u64);
                m
            },
        }));
    }
    // `approval.best_effort.fallback{tool=shell}` ticks once per silent
    // shell-sandbox degradation. Co-located with the per-tool event so
    // every call site that already calls `emit_tool_telemetry` benefits
    // without threading the new event through individual handlers.
    if let Some(fallback) = shell_best_effort_fallback_from_result(result) {
        telemetry.spawn(TelemetryEvent::shell_sandbox_best_effort_fallback(
            &fallback.backend,
        ));
    }
    // Windows: fire a separate telemetry event so Windows shell backend
    // degradation is separable from Unix sandbox runtime failures in dashboards.
    if let Some(degraded) = shell_windows_degraded_from_result(result) {
        telemetry.spawn(TelemetryEvent::shell_windows_degraded(&degraded.backend));
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
    // Unix best_effort path: fires once per session on the first sandbox
    // degradation (sandbox exec crashed, unshare failed, etc.).
    if let Some(ShellBestEffortFallback {
        backend,
        fallback_count,
        first_in_session,
        fallback_reason,
    }) = shell_best_effort_fallback_from_result(result)
    {
        if first_in_session {
            let _ = tx
                .send(AgentEvent::ShellSandboxBestEffortFallback {
                    turn_id,
                    backend,
                    fallback_count,
                    fallback_reason,
                })
                .await;
        }
        return;
    }
    // Windows: every shell run uses `windows-job-object` with no FS/network
    // isolation. Emit the dedicated Windows warning once per session so the
    // TUI can display a Windows-specific safety notice.
    if let Some(ShellWindowsDegraded {
        backend,
        filesystem,
        first_in_session,
    }) = shell_windows_degraded_from_result(result)
        && first_in_session
    {
        let _ = tx
            .send(AgentEvent::ShellWindowsDegraded {
                turn_id,
                backend,
                filesystem,
            })
            .await;
    }
}

/// SHA-256 of the canonical JSON arguments the model sent for a tool call.
/// Used to pair with `output_sha256` in telemetry (F06) and to detect
/// intra-batch duplicates in `mark_intra_batch_duplicates`.
///
/// CANONICAL ORDERING: dedup correctness depends on `serde_json::to_vec`
/// producing the same bytes for two semantically-identical
/// `serde_json::Value` objects whose keys arrived in different
/// insertion order. The default `serde_json` build backs `Value::Object`
/// with `BTreeMap` (always sorted, canonical), so this holds today. If
/// the agent crate ever enables `serde_json/preserve_order` the map flips
/// to `IndexMap` (insertion-order) and dedup will start false-missing on
/// reordered-but-equivalent calls; this hash must then be replaced with
/// an explicit canonical serializer.
fn tool_call_args_sha256(call: &ToolCall) -> Option<String> {
    serde_json::to_vec(&call.arguments)
        .ok()
        .map(|bytes| squeezy_tools::sha256_hex(&bytes))
}

/// Drain and clear the MCP elicitation audit ring, then fire `mcp_elicitation`
/// telemetry for each new event. Called at the end of each turn so each
/// elicitation decision is counted exactly once across the session.
fn emit_mcp_elicitation_telemetry(tools: &ToolRegistry, telemetry: &TelemetryClient) {
    for event in tools.drain_mcp_elicitation_audit() {
        let policy_str = match event.policy {
            squeezy_core::PermissionMode::Allow => "allow",
            squeezy_core::PermissionMode::Ask => "ask",
            squeezy_core::PermissionMode::Deny => "deny",
        };
        telemetry.spawn(TelemetryEvent::mcp_elicitation(
            event.kind.as_str(),
            policy_str,
            event.outcome.as_str(),
        ));
    }
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

fn telemetry_session_status(status: SessionStatus) -> TelemetrySessionStatusKind {
    match status {
        SessionStatus::Running => TelemetrySessionStatusKind::Running,
        SessionStatus::Archived => TelemetrySessionStatusKind::Archived,
        SessionStatus::Completed => TelemetrySessionStatusKind::Completed,
        SessionStatus::Cancelled => TelemetrySessionStatusKind::Cancelled,
        SessionStatus::Failed => TelemetrySessionStatusKind::Failed,
        SessionStatus::Truncated => TelemetrySessionStatusKind::Truncated,
    }
}

fn telemetry_slash_arg_shape(cmd: &DispatchCommand) -> SlashArgShape {
    match cmd {
        DispatchCommand::Cost
        | DispatchCommand::Context
        | DispatchCommand::Reviewer
        | DispatchCommand::Mcp
        | DispatchCommand::Model
        | DispatchCommand::Permissions
        | DispatchCommand::Attachments
        | DispatchCommand::Clear
        | DispatchCommand::Diff
        | DispatchCommand::Tasks
        | DispatchCommand::Pins
        | DispatchCommand::Sessions
        | DispatchCommand::Fork
        | DispatchCommand::Checkpoints
        | DispatchCommand::Undo
        | DispatchCommand::Statusline
        | DispatchCommand::Keymap
        | DispatchCommand::Cheap
        | DispatchCommand::Parent
        | DispatchCommand::Terminal => SlashArgShape::None,
        DispatchCommand::CheckpointsDoctor => SlashArgShape::FixedSubcommand,
        DispatchCommand::Attach { .. } => SlashArgShape::Path,
        DispatchCommand::Plan { prompt } | DispatchCommand::Build { prompt } => {
            if option_has_text(prompt.as_ref()) {
                SlashArgShape::FreeText
            } else {
                SlashArgShape::None
            }
        }
        DispatchCommand::Help { topic } => {
            if option_has_text(topic.as_ref()) {
                SlashArgShape::FreeText
            } else {
                SlashArgShape::None
            }
        }
        DispatchCommand::Theme { theme } => {
            if option_has_text(theme.as_ref()) {
                SlashArgShape::FixedSubcommand
            } else {
                SlashArgShape::None
            }
        }
        DispatchCommand::Effort { value }
        | DispatchCommand::ToolVerbosity { value }
        | DispatchCommand::Router { value } => {
            if option_has_text(value.as_ref()) {
                SlashArgShape::FixedSubcommand
            } else {
                SlashArgShape::None
            }
        }
        DispatchCommand::Config { section } => {
            if option_has_text(section.as_ref()) {
                SlashArgShape::FixedSubcommand
            } else {
                SlashArgShape::None
            }
        }
        DispatchCommand::Compact { subcommand } => match subcommand {
            CompactSubcommand::Undo | CompactSubcommand::History => SlashArgShape::FixedSubcommand,
            CompactSubcommand::Run => SlashArgShape::None,
        },
        DispatchCommand::Plans { args }
        | DispatchCommand::Feedback { args }
        | DispatchCommand::Report { args } => {
            if args.trim().is_empty() {
                SlashArgShape::None
            } else {
                SlashArgShape::FixedSubcommand
            }
        }
        DispatchCommand::Task { .. }
        | DispatchCommand::TaskCancel { .. }
        | DispatchCommand::Unpin { .. }
        | DispatchCommand::Resume { .. }
        | DispatchCommand::SessionExport { .. }
        | DispatchCommand::Checkpoint { .. }
        | DispatchCommand::RevertTurn { .. }
        | DispatchCommand::Detach { .. } => SlashArgShape::Id,
        DispatchCommand::Pin { target } => {
            if target.is_some() {
                SlashArgShape::Id
            } else {
                SlashArgShape::None
            }
        }
        DispatchCommand::Session { .. }
        | DispatchCommand::SessionRename { .. }
        | DispatchCommand::SessionLabel { .. } => SlashArgShape::FixedSubcommand,
        DispatchCommand::SessionExportHtml { path, .. } => {
            if path.is_some() {
                SlashArgShape::Path
            } else {
                SlashArgShape::Id
            }
        }
    }
}

fn option_has_text(value: Option<&String>) -> bool {
    value.map(|value| !value.trim().is_empty()).unwrap_or(false)
}

fn telemetry_slash_outcome_from_dispatch(outcome: &DispatchOutcome) -> SlashOutcome {
    match outcome {
        DispatchOutcome::Error { .. } => SlashOutcome::Error,
        DispatchOutcome::Unsupported { .. } => SlashOutcome::Unknown,
        DispatchOutcome::TuiOnly { .. } => SlashOutcome::OpenedOverlay,
        DispatchOutcome::ModeChanged {
            prompt: Some(_), ..
        } => SlashOutcome::StartedTurn,
        DispatchOutcome::DiffSnapshot { .. } | DispatchOutcome::CheckpointUndo { .. } => {
            SlashOutcome::StartedJob
        }
        DispatchOutcome::Compacted { skipped: true } => SlashOutcome::Skipped,
        _ => SlashOutcome::LocalAction,
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

/// Classify a provider error message into a coarse `ProviderErrorKind` bucket
/// for telemetry. Inspects the error string with simple keyword matching,
/// which is sufficient for the "what fraction are rate limits vs auth?"
/// use case without requiring squeezy-llm to export a typed error enum.
fn classify_provider_error(error: &SqueezyError) -> Option<ProviderErrorKind> {
    let message = match error {
        SqueezyError::ProviderRequest(msg) | SqueezyError::ProviderStream(msg) => msg.as_str(),
        SqueezyError::ProviderNotConfigured(_) => return Some(ProviderErrorKind::Auth),
        _ => return None,
    };
    let lower = message.to_ascii_lowercase();
    if lower.contains("unauthorized")
        || lower.contains("unauthenticated")
        || lower.contains("authentication_error")
        || lower.contains("invalid api key")
        || lower.contains("authentication failed")
    {
        Some(ProviderErrorKind::Auth)
    } else if lower.contains("forbidden") || lower.contains("permission_error") {
        Some(ProviderErrorKind::Permission)
    } else if lower.contains("quota_exceeded")
        || lower.contains("insufficient_quota")
        || lower.contains("monthly_usage_limit")
        || lower.contains("billing_hard_limit")
    {
        Some(ProviderErrorKind::Quota)
    } else if lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("too many requests")
        || lower.contains("429")
    {
        Some(ProviderErrorKind::RateLimit)
    } else if lower.contains("context window")
        || lower.contains("context_window")
        || lower.contains("context_length")
        || lower.contains("token limit")
    {
        Some(ProviderErrorKind::ContextOverflow)
    } else if lower.contains("content_filtered")
        || lower.contains("content filter")
        || lower.contains("refusal")
        || lower.contains("safety")
    {
        Some(ProviderErrorKind::ContentFilter)
    } else if lower.contains("invalid_request") || lower.contains("bad request") {
        Some(ProviderErrorKind::InvalidRequest)
    } else if lower.contains("not_found") || lower.contains("404") {
        Some(ProviderErrorKind::NotFound)
    } else if lower.contains("server error") || lower.contains("5xx") || lower.contains("503") {
        Some(ProviderErrorKind::Server)
    } else if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection")
    {
        Some(ProviderErrorKind::Transport)
    } else if lower.contains("parse") || lower.contains("invalid json") {
        Some(ProviderErrorKind::Parse)
    } else {
        Some(ProviderErrorKind::Unknown)
    }
}

/// Maximum chars of preceding assistant text passed in
/// [`ToolApprovalRequest::context`]. Sized to fit a short rationale without
/// dominating the approval modal.
const APPROVAL_CONTEXT_CAP: usize = 240;

/// Extract an explicit tool-call rationale, redact it, and keep only a
/// complete short sentence/line. Approval prompts should omit this field
/// rather than reuse unrelated transcript text.
fn approval_context_from_request(
    request: &PermissionRequest,
    redactor: &Redactor,
) -> Option<String> {
    let rationale = request
        .metadata
        .get("description")
        .or_else(|| request.metadata.get("justification"))?;
    let redacted = redactor.redact(rationale).text;
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return None;
    }
    approval_context_excerpt(trimmed)
}

fn approval_context_excerpt(value: &str) -> Option<String> {
    let collapsed = collapse_status_text(value);
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= APPROVAL_CONTEXT_CAP {
        return Some(trimmed.to_string());
    }

    let mut best_boundary = None;
    for (idx, ch) in trimmed.char_indices() {
        let end = idx + ch.len_utf8();
        let chars = trimmed[..end].chars().count();
        if chars > APPROVAL_CONTEXT_CAP {
            break;
        }
        if matches!(ch, '.' | '!' | '?' | ':') {
            best_boundary = Some(end);
        }
    }
    let end = best_boundary?;
    let excerpt = trimmed[..end].trim();
    (!excerpt.is_empty()).then(|| excerpt.to_string())
}

async fn permission_decision(
    call: &ToolCall,
    context: &ToolExecutionContext<'_>,
) -> PermissionOutcome {
    if is_direct_user_shell_call(call) {
        return PermissionOutcome::no_reviewer_cost(ApprovalDecision::Approved);
    }
    let runtime = PermissionDecisionContext::from_tool_context(context);
    let request = runtime.tools.permission_request(call);
    permission_decision_for_request(&runtime, call, request).await
}

async fn permission_decision_for_request(
    context: &PermissionDecisionContext,
    call: &ToolCall,
    request: PermissionRequest,
) -> PermissionOutcome {
    let mut reviewer_usd_micros: u64 = 0;
    // PermissionRequest fires once per decision attempt, before any
    // verdict is computed. Lets audit handlers record every gated
    // request — including those resolved by an auto-allow rule or
    // mode policy before the user is asked. A non-zero exit from a
    // skill hook returns `allow=false` which is now enforced here,
    // consistent with PreToolUse denial semantics.
    if let Some(registry) = context.hooks.as_ref()
        && let Some(deny_reason) =
            dispatch_permission_request(registry, context.turn_id, call, &request)
    {
        dispatch_permission_denied(registry, context.turn_id, call, &request, &deny_reason);
        return PermissionOutcome::no_reviewer_cost(ApprovalDecision::Denied(deny_reason));
    }
    let active_mode = load_session_mode(&context.session_mode);
    let session_id_for_plan_mode = context.session_id_for_plan_mode();
    let active_plan = plan_mode::latest_plan_path(
        &context.config.workspace_root,
        session_id_for_plan_mode.as_deref(),
    );
    let mut mode_ask_verdict = None;
    if let Some(verdict) = mode_permission_verdict(active_mode, &request, active_plan.as_deref()) {
        log_permission_verdict(&request, &verdict);
        match verdict.action {
            PermissionAction::Deny => {
                if let Some(registry) = context.hooks.as_ref() {
                    dispatch_permission_denied(
                        registry,
                        context.turn_id,
                        call,
                        &request,
                        &verdict.reason,
                    );
                }
                return PermissionOutcome::no_reviewer_cost(ApprovalDecision::Denied(
                    verdict_deny_reason_for_model(context, &verdict),
                ));
            }
            PermissionAction::Ask => {
                mode_ask_verdict = Some(verdict);
            }
            PermissionAction::Allow => {
                return PermissionOutcome::no_reviewer_cost(approved_decision(context, &request));
            }
        }
    }
    let session_rules = snapshot_session_rules(&context.session_rules);
    let mut mode_forced_ask = false;
    let mut verdict = context
        .config
        .permissions
        .evaluate_with_extra(&request, &session_rules);
    if let Some(mode_verdict) = mode_ask_verdict
        && verdict.action != PermissionAction::Deny
    {
        mode_forced_ask = true;
        verdict = mode_verdict;
    }
    // When the structural pre-classifier raises a permissive verdict to an Ask
    // floor, remember its raw reason (e.g. `dangerous interpreter "sudo"`). If
    // the AI reviewer then denies, we thread this forward so the user can tell a
    // structural hazard apart from a context-weighed refusal — two different
    // remediations (rephrase vs. argue the context) that would otherwise collapse
    // to the reviewer's reason alone.
    let mut pre_classifier_ask_reason: Option<String> = None;
    // The structural pre-classifier runs for every shell call, not just those
    // whose policy verdict is already Ask. Its hazardous-shape floor
    // (dangerous interpreter, destructive verb, sensitive path) must be able to
    // override a permissive `shell = Allow` default — otherwise
    // `python -c '...'`, `sudo ...`, and sensitive-path access execute with no
    // gate. It should not turn a default human prompt into an automatic denial;
    // false positives must stay recoverable by approval.
    if request.tool_name == "shell"
        && let Some(command) = request.metadata.get("command")
    {
        match pre_classify_shell(command, &context.config.permissions.shell_sandbox) {
            ShellPreClassification::AutoAllow { reason } => {
                // Only relax an Ask to Allow; never re-affirm an existing Allow
                // nor weaken a Deny. Plan-mode forced asks must still reach the
                // user instead of being relaxed by the shell pre-classifier.
                if verdict.action == PermissionAction::Ask && !mode_forced_ask {
                    let reason = format!("pre-classifier auto-allow: {reason}");
                    log_session_event(
                        context.session_log.as_ref(),
                        &context.redactor,
                        "permission_pre_classifier_allow",
                        Some(context.turn_id),
                        Some(reason.clone()),
                        json!({
                            "reason": reason,
                            "capability": request.capability.as_str(),
                            "target": request.target.clone(),
                        }),
                    );
                    verdict = PermissionVerdict {
                        action: PermissionAction::Allow,
                        matched_rule: None,
                        reason,
                        silent: false,
                    };
                }
            }
            ShellPreClassification::RequiresApproval { reason } => {
                // Tighten permissive verdicts into a gate so the command cannot
                // run silently. Existing Ask/Deny verdicts already carry the
                // desired user or policy boundary and should not be escalated
                // further by a structural heuristic.
                let tightened = match verdict.action {
                    PermissionAction::Allow => PermissionAction::Ask,
                    PermissionAction::Ask => PermissionAction::Ask,
                    PermissionAction::Deny => PermissionAction::Deny,
                };
                if tightened != verdict.action {
                    // Carry the raw structural reason forward in case the AI
                    // reviewer denies this same request below.
                    pre_classifier_ask_reason = Some(reason.clone());
                    let reason = format!("pre-classifier requires approval: {reason}");
                    log_session_event(
                        context.session_log.as_ref(),
                        &context.redactor,
                        "permission_pre_classifier_ask",
                        Some(context.turn_id),
                        Some(reason.clone()),
                        json!({
                            "reason": reason,
                            "action": tightened.as_str(),
                            "capability": request.capability.as_str(),
                            "target": request.target.clone(),
                        }),
                    );
                    verdict = PermissionVerdict {
                        action: tightened,
                        matched_rule: None,
                        reason,
                        silent: false,
                    };
                }
            }
            ShellPreClassification::AskAi => {}
        }
    }
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
        let reviewer_result = ai_reviewer::review_permission(ai_reviewer::AiReviewerInput {
            config: &context.config,
            provider: context.provider.clone(),
            request: &request,
            transcript,
            state: context.ai_reviewer_state.clone(),
            turn_id: context.turn_id,
            cancel: context.cancel.child_token(),
            telemetry: context.telemetry.clone(),
        })
        .await;
        // The reviewer's LLM call is real billable spend. Record it into
        // the persisted session cost + per-model ledger so `/cost` and
        // the By-model drill are always correct. Also accumulate the USD
        // micros in `reviewer_usd_micros` so the turn loop can fold this
        // spend into the active `CostBroker`, keeping the live
        // session-cost snapshot and cap checks accurate within the turn.
        if (reviewer_result.cost.estimated_usd_micros.is_some()
            || reviewer_result.cost.input_tokens.is_some()
            || reviewer_result.cost.output_tokens.is_some())
            && let Some(conversation_state) = &context.conversation_state
        {
            let mut state = conversation_state.lock().await;
            merge_cost(&mut state.cost, &reviewer_result.cost);
            merge_cost(&mut state.metrics.provider, &reviewer_result.cost);
            state.metrics.model_ledger.record(
                context.provider.name(),
                &reviewer_result.model,
                CostOrigin::AiReviewer,
                &reviewer_result.cost,
            );
        }
        reviewer_usd_micros = reviewer_usd_micros
            .saturating_add(reviewer_result.cost.estimated_usd_micros.unwrap_or(0));
        match reviewer_result.outcome {
            ai_reviewer::AiReviewerOutcome::Verdict(mut reviewed) => {
                // When the pre-classifier raised this request to an Ask *and* the
                // reviewer denied it, name both nodes of the decision tree so the
                // user sees the structural hazard was the floor and the reviewer
                // agreed — not just the reviewer's text replacing the structural
                // reason wholesale.
                if reviewed.action == PermissionAction::Deny
                    && let Some(structural) = pre_classifier_ask_reason.as_deref()
                {
                    reviewed.reason = format!(
                        "{structural} (pre-classified) · reviewer agreed: {}",
                        reviewed.reason
                    );
                }
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
    if !mode_forced_ask
        && should_classify_shell(&context.config, context.provider.name(), &request, &verdict)
        && let Some(classifier) = classify_ambiguous_shell(
            context.provider.clone(),
            &context.config,
            &request,
            context.cancel.clone(),
        )
        .await
    {
        // Mirrors the AI-reviewer fold above — same KNOWN LIMITATION (the
        // active turn's CostBroker is not on the permission path, so the
        // current turn's live status-line snapshot and cap checks lag by
        // one turn). Tightened to `> 0` so a provider that streams a
        // `CostSnapshot` with zeroed counters (e.g. cancelled mid-stream
        // after some delta but before billing) does not churn the ledger
        // with a no-op row.
        let cost_present = classifier.cost.estimated_usd_micros.unwrap_or(0) > 0
            || classifier.cost.input_tokens.unwrap_or(0) > 0
            || classifier.cost.output_tokens.unwrap_or(0) > 0;
        if cost_present && let Some(conversation_state) = &context.conversation_state {
            let mut state = conversation_state.lock().await;
            merge_cost(&mut state.cost, &classifier.cost);
            merge_cost(&mut state.metrics.provider, &classifier.cost);
            state.metrics.model_ledger.record(
                context.provider.name(),
                &classifier.model,
                CostOrigin::Main,
                &classifier.cost,
            );
        }
        // Accumulate classifier cost so the turn loop can fold it into
        // the active CostBroker alongside reviewer spend.
        reviewer_usd_micros =
            reviewer_usd_micros.saturating_add(classifier.cost.estimated_usd_micros.unwrap_or(0));
        verdict = classifier.verdict;
    }
    log_permission_verdict(&request, &verdict);
    // Emit permission_decided telemetry for auto-evaluated verdicts (Allow/Deny
    // from rules or mode policy, before any user prompt). Never includes targets
    // or reasons — only capability, action, and rule source.
    if verdict.action != PermissionAction::Ask {
        let source_token = verdict
            .matched_rule
            .as_ref()
            .map(|r| r.source.as_str())
            .unwrap_or("policy");
        context.telemetry.spawn(TelemetryEvent::permission_decided(
            request.capability.as_str(),
            verdict.action.as_str(),
            source_token,
        ));
    }
    match verdict.action {
        PermissionAction::Allow => {
            PermissionOutcome::no_reviewer_cost(approved_decision(context, &request))
        }
        PermissionAction::Deny => {
            if verdict.silent {
                log_silent_deny(context, &request, &verdict);
            }
            if let Some(registry) = context.hooks.as_ref() {
                dispatch_permission_denied(
                    registry,
                    context.turn_id,
                    call,
                    &request,
                    &verdict.reason,
                );
            }
            PermissionOutcome {
                decision: ApprovalDecision::Denied(verdict_deny_reason_for_model(
                    context, &verdict,
                )),
                reviewer_usd_micros,
            }
        }
        PermissionAction::Ask => {
            let (decision_tx, decision_rx) = oneshot::channel();
            let approval_context = approval_context_from_request(&request, &context.redactor);
            let preview = context.tools.preview_for(call, &request);
            let approval_request = ToolApprovalRequest {
                id: context.approval_ids.fetch_add(1, Ordering::Relaxed),
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                scope: legacy_scope_for_capability(request.capability),
                permission: redact_permission_request(request.clone(), &context.redactor),
                matched_rule: verdict.matched_rule,
                reason: context.redactor.redact(&verdict.reason).text,
                context: approval_context,
                preview,
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
                Err(CancelErr::Cancelled) => {
                    return PermissionOutcome {
                        decision: ApprovalDecision::Cancelled,
                        reviewer_usd_micros,
                    };
                }
            };
            if send_result.is_err() {
                let reason = "approval channel closed".to_string();
                if let Some(registry) = context.hooks.as_ref() {
                    dispatch_permission_denied(registry, context.turn_id, call, &request, &reason);
                }
                return PermissionOutcome {
                    decision: ApprovalDecision::Denied(reason),
                    reviewer_usd_micros,
                };
            }
            let decision = match decision_rx.or_cancel(&context.cancel).await {
                Ok(decision) => decision,
                Err(CancelErr::Cancelled) => {
                    return PermissionOutcome {
                        decision: ApprovalDecision::Cancelled,
                        reviewer_usd_micros,
                    };
                }
            };
            log_session_event(
                context.session_log.as_ref(),
                &context.redactor,
                "approval_decided",
                Some(context.turn_id),
                Some(format!("{decision:?}")),
                json!({ "decision": format!("{decision:?}") }),
            );
            let outcome = match decision {
                Ok(ToolApprovalDecision::Approved | ToolApprovalDecision::AllowOnce) => {
                    approved_decision(context, &request)
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
                    approved_decision(context, &request)
                }
                Ok(ToolApprovalDecision::AllowRuleUser) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::User,
                        PermissionAction::Allow,
                    )
                    .await;
                    approved_decision(context, &request)
                }
                Ok(ToolApprovalDecision::AllowRuleProject) => {
                    install_persistent_rule(
                        context,
                        &request,
                        PermissionRuleSource::Project,
                        PermissionAction::Allow,
                    )
                    .await;
                    approved_decision(context, &request)
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
            };
            // Single PermissionDenied dispatch covers every deny exit
            // from the ask flow — user-clicked-deny, ask-rule installs
            // that resolve as deny, persistent-deny rule installs, and
            // the timed-out "approval was not answered" fallback.
            // Skipped on Approved / Cancelled so handlers only see the
            // deny half.
            if let (Some(registry), ApprovalDecision::Denied(reason)) =
                (context.hooks.as_ref(), &outcome)
            {
                dispatch_permission_denied(registry, context.turn_id, call, &request, reason);
            }
            // Emit approval_decided telemetry for user-prompted verdicts.
            // Capability + risk + decision + source only — no targets or reasons.
            let approval_decision_token = match &outcome {
                ApprovalDecision::Approved => "approved",
                ApprovalDecision::Denied(_) => "denied",
                ApprovalDecision::Cancelled => "cancelled",
            };
            let risk_token = match request.risk {
                PermissionRisk::Low => "low",
                PermissionRisk::Medium => "medium",
                PermissionRisk::High => "high",
                PermissionRisk::Critical => "critical",
            };
            context.telemetry.spawn(TelemetryEvent::approval_decided(
                request.capability.as_str(),
                risk_token,
                approval_decision_token,
                "user",
            ));
            PermissionOutcome {
                decision: outcome,
                reviewer_usd_micros,
            }
        }
    }
}

fn approved_decision(
    context: &PermissionDecisionContext,
    request: &PermissionRequest,
) -> ApprovalDecision {
    context.tools.record_permission_grant(request);
    ApprovalDecision::Approved
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
            // reviewer_usd_micros is not folded into a broker here because
            // shell_ask callbacks run outside the main turn loop and have no
            // broker reference. The spend IS already persisted to state.cost
            // (and thus visible to the next turn's broker seed), so the
            // cap-basis total is always eventually correct. The intra-turn
            // live snapshot has a minor lag bounded by a single reviewer or
            // classifier call (max_output_tokens: 120 / 80).
            let outcome =
                permission_decision_for_request(&runtime, &synthetic_call, permission).await;
            match outcome.decision {
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
    if call.name != "shell" || !call.call_id.starts_with("local-shell-") {
        return false;
    }
    let direct = call
        .arguments
        .get("direct_user_shell")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !direct {
        return false;
    }
    // Mirror the registry's nonce check: the auto-approve path that skips
    // the permission prompt requires the same per-process nonce that the
    // TUI's `!cmd` minter ships. Without it, a downstream caller that
    // synthesises a `local-shell-…` call_id falls back to the normal
    // permission flow.
    call.arguments
        .get("direct_user_shell_nonce")
        .and_then(Value::as_str)
        == Some(squeezy_tools::direct_user_shell_nonce())
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

/// Short hint about Build-mode shell-sandbox readiness for inclusion in
/// Plan-mode denial messages. The underlying kernel probes
/// (`linux_unshare_supported`, `linux_landlock_supported`) are OnceLock-cached
/// so they are cheap after the first call; the `ShellSandboxDoctor` struct
/// itself is allocated on each call (only `backend: &'static str` and
/// `available: bool` are read from it here).
fn build_mode_sandbox_hint() -> String {
    let doc = squeezy_tools::shell_sandbox_doctor();
    if doc.available {
        format!("Build mode would use sandbox backend {}", doc.backend)
    } else {
        format!(
            "Build mode sandbox backend {} is unavailable — required mode would fail",
            doc.backend
        )
    }
}

pub(crate) fn mode_permission_verdict(
    mode: SessionMode,
    request: &PermissionRequest,
    active_plan_path: Option<&Path>,
) -> Option<PermissionVerdict> {
    // Pre-canonicalize the active plan path once so it can be reused for
    // both the permission gate (via is_active_plan_path_with_canon) and the
    // denial-message display, avoiding a redundant fs::canonicalize syscall.
    // On Windows this also normalises drive-letter case, UNC prefixes, and
    // junction targets before either comparison or display.
    //
    // Gate the canonicalize on the only branches that consume the result:
    // Plan-mode + Edit (used by `plan_edit_allowed` and the denial display).
    // Read / Search / Network / Mcp / Shell / Git / Compiler permission
    // decisions (the high-volume path on every Plan-mode turn) skip the
    // syscall entirely.
    let active_plan_canon =
        if mode == SessionMode::Plan && request.capability == PermissionCapability::Edit {
            active_plan_path.and_then(plan_mode::canonicalize_active_plan_path)
        } else {
            None
        };
    let plan_edit_allowed = matches!(
        (mode, request.capability),
        (SessionMode::Plan, PermissionCapability::Edit)
    ) && active_plan_canon.as_deref().is_some_and(|active| {
        plan_mode::is_active_plan_path_with_canon(Path::new(&request.target), active)
    });
    if mode == SessionMode::Plan && request.tool_name == "shell" {
        if matches!(
            request.capability,
            PermissionCapability::Destructive | PermissionCapability::Edit
        ) {
            return Some(PermissionVerdict {
                action: PermissionAction::Deny,
                matched_rule: None,
                reason: format!(
                    "{} mode refuses mutating shell command; switch to Build mode (Shift+Tab) — {}",
                    mode.as_str(),
                    build_mode_sandbox_hint()
                ),
                silent: false,
            });
        }
        if matches!(
            request.capability,
            PermissionCapability::Shell
                | PermissionCapability::Git
                | PermissionCapability::Compiler
        ) {
            let Some(command) = request.metadata.get("command") else {
                return Some(PermissionVerdict {
                    action: PermissionAction::Deny,
                    matched_rule: None,
                    reason: format!(
                        "{} mode refuses shell command with no command text",
                        mode.as_str()
                    ),
                    silent: false,
                });
            };
            return match classify_plan_mode_shell_command(command) {
                PlanModeShellSafety::ReadOnly => None,
                PlanModeShellSafety::Mutating => Some(PermissionVerdict {
                    action: PermissionAction::Deny,
                    matched_rule: None,
                    reason: format!(
                        "{} mode refuses mutating shell command; switch to Build mode (Shift+Tab) — {}",
                        mode.as_str(),
                        build_mode_sandbox_hint()
                    ),
                    silent: false,
                }),
                PlanModeShellSafety::NeedsApproval => Some(PermissionVerdict {
                    action: PermissionAction::Ask,
                    matched_rule: None,
                    reason: format!(
                        "{} mode requires approval for unproven shell command",
                        mode.as_str()
                    ),
                    silent: false,
                }),
            };
        }
    }
    if !mode_refuses_capability(mode, request.capability, plan_edit_allowed) {
        return None;
    }
    let reason = if mode == SessionMode::Plan && request.capability == PermissionCapability::Edit {
        // Prefer the pre-canonicalized path for display so the message
        // shows the resolved (drive-letter-normalized, UNC-resolved,
        // junction-followed) form that the permission gate actually compared.
        match active_plan_canon.as_deref().or(active_plan_path) {
            Some(active) => {
                // Normalise both paths to forward-slashes so the message is
                // readable on Windows (where Display would otherwise print
                // backslashes) and to help users spot drive-letter or UNC
                // differences. The guard itself uses canonicalize/PathBuf
                // equality; this is display-only.
                let active_display = active.display().to_string().replace('\\', "/");
                let target_display = request.target.replace('\\', "/");
                format!(
                    "Plan mode: only the active plan file is editable \
                     (active: {active_display}; requested: {target_display}). \
                     If paths differ only in drive-letter case, UNC prefix, or \
                     junction resolution, accept the plan-handoff prompt to reload the session.",
                )
            }
            None => format!(
                "{} mode refuses {} (no active plan file to edit)",
                mode.as_str(),
                request.capability.as_str()
            ),
        }
    } else {
        let base = format!(
            "{} mode refuses {}",
            mode.as_str(),
            request.capability.as_str()
        );
        // Append sandbox readiness hint for capabilities that involve
        // shell execution so the user knows what Build mode would do.
        if mode == SessionMode::Plan
            && matches!(
                request.capability,
                PermissionCapability::Shell
                    | PermissionCapability::Git
                    | PermissionCapability::Compiler
                    | PermissionCapability::Destructive
            )
        {
            format!(
                "{}; switch to Build mode (Shift+Tab) — {}",
                base,
                build_mode_sandbox_hint()
            )
        } else {
            base
        }
    };
    Some(PermissionVerdict {
        action: PermissionAction::Deny,
        matched_rule: None,
        reason,
        silent: false,
    })
}

/// Single source of truth for whether a session mode forbids a capability.
/// Plan mode is mutation-gated, not shell-gated. This capability-only filter
/// is used for schema advertisement; [`mode_permission_verdict`] adds
/// command-level shell checks at runtime so broad Git/Compiler/Shell
/// capabilities cannot run repo-mutating commands just because the default
/// policy allows them. The
/// capability list is intentionally exhaustive (`match`) so adding a new
/// capability is a compile-time prompt to decide whether plan mode admits it.
/// `plan_edit_allowed` is computed by
/// `plan_mode::plan_edit_allowed_in_workspace` at schema-build sites and by
/// `mode_permission_verdict`'s pre-canonicalized pair
/// (`plan_mode::canonicalize_active_plan_path` +
/// `plan_mode::is_active_plan_path_with_canon`) at runtime (issue 2).
fn mode_refuses_capability(
    mode: SessionMode,
    capability: PermissionCapability,
    plan_edit_allowed: bool,
) -> bool {
    if mode == SessionMode::Build {
        return false;
    }
    match capability {
        PermissionCapability::Read
        | PermissionCapability::Search
        | PermissionCapability::Shell
        | PermissionCapability::Git
        | PermissionCapability::Network
        | PermissionCapability::Mcp
        | PermissionCapability::Compiler => false,
        PermissionCapability::Edit => !plan_edit_allowed,
        PermissionCapability::Destructive => true,
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
        silent = verdict.silent,
        matched_source,
        matched_target,
        reason = %verdict.reason,
        "permission verdict",
    );
}

/// Static placeholder sent to the model when a silent-deny rule fires. Kept
/// deliberately short so boilerplate policy denials (e.g. an absolute deny
/// rule for `rm -rf /`) do not burn tool-result tokens with a structured
/// `capability=...; target=...; risk=...` line on every retry. The audit
/// JSONL still receives the full `verdict.reason` via [`log_silent_deny`].
const SILENT_DENY_MODEL_MESSAGE: &str = "action denied by policy";

/// Build the deny reason the model sees on its tool-result message. For
/// silent rules, returns the minimal [`SILENT_DENY_MODEL_MESSAGE`]; otherwise
/// returns the redacted full reason. The full reason is preserved in the
/// audit JSONL by [`log_silent_deny`] before this returns.
fn verdict_deny_reason_for_model(
    context: &PermissionDecisionContext,
    verdict: &PermissionVerdict,
) -> String {
    if verdict.silent {
        SILENT_DENY_MODEL_MESSAGE.to_string()
    } else {
        context.redactor.redact(&verdict.reason).text
    }
}

/// Write a `permission_denied_silent` audit event with the full reason and
/// matched-rule shape. The model only sees `SILENT_DENY_MODEL_MESSAGE`, so
/// this is the only place the rich diagnostics land for these rules.
fn log_silent_deny(
    context: &PermissionDecisionContext,
    request: &PermissionRequest,
    verdict: &PermissionVerdict,
) {
    let matched = verdict.matched_rule.as_ref();
    log_session_event(
        context.session_log.as_ref(),
        &context.redactor,
        "permission_denied_silent",
        Some(context.turn_id),
        Some(verdict.reason.clone()),
        json!({
            "reason": verdict.reason.clone(),
            "tool": request.tool_name.clone(),
            "capability": request.capability.as_str(),
            "target": request.target.clone(),
            "risk": request.risk.as_str(),
            "matched_rule": matched.map(|rule| json!({
                "capability": rule.capability.clone(),
                "target": rule.target.clone(),
                "source": rule.source.as_str(),
            })),
        }),
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

/// Result of the out-of-band shell-classifier LLM call: verdict, billed
/// cost snapshot, and the model the classifier actually used. The caller
/// folds `cost` into session accounting and keys the per-model ledger by
/// `model`, mirroring `AiReviewerResult` so a future move of the
/// classifier onto a cheap-tier model (separate from `config.model`) does
/// not silently misroute ledger rows.
///
/// **Cancellation gap (intentional, bounded):** the classifier loop returns
/// `None` on `LlmEvent::Cancelled`, dropping any partial cost the provider
/// streamed before cancellation. Unlike `TurnRuntime::run`'s
/// `fold_partial_cancel_cost` path, this path has no broker to attribute
/// the partial spend to. The gap is bounded by the classifier's tiny
/// prompt (one user message, `max_output_tokens: 80`) and the next turn
/// reseeds session accounting from `state.cost`. Closing it fully needs
/// the unified out-of-band LLM accounting path (Category3.md `:44`).
struct ClassifierResult {
    verdict: PermissionVerdict,
    cost: CostSnapshot,
    model: Arc<str>,
}

async fn classify_ambiguous_shell(
    provider: Arc<dyn LlmProvider>,
    config: &AppConfig,
    request: &PermissionRequest,
    cancel: CancellationToken,
) -> Option<ClassifierResult> {
    let command = request.metadata.get("command")?;
    let prompt = format!(
        "Classify this shell command for a coding agent permission gate.\n\
Return compact JSON with fields {{\"action\": \"ask\"|\"deny\", \"reason\": string}}.\n\
Never return allow.\n\
Command: {command:?}\n\
Working target: {:?}",
        request.target
    );
    let output_schema = provider_honors_output_schema(provider.name(), &config.model)
        .then(shell_classifier_output_schema);
    let model: Arc<str> = Arc::from(config.model.as_str());
    let llm_request = LlmRequest {
        model: model.clone(),
        instructions: Arc::from(
            "You classify shell-command risk for a local coding agent. Return JSON only.",
        ),
        input: Arc::from(vec![LlmInputItem::UserText(prompt)]),
        max_output_tokens: Some(80),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };
    let mut stream = provider.stream_response(llm_request, cancel.clone());
    let mut text = String::new();
    let mut cost = CostSnapshot::default();
    while let Some(event) = next_llm_stream_event(&mut stream, &cancel, config.stream_idle_timeout)
        .await
        .ok()?
    {
        match event {
            LlmEvent::TextDelta(delta) => text.push_str(&delta),
            LlmEvent::Completed { cost: snap, .. } => {
                cost = snap;
                break;
            }
            // Cancellation drops any partial cost streamed so far (see the
            // `ClassifierResult` doc comment for the bounded gap rationale).
            LlmEvent::Cancelled => return None,
            LlmEvent::Started
            | LlmEvent::ToolCall(_)
            | LlmEvent::ReasoningDelta { .. }
            | LlmEvent::ReasoningDone(_)
            | LlmEvent::ContextOverflow { .. }
            | LlmEvent::ServerModel(_) => {}
            // The classifier verdict is parsed from `TextDelta` only; the
            // refusal/citation/tool-args-delta additive variants carry
            // nothing the verdict parser reads. Named explicitly so the
            // wildcard stays reserved for unknown future variants.
            LlmEvent::Refusal { .. }
            | LlmEvent::Citation { .. }
            | LlmEvent::ToolCallDelta { .. } => {}
            // `LlmEvent` is `#[non_exhaustive]`; unknown future variants
            // flow past without disturbing the classifier — they get a
            // dedicated arm once the verdict parser learns to read them.
            _ => {}
        }
    }
    // Fill in estimated cost when the provider did not return a token count.
    if cost.estimated_usd_micros.is_none() {
        cost.estimated_usd_micros = estimate_cost(provider.name(), &model, &cost);
    }
    Some(ClassifierResult {
        verdict: parse_classifier_verdict(&text),
        cost,
        model,
    })
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
            silent: false,
        },
        // Allow from the classifier is intentionally disallowed - we keep the
        // verdict at Ask so a human still confirms.
        _ => PermissionVerdict {
            action: PermissionAction::Ask,
            matched_rule: None,
            reason: format!("shell classifier requires approval: {reason_excerpt}"),
            silent: false,
        },
    }
}

/// Strict JSON-schema contract mirroring what [`extract_json_action`]
/// deserializes for the shell classifier: an `action` constrained to the
/// two values the classifier prompt permits (`ask`/`deny` — `allow` is
/// disallowed by design) plus a free-text `reason`. Attached only on
/// providers that forward `output_schema`
/// ([`provider_honors_output_schema`]) so the cheap classifier model emits
/// schema-valid JSON instead of fenced/prose-wrapped output that costs a
/// retry round; providers that drop the schema keep the loose-parse path
/// (`extract_loose_action`) and behave exactly as before.
fn shell_classifier_output_schema() -> LlmOutputSchema {
    LlmOutputSchema {
        name: "shell_command_verdict".to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        PermissionAction::Ask.as_str(),
                        PermissionAction::Deny.as_str(),
                    ],
                },
                "reason": { "type": "string" },
            },
            "required": ["action", "reason"],
            "additionalProperties": false,
        }),
        strict: true,
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
        // `shell:*` is the catch-all the shell analyzer assigns to dynamic,
        // unparseable, or unknown-env commands precisely so each one re-prompts.
        // Persisting it as an Allow rule would silently auto-approve every future
        // dynamic command, defeating that guard — refuse it like any other
        // blanket target. (Deny rules may keep it: a broad deny fails closed.)
        if rule.target == "shell:*" {
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
            // LlmToolSpec is the provider-facing surface and intentionally
            // stays a free-shape `Value` so it can be embedded directly into
            // every provider request body. Serializing the typed
            // [`squeezy_tools::JsonSchema`] back into a `Value` here is the
            // only boundary point where the conversion runs; the
            // registration-time `deny_unknown_fields` guard on
            // [`squeezy_tools::ToolSpec::parameters`] has already rejected
            // any first-party drift before this point.
            parameters: serde_json::to_value(&spec.parameters)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
            strict: false,
        }),
    }
}

/// Drop entries from `tools` whose name appears in
/// `tools_config.excluded`. The list is small (typically <20 names) and
/// the excluded set is short (used today by graph-vs-no-graph eval
/// scenarios), so a per-entry scan is fine.
pub(crate) fn retain_non_excluded_tools(
    tools: &mut Vec<AdvertisedTool>,
    tools_config: &ToolSchemaConfig,
) {
    if tools_config.excluded.is_empty() {
        return;
    }
    tools.retain(|tool| !tools_config.is_excluded(tool.spec.name.as_str()));
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
    catalog: &SubagentCatalog,
) -> Vec<AdvertisedTool> {
    let mut tools = Vec::new();
    if subagents.enabled {
        tools.push(delegate_advertised_tool(catalog));
        if subagents.explore_enabled {
            tools.push(explore_advertised_tool());
        }
        tools.push(delegate_plan_advertised_tool());
        tools.push(delegate_review_advertised_tool());
        tools.push(delegate_chain_advertised_tool());
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

fn delegate_advertised_tool(catalog: &SubagentCatalog) -> AdvertisedTool {
    let custom_agents: Vec<&SubagentDefinition> = catalog.user_provided().collect();
    let mut description = "Delegate open-ended work — research AND scoped implementation — to an isolated subagent. \
                          The subagent can read, edit, and run with the same permissions you have (edits/shell still prompt for approval where your policy requires it); it reports back a structured summary. \
                          Reserve it for genuinely multi-pass, context-isolating, or cross-cutting work — \
                          a task spanning several rounds of discovery, or one whose intermediate reading would bloat your context, or one that fans out across unrelated areas. \
                          NOT for greetings, casual replies, or simple questions the parent can answer directly. \
                          A single-pass enumeration or audit — grep/scan a known set of files or symbols once and report — is NOT multi-pass: do it yourself in-context with grep/read, or via the bounded `explore` tool, rather than firing a whole-task delegate. A cold subagent re-explores from scratch and runs the same model, so on bounded single-pass work it is pure overhead and slower. \
                          Do NOT delegate enumeration or extraction over a list of files or symbols you ALREADY have (e.g. from a graph/hierarchy result) — read or slice those yourself; the subagent re-reads the same files, so delegating known-target extraction is pure overhead. Delegate only when the set of files to inspect is itself unknown, large, and must be discovered across multiple passes. \
                          `prompt` is required; the parent receives only a structured summary, supporting receipts, and separate spend metrics."
        .to_string();
    let mut properties = json!({
        "prompt": {
            "type": "string",
            "description": "Required, non-empty: a concrete instruction for the subagent (research or a scoped change)."
        },
        "scope": {
            "type": ["string", "null"],
            "description": "Optional bounded scope such as paths, modules, symbols, or exclusions."
        }
    });
    // Advertise any disk-loaded custom subagents so the model can route to one
    // by name via the `agent` parameter. Each runs its `.md` body as its system
    // prompt and is restricted to its declared read-only tools.
    if !custom_agents.is_empty() {
        let listing = custom_agents
            .iter()
            .map(|agent| format!("`{}` — {}", agent.name, agent.description))
            .collect::<Vec<_>>()
            .join("; ");
        description.push_str(&format!(
            " Set `agent` to one of the available custom subagents to run it with its own system prompt and model: {listing}."
        ));
        let agent_names: Vec<&str> = custom_agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();
        properties["agent"] = json!({
            "type": "string",
            "enum": agent_names,
            "description": "Optional: name of a custom subagent (from .squeezy/agents) to run for this task. Omit to use the default general-purpose subagent."
        });
    }
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: Arc::new(LlmToolSpec {
            name: DELEGATE_TOOL_NAME.to_string(),
            description,
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": properties,
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

fn delegate_chain_advertised_tool() -> AdvertisedTool {
    AdvertisedTool {
        capability: PermissionCapability::Read,
        spec: Arc::new(LlmToolSpec {
            name: DELEGATE_CHAIN_TOOL_NAME.to_string(),
            description: format!(
                "Run a sequential chain of Delegate subagents. Each step's `prompt` may include the literal substring `{placeholder}`, which is replaced verbatim with the prior step's summary before the subagent is invoked. Use this when later steps must consume earlier output; for independent fanouts, issue multiple `delegate` calls in the same turn instead — they run in parallel. Chain length is capped at {max_steps} steps.",
                placeholder = DELEGATE_CHAIN_PREVIOUS_PLACEHOLDER,
                max_steps = DELEGATE_CHAIN_MAX_STEPS,
            ),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Ordered list of delegate steps to run sequentially.",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "prompt": {
                                    "type": "string",
                                    "description": "Required: instruction for this step. May include `{previous}` to substitute the prior step's summary."
                                },
                                "model": {
                                    "type": ["string", "null"],
                                    "description": "Optional per-step model override; defaults to the parent's delegate model when omitted."
                                },
                                "scope": {
                                    "type": ["string", "null"],
                                    "description": "Optional bounded scope passed through to the subagent for this step."
                                }
                            },
                            "required": ["prompt"]
                        }
                    }
                },
                "required": ["steps"]
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
            description: "Ask a cheaper read-only exploration subagent to scan the codebase with Squeezy semantic tools. \
                          Use only for a non-trivial codebase question — \
                          NOT for greetings, chitchat, or questions the parent can answer directly from context. \
                          `prompt` is required and must contain a concrete codebase question."
                .to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Required, non-empty: a concrete codebase question or task context to investigate."
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
        // The custom-agent listing lives on the enriched `delegate` spec built
        // by `core_control_tools`; `push_tool_spec_by_name` resolves that copy
        // from the advertised `tools` slice directly, so this arm only fires as
        // the bare fallback when `delegate` isn't otherwise advertised. Building
        // from an empty catalog here is deliberately leaf: `delegate_advertised_tool`
        // only reads the catalog, so it cannot re-enter the spec builder.
        DELEGATE_TOOL_NAME => Some(delegate_advertised_tool(&SubagentCatalog::empty())),
        EXPLORE_TOOL_NAME => Some(explore_advertised_tool()),
        DELEGATE_PLAN_TOOL_NAME => Some(delegate_plan_advertised_tool()),
        DELEGATE_REVIEW_TOOL_NAME => Some(delegate_review_advertised_tool()),
        DELEGATE_CHAIN_TOOL_NAME => Some(delegate_chain_advertised_tool()),
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

/// Tool name whose outputs deliver skill bodies into the transcript. Used to
/// attribute output bytes to the skills bucket in [`conversation_shape`].
const LOAD_SKILL_TOOL_NAME: &str = "load_skill";

/// Render an MCP server's live status into the short label shown by `/context`.
fn format_mcp_status(status: &squeezy_tools::McpServerStatus) -> String {
    match status {
        squeezy_tools::McpServerStatus::Starting => "starting".to_string(),
        squeezy_tools::McpServerStatus::Ready {
            tools_count,
            cached,
        } => {
            if *cached {
                format!("ready (cached, {tools_count} tools)")
            } else {
                format!("ready ({tools_count} tools)")
            }
        }
        squeezy_tools::McpServerStatus::Stale {
            tools_count,
            outcome,
        } => format!("stale ({tools_count} tools, {outcome:?})"),
        squeezy_tools::McpServerStatus::Failed { error } => format!("failed: {error}"),
        squeezy_tools::McpServerStatus::Cancelled => "cancelled".to_string(),
    }
}

fn conversation_shape(conversation: &[LlmInputItem]) -> ConversationShape {
    let mut shape = ConversationShape {
        items: conversation.len(),
        ..ConversationShape::default()
    };
    // Call ids whose originating `FunctionCall` was `load_skill`, so the
    // matching output bytes can be attributed to the "skills" bucket rather
    // than left lumped into generic tool outputs. A `FunctionCall` always
    // precedes its `FunctionCallOutput`, so a single forward pass suffices.
    let mut load_skill_call_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
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
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                shape.function_calls += 1;
                shape.text_bytes += arguments.to_string().len();
                if name == LOAD_SKILL_TOOL_NAME {
                    load_skill_call_ids.insert(call_id.as_str());
                }
            }
            LlmInputItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                shape.function_outputs += 1;
                shape.tool_output_bytes += output.len();
                if load_skill_call_ids.contains(call_id.as_str()) {
                    shape.skill_output_bytes += output.len();
                }
            }
            LlmInputItem::Reasoning(payload) => {
                shape.reasoning_items += 1;
                shape.reasoning_bytes += payload.display_text().len();
            }
            LlmInputItem::Image { bytes, .. } => {
                shape.image_items += 1;
                shape.image_bytes += bytes.len();
            }
            // `LlmInputItem` is `#[non_exhaustive]`; unknown future variants
            // increment no counters until a dedicated arm exists.
            _ => {}
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
        shape.redactions += attachment.redactions;
        match attachment.status {
            ContextAttachmentStatus::Attached => {
                shape.active += 1;
                shape.stored_bytes += attachment.stored_bytes;
            }
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
    // The `delegate` spec carried in `tools` is enriched with the discovered
    // custom-agent listing by `core_control_tools`. Prefer that already-built
    // copy over the bare synthetic rebuild so the lazy-schema path advertises
    // the same `agent` selection the eager path does. Resolving it from `tools`
    // (rather than calling back into a spec builder) keeps this path a strict
    // leaf: it never re-enters `request_tool_specs` / `push_tool_spec_by_name`.
    if name == DELEGATE_TOOL_NAME
        && let Some(tool) = tools.iter().find(|tool| tool.spec.name == name)
    {
        if !mode_refuses_capability(mode, tool.capability, plan_edit_allowed) {
            specs.push(Arc::clone(&tool.spec));
        }
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
    // first-party tool definition with `cache_control: ephemeral` (see
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
    for (idx, row) in rows.iter().enumerate() {
        if idx > 0 {
            index.push('\n');
        }
        index.push_str(row);
    }
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

fn first_line_of_description(description: &str) -> &str {
    description.lines().next().unwrap_or_default().trim()
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
        LlmInputItem::FunctionCallOutput {
            call_id,
            output,
            content_parts,
            is_error,
        } => LlmInputItem::FunctionCallOutput {
            call_id,
            output: redactor.redact(&output).text,
            // No producer populates `content_parts` yet, but redact each
            // text part defensively so a future structured-tool-result
            // path can't slip a secret past the redactor through the array
            // shape. `Image` bytes are raw binary (secret detection runs on
            // text, not pixels) so they pass through unchanged, mirroring
            // the `LlmInputItem::Image` arm below.
            content_parts: content_parts.map(|parts| {
                parts
                    .into_iter()
                    .map(|part| match part {
                        squeezy_llm::ToolResultPart::Text { text } => {
                            squeezy_llm::ToolResultPart::Text {
                                text: redactor.redact(&text).text,
                            }
                        }
                        image @ squeezy_llm::ToolResultPart::Image { .. } => image,
                    })
                    .collect()
            }),
            is_error,
        },
        // Reasoning payloads are model-signed blobs. Redacting the opaque
        // bytes would break replay; redact only the human-readable summary
        // fields so secrets that surface in the chain-of-thought are hidden
        // from the TUI without invalidating the signature.
        LlmInputItem::Reasoning(payload) => {
            LlmInputItem::Reasoning(redact_reasoning_payload(payload, redactor))
        }
        // Image payloads are raw binary content (PNG/JPEG/...); secret
        // detection runs on text, not pixels. Pass the bytes through
        // unchanged so the provider's vision pipeline still receives the
        // original image.
        LlmInputItem::Image { media_type, bytes } => LlmInputItem::Image { media_type, bytes },
        // `LlmInputItem` is `#[non_exhaustive]`; pass unknown future
        // variants through unchanged so they survive the redaction pass.
        other => other,
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
/// "already redacted" invariant and the conversation has no orphan
/// `FunctionCallOutput` whose declaring `FunctionCall` is missing AND
/// no orphan `FunctionCall` whose answering `FunctionCallOutput` is
/// missing. Used to upgrade conversation state loaded from a resume
/// tape that may pre-date either invariant (insertion-time redaction
/// or compaction's orphan-drop). The pairing checks are last-resort
/// safety nets: OpenAI 400s the turn with *"No tool call found for
/// function call output with call_id …"* on orphan outputs, and the
/// Anthropic Messages API rejects the turn with *"tool_use blocks must
/// be followed by a tool_result"* on orphan calls. Both failures are
/// sticky — every retry hits the same wedged conversation until the
/// user `/clear`s.
fn redact_llm_input_items(input: Vec<LlmInputItem>, redactor: &Redactor) -> Vec<LlmInputItem> {
    let redacted: Vec<LlmInputItem> = input
        .into_iter()
        .map(|item| redact_input_item(item, redactor))
        .collect();
    let without_orphan_outputs = drop_orphan_function_call_outputs(redacted);
    repair_orphan_function_calls(without_orphan_outputs)
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

/// Build a best-effort [`CostSnapshot`] for a round that was cancelled
/// mid-stream — before the provider emitted a `Completed` event with
/// usage. Returns `None` when no assistant or reasoning bytes were
/// observed; in that case the provider may or may not have already
/// charged us for the input prompt (its decision-to-cancel races with
/// our send), so attributing input tokens would be guesswork. Once any
/// output byte has streamed back we know the provider definitely read
/// the prompt, so both input and output sides of the estimate are
/// folded in. Tokens come from the per-provider calibration; the
/// dollar cost comes from the pricing registry via [`estimate_cost`],
/// the same fallback the `Completed` arm uses when the provider stays
/// silent on `estimated_usd_micros`.
fn partial_cancel_cost(
    provider: &str,
    model: &str,
    request_input_bytes: u64,
    round_output_bytes: u64,
    calibration: &squeezy_llm::TokenCalibration,
) -> Option<CostSnapshot> {
    if round_output_bytes == 0 {
        return None;
    }
    let bytes_per_token = calibration.bytes_per_token(provider);
    let input_tokens = bytes_to_tokens(request_input_bytes, bytes_per_token);
    let output_tokens = bytes_to_tokens(round_output_bytes, bytes_per_token);
    let mut snapshot = CostSnapshot {
        input_tokens: (input_tokens > 0).then_some(input_tokens),
        output_tokens: (output_tokens > 0).then_some(output_tokens),
        ..CostSnapshot::default()
    };
    snapshot.estimated_usd_micros = estimate_cost(provider, model, &snapshot);
    Some(snapshot)
}

fn bytes_to_tokens(bytes: u64, bytes_per_token: f64) -> u64 {
    if bytes == 0 {
        return 0;
    }
    let bpt = bytes_per_token.max(0.1);
    ((bytes as f64) / bpt).ceil() as u64
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

/// Build the `LlmInputItem::Image` items for a turn from the agent's
/// active context attachments. Only attachments with
/// `kind.is_routable_image()` and a populated
/// `image_data_base64` participate; the helper silently drops
/// attachments missing the decoded payload (resumed legacy
/// `UnsupportedImage` entries) so a stale persisted attachment never
/// crashes the turn build.
fn image_input_items_for_attachments(attachments: &[ContextAttachment]) -> Vec<LlmInputItem> {
    use base64::Engine as _;
    let mut items = Vec::new();
    for attachment in attachments {
        if !attachment.kind.is_routable_image() {
            continue;
        }
        let (Some(media_type), Some(encoded)) = (
            attachment.image_media_type.as_deref(),
            attachment.image_data_base64.as_deref(),
        ) else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()) else {
            continue;
        };
        items.push(LlmInputItem::Image {
            media_type: media_type.to_string(),
            bytes: Arc::from(bytes.into_boxed_slice()),
        });
    }
    items
}

/// Build the `LlmInputItem::Document` items for a turn from the agent's
/// active context attachments. Mirrors
/// [`image_input_items_for_attachments`]: only attachments with
/// `kind.is_routable_document()` and a populated `image_data_base64`
/// (the shared byte slot) participate, and entries missing a decodable
/// payload are silently dropped so a stale persisted attachment never
/// crashes the turn build. The human-facing `label` rides through as the
/// document `name` so providers can echo it.
fn document_input_items_for_attachments(attachments: &[ContextAttachment]) -> Vec<LlmInputItem> {
    use base64::Engine as _;
    let mut items = Vec::new();
    for attachment in attachments {
        if !attachment.kind.is_routable_document() {
            continue;
        }
        let (Some(media_type), Some(encoded)) = (
            attachment.image_media_type.as_deref(),
            attachment.image_data_base64.as_deref(),
        ) else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()) else {
            continue;
        };
        items.push(LlmInputItem::Document {
            media_type: media_type.to_string(),
            name: attachment.label.clone(),
            bytes: Arc::from(bytes.into_boxed_slice()),
        });
    }
    items
}

fn has_large_non_image_attachment(attachments: &[ContextAttachment], threshold: u32) -> bool {
    if threshold == 0 {
        return false;
    }
    attachments
        .iter()
        .filter(|attachment| attachment.is_active())
        .filter(|attachment| attachment.kind != ContextAttachmentKind::Image)
        .map(|attachment| attachment.original_bytes as u64)
        .sum::<u64>()
        >= u64::from(threshold)
}

fn format_user_text_with_context(input: &str, attachments: &[ContextAttachment]) -> String {
    if attachments.is_empty() {
        return input.to_string();
    }
    let mut output = input.to_string();
    output.push_str("\n\nAttached context references:\n");
    for attachment in attachments {
        let _ = writeln!(
            output,
            "- {reference} id={id} source={source} kind={kind} label={label:?} bytes={bytes} stored_bytes={stored_bytes} truncated={truncated}",
            reference = attachment.reference(),
            id = attachment.id,
            source = attachment.source.as_str(),
            kind = attachment.kind.as_str(),
            label = attachment.label,
            bytes = attachment.original_bytes,
            stored_bytes = attachment.stored_bytes,
            truncated = attachment.truncated,
        );
        if let Some(path) = &attachment.path {
            let _ = writeln!(output, "  path={path:?}");
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
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut output = String::with_capacity(text.len());
    output.push_str(first);
    for word in words {
        output.push(' ');
        output.push_str(word);
    }
    output
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
/// The cache directive (both the legacy `cache_key` and the new `cache`
/// field) is derived from the live session id, which changes across
/// record/replay runs, so both must be excluded from the divergence hash.
fn replay_request_view(request: &LlmRequest) -> LlmRequest {
    let mut view = request.clone();
    view.cache_key = None;
    view.cache = CacheSpec::default();
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
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
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
        // `content_parts` / `is_error` from the structured-tool-result
        // extension are dropped on persistence — the resume schema
        // hasn't been bumped yet, so a checkpoint round-trip materializes
        // a plain string output. Phase 4 lowers structured arrays at the
        // provider boundary directly from the live `LlmInputItem`, so
        // the loss only affects the resume edge case.
        LlmInputItem::FunctionCallOutput {
            call_id, output, ..
        } => ResumeItem::FunctionCallOutput { call_id, output },
        LlmInputItem::Reasoning(payload) => ResumeItem::Reasoning { payload },
        LlmInputItem::Image { media_type, bytes } => ResumeItem::Image {
            media_type,
            data_base64: BASE64_STANDARD.encode(bytes.as_ref()),
        },
        // `ResumeItem` has no `Document` variant yet (the resume schema
        // lives in `squeezy-store` and bumping it is a separate change).
        // Until it gains one, persist a descriptive placeholder that names
        // the attachment and its type instead of letting the catch-all
        // silently flatten a fully-defined document into an empty
        // `UserText` (data loss). The original bytes are dropped on resume,
        // but the user/model at least sees that a document was attached.
        // TODO: add a `ResumeItem::Document { media_type, name, data_base64 }`
        // variant to `squeezy-store` and round-trip the bytes like `Image`.
        LlmInputItem::Document {
            media_type, name, ..
        } => ResumeItem::UserText {
            text: format!("[document attachment dropped on resume: {name} ({media_type})]"),
        },
        // `LlmInputItem` is `#[non_exhaustive]`; unknown future variants
        // round-trip through an empty user text marker until the resume
        // schema gains a dedicated representation for them.
        _ => ResumeItem::UserText {
            text: String::new(),
        },
    }
}

fn resume_item_for_json(item: LlmInputItem) -> Value {
    serde_json::to_value(llm_input_to_resume_item(item))
        .unwrap_or_else(|_| json!({"error": "resume item serialization failed"}))
}

fn resume_item_to_llm_input(item: ResumeItem) -> LlmInputItem {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
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
            LlmInputItem::function_output(call_id, output)
        }
        ResumeItem::Reasoning { payload } => LlmInputItem::Reasoning(payload),
        ResumeItem::Image {
            media_type,
            data_base64,
        } => {
            let bytes = BASE64_STANDARD
                .decode(data_base64.as_bytes())
                .unwrap_or_default();
            LlmInputItem::Image {
                media_type,
                bytes: std::sync::Arc::from(bytes.into_boxed_slice()),
            }
        }
    }
}

/// Combined token count from a `CostSnapshot`. Sums `input_tokens` and
/// `output_tokens` when present; falls back to `None` if the provider
/// reported no usage. `reasoning_output_tokens` is the subset of
/// `output_tokens` that was reasoning (see
/// docs/internal/cost-saving/10-token-accounting.md), so it is already
/// inside `output_tokens` and must not be added again.
fn total_tokens_from_cost(cost: &CostSnapshot) -> Option<u64> {
    let mut total: u64 = 0;
    let mut saw_any = false;
    for value in [cost.input_tokens, cost.output_tokens]
        .into_iter()
        .flatten()
    {
        saw_any = true;
        total = total.saturating_add(value);
    }
    if saw_any { Some(total) } else { None }
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
    /// Per-tool structured preview lines (diff, syntax-highlighted command,
    /// host vs URL, etc.) produced by `ToolRegistry::preview_for`. The TUI
    /// renders each variant with its own style (Diff -> red/green,
    /// Highlighted -> palette, Warning -> orange). Empty when no preview
    /// is available for the tool.
    pub preview: Vec<squeezy_tools::preview::PreviewLine>,
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

/// Return value of [`permission_decision`] and
/// [`permission_decision_for_request`]. Carries the gate outcome together
/// with any out-of-band LLM cost incurred by the AI reviewer during this
/// decision so the caller can fold it into the active turn's [`CostBroker`]
/// without a separate channel.
struct PermissionOutcome {
    decision: ApprovalDecision,
    /// Total reviewer spend in USD micros recorded during this permission
    /// evaluation. Zero when the reviewer did not run or had no priced
    /// response. Must be folded into the active [`CostBroker`] by the
    /// turn loop so the live session-cost snapshot and cap checks stay
    /// accurate within the turn.
    reviewer_usd_micros: u64,
}

impl PermissionOutcome {
    fn no_reviewer_cost(decision: ApprovalDecision) -> Self {
        Self {
            decision,
            reviewer_usd_micros: 0,
        }
    }
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
    SkillActivationWarning {
        turn_id: TurnId,
        name: String,
        message: String,
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
    /// Live estimate of the request context about to be sent for a provider
    /// round. This lets the TUI status line update context usage mid-turn,
    /// after tool results/reasoning have been appended, instead of waiting for
    /// the final `Completed` event.
    ContextUsageUpdate {
        turn_id: TurnId,
        input_tokens: u64,
        context_window_tokens: Option<u64>,
    },
    SubagentStarted {
        turn_id: TurnId,
        id: SubagentId,
        agent: String,
        prompt: String,
    },
    /// A subagent's completed tool result, forwarded with its full structure so
    /// the parent can render it as a rail card in the subagent's transcript
    /// view rather than a flat `completed X` status line.
    SubagentToolResult {
        turn_id: TurnId,
        id: SubagentId,
        agent: String,
        result: ToolResult,
    },
    SubagentCompleted {
        turn_id: TurnId,
        id: SubagentId,
        agent: String,
        summary: String,
        metrics: TurnMetrics,
    },
    SubagentFailed {
        turn_id: TurnId,
        id: SubagentId,
        agent: String,
        error: String,
        metrics: TurnMetrics,
    },
    /// The subagent registry refused to admit a new subagent. Fires before
    /// any provider work, in lieu of `SubagentStarted`/`SubagentFailed`,
    /// so the TUI can surface a "concurrency cap reached, 4 already
    /// running" warning instead of a bare failure with no diagnostic
    /// hook. `active` is the count observed at rejection time (always
    /// `>= limit` for `ConcurrencyCap`); both are surfaced so future
    /// rejection reasons (e.g. depth cap) can reuse the same shape.
    SubagentRejected {
        turn_id: TurnId,
        agent: String,
        reason: SubagentRejectionReason,
        limit: usize,
        active: usize,
    },
    /// A source citation received from the provider stream (OpenAI
    /// annotations, xAI Live Search). `text_index` is the byte offset in
    /// the running assistant-text buffer the citation refers to. Consumers
    /// that do not display source attribution can ignore this event.
    ///
    /// **Emission deferred** (same constraint as
    /// [`AgentEvent::ControlToolTrace`] below): constructing a
    /// `sizeof(AgentEvent)`-byte (~1 KiB) temporary inside the deeply
    /// nested `TurnRuntime::run` stream loop pushes borderline tests over
    /// the default thread-stack ceiling on macOS/ARM64 debug builds. The
    /// variant + consumer-side handlers are in place; emission will move
    /// to a dedicated transcript-sink path once that exists.
    Citation {
        turn_id: TurnId,
        text_index: u32,
        source: CitationSource,
    },
    /// A hidden control-plane tool completed without consuming normal tool
    /// budget. Intended to be emitted for `load_tool_schema` and
    /// `update_task_state` so debuggers and eval replay can observe
    /// control-plane activity without adding noisy user-facing tool cards.
    ///
    /// **Emission deferred**: creating a ~1 KiB `AgentEvent` temporary inside
    /// the deeply-nested `execute_tool_calls` / `TurnRuntime::run` call stack
    /// pushes borderline tests over the default thread-stack limit on macOS.
    /// The infrastructure is ready (all match sites handle this variant via
    /// `_ => {}` or `{ .. } => {}`); emission will be wired up once the
    /// control-tool result path is moved off the hot-path stack.
    ControlToolTrace {
        turn_id: TurnId,
        /// Stable tool name token, e.g. `"load_tool_schema"` or
        /// `"update_task_state"`.
        tool_name: String,
        /// Short human-readable summary, e.g. `"schema attached: grep"` or
        /// `"task state updated"`.
        label: String,
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
        /// Provider-reported normalized stop kind from the final round's
        /// stream, if available. Surfaced for eval / regression tooling
        /// so rules can distinguish "stop after content", "stop after
        /// tool calls", "length truncation", etc. `None` when the
        /// provider didn't report one or the stream was synthetic
        /// (e.g. agent-loop short-circuit, replay reconstruction).
        stop_reason: Option<StopReason>,
        /// `true` iff the final round's `Completed` was the canonical
        /// Qwen3 "reasoning-only finish" pattern (see
        /// `LlmEvent::Completed::reasoning_only_stop`).
        reasoning_only_stop: bool,
        /// Session-cumulative cost (token distribution + USD) at turn end, for
        /// the live status-line cost segment. `None` on turns with no
        /// `CostBroker` handle (help / local-tool turns); the TUI then keeps
        /// the last known cumulative value rather than blanking.
        session_cost: Option<CostSnapshot>,
    },
    /// Emitted at most once per session, the first time the running provider
    /// cost crosses `cost_warn_percent` of the configured
    /// `max_session_cost_usd_micros` cap. The TUI renders a transcript
    /// notice; non-TUI consumers (replay tooling, telemetry) can ignore it.
    CostWarning {
        turn_id: TurnId,
        status: CostCapStatus,
    },
    /// Emitted at most once per turn, the first round where a configured
    /// `max_session_cost_usd_micros` cap cannot be enforced because the
    /// active `(provider, model)` has no registry pricing (the per-round
    /// dollar estimate is `None`, so the running total never advances and
    /// the cap can never trip). The TUI renders a transcript notice so the
    /// user knows the guardrail is inert; non-TUI consumers can ignore it.
    CostCapUnenforceable {
        turn_id: TurnId,
        provider: String,
        model: String,
    },
    /// Emitted at most once per session, the first time the shell tool's OS
    /// sandbox backend silently degrades to the best_effort path (probe
    /// failure, runtime sandbox_apply error, etc.). The TUI surfaces a
    /// warning so users see the degradation; the per-call telemetry counter
    /// `approval.best_effort.fallback{tool=shell}` keeps ticking on every
    /// fallback for backend dashboards. `fallback_reason` carries the
    /// human-readable root cause so the TUI can explain the specific failure
    /// (e.g. spawn/pre-exec blocked, probe signal, cached unavailable).
    ShellSandboxBestEffortFallback {
        turn_id: TurnId,
        backend: String,
        fallback_count: u64,
        fallback_reason: Option<String>,
    },
    /// Emitted exactly once, on the first turn of a Windows session, to
    /// surface the steady-state sandbox posture. Unlike
    /// `ShellSandboxBestEffortFallback` (which fires when a previously
    /// capable backend silently downgrades), this variant fires because
    /// Windows Job-Object cleanup is the *intentional* Windows design, not a
    /// runtime fallback. The TUI renders a durable session-level notice so
    /// users running Build-mode shell work on Windows see the isolation
    /// caveat without having to execute a shell command first.
    WindowsSandboxActive {
        turn_id: TurnId,
    },
    /// Fires once per session on Windows when the first shell result reports
    /// `windows-job-object` or `best_effort_unavailable` filesystem isolation.
    /// Unlike [`ShellSandboxBestEffortFallback`] this is not a runtime
    /// failure; it describes the steady-state Windows sandbox posture and lets
    /// the TUI display a Windows-specific safety notice.
    ShellWindowsDegraded {
        turn_id: TurnId,
        backend: String,
        filesystem: String,
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
        /// Session-cumulative cost (token distribution + USD) so far, so the
        /// status-line cost segment ticks up live mid-turn. `None` only if no
        /// broker snapshot was available.
        session_cost: Option<CostSnapshot>,
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
        /// Cumulative cost for the turn at the moment of cancel, including
        /// the partial work of the in-flight round. Mirrors the shape of
        /// [`AgentEvent::Completed::cost`] so cost-reporting consumers
        /// (eval frames, TUI footer, `/cost`) read the same field on both
        /// terminal paths. Defaults to zero for cancel paths that fire
        /// before any provider work has happened (e.g. cancel landing
        /// before the first round's stream starts).
        cost: CostSnapshot,
        /// Per-turn metrics snapshot at the moment of cancel, again
        /// mirroring [`AgentEvent::Completed::metrics`] so consumers can
        /// account for partial spend on a cancelled turn the same way
        /// they do on a completed one.
        metrics: TurnMetrics,
        /// Session-cumulative cost (token distribution + USD) at the moment of
        /// cancel, so the status-line cost segment keeps showing real spend
        /// instead of blanking after a mid-turn break. `None` only on the
        /// watchdog path that has no broker/state handle.
        session_cost: Option<CostSnapshot>,
    },
    Failed {
        turn_id: TurnId,
        error: SqueezyError,
        /// Session-cumulative cost (token distribution + USD) at the moment of
        /// failure, so a failed turn's already-billed partial spend stays on
        /// the status line. `None` on outer/no-broker failure paths; the TUI
        /// then keeps the last known cumulative value.
        session_cost: Option<CostSnapshot>,
    },
    /// Emitted whenever the per-turn router swaps the model on the wire
    /// away from the user's configured parent model. Fires twice on an
    /// escalated turn: once at the start when the cheap tier is
    /// selected, and once mid-turn when the cheap model handed back to
    /// the parent. `from` is the model the agent would otherwise have
    /// used; `to` is the model the next round will dispatch on; `reason`
    /// is a short stable token (`heuristic_slam_dunk_<rule>`,
    /// `llm_judge`, `user_explicit`, `escalated_<signal>`) so TUI and
    /// eval consumers can match on it without parsing prose. `effort` is the
    /// reasoning effort the `to` rung will run at (tier-effort), or `None` when
    /// the rung uses the provider default — surfaced as a live indicator.
    TurnRouted {
        turn_id: TurnId,
        from: String,
        to: String,
        reason: String,
        effort: Option<squeezy_core::ReasoningEffort>,
    },
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
