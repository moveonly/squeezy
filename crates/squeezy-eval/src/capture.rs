use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::driver::EvalError;
use crate::live::LivePrinter;

/// Normalized trace event written one-per-line to `trace.jsonl`.
///
/// We deliberately use a fresh, self-contained schema rather than reusing
/// `squeezy_store::SessionReplayEvent` so we can add eval-specific kinds
/// (approvals, slash commands, action steps, snapshots, perf samples)
/// without churning the replay enum used by the rest of the codebase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalEvent {
    pub schema_version: u32,
    pub ts_unix_ms: u64,
    pub sequence: u64,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(flatten)]
    pub kind: EvalEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvalEventKind {
    UserMessage {
        text: String,
    },
    TurnStarted,
    TurnCompleted {
        metrics: Value,
        cost: Value,
        /// Provider-reported normalized stop kind from the final round
        /// of this turn, propagated from `AgentEvent::Completed`. `None`
        /// when the provider didn't report one or the stream ended
        /// synthetically (e.g. truncated upstream connection,
        /// agent-loop short-circuit). Surfaced in trace.jsonl so
        /// findings rules can branch on the actual terminal state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<squeezy_llm::StopReason>,
        /// `true` iff the final round was a Qwen3-style "reasoning-only
        /// finish" (`stop_reason=EndTurn` with reasoning text but no
        /// content or tool call).
        #[serde(default)]
        reasoning_only_stop: bool,
        /// Final assistant transcript item (role, text, optional
        /// reasoning snapshot). Schema v3+. Older v2 traces omit
        /// this; consumers must handle `null`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Value>,
        /// Provider response id when surfaced. Schema v3+.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        /// Post-turn context-window estimate (bytes / tokens / items).
        /// Schema v3+. Lets context-window regressions be diffable
        /// across runs without re-deriving from the request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_estimate: Option<Value>,
    },
    TurnFailed {
        error: String,
    },
    TurnCancelled,
    AssistantDelta {
        delta: String,
    },
    /// Schema v3+. Per-token reasoning chunk streaming from the
    /// provider. Captures the live "thinking" surface so regressions
    /// in reasoning emission (silent loss, truncation) are diffable.
    /// Older v2 traces never emitted these; rules that key on
    /// reasoning must tolerate absent variants.
    ReasoningDelta {
        delta: String,
    },
    /// Schema v3+. A complete reasoning segment landing as a
    /// structured snapshot (the TUI uses this to swap the live
    /// "thinking..." buffer into a permanent collapsible transcript
    /// entry). The full `ReasoningSnapshot` is preserved.
    ReasoningSegment {
        display_text: String,
        payload: Value,
    },
    /// Schema v3+. The shell tool's OS sandbox backend degraded to
    /// best-effort mode at least once during the run. Emitted at
    /// most once per session by the agent.
    ShellSandboxDegraded {
        backend: String,
        fallback_count: u64,
    },
    ToolCallQueued {
        call: Value,
    },
    ToolCallStarted {
        call: Value,
        /// `"planner"`, `"model"`, or `"subagent"` — set from
        /// `squeezy_agent::ToolOrigin`. Defaults to `"model"` when
        /// reading older traces written before the field existed.
        #[serde(default = "default_tool_origin")]
        origin: String,
    },
    ToolCallCompleted {
        result: Value,
    },
    Approval {
        request: Value,
        decision: String,
    },
    ContextCompacted {
        report: Value,
    },
    /// Schema v3+: `snapshot` is the full
    /// `squeezy_core::TaskStateSnapshot` serialized as JSON, so rules
    /// can read structured `steps`, `blocker`, `next_action`,
    /// `verification`, `recent_changes`, `replan_reason`. v2 traces
    /// emitted `{"debug": ..., "summary": ..., "status": ...}` — the
    /// `summary` field is still present in v3 traces under the same
    /// key so the existing `ungrounded_citation` Squeezy-help bypass
    /// works on both.
    TaskStateUpdated {
        snapshot: Value,
    },
    SubagentEvent {
        event: Value,
    },
    SlashCommand {
        command: String,
    },
    ActionStep {
        action: Value,
        status: String,
    },
    /// Schema v3+. Typed McpStatusUpdated. Was folded into
    /// `Snapshot{snapshot_kind:"mcp_status"}` in v2.
    McpStatusUpdated {
        servers: Value,
        generated_unix_millis: u128,
    },
    /// Schema v3+. Typed JobUpdated with structured snapshot fields.
    /// Was folded into `Snapshot{snapshot_kind:"job"}` in v2.
    JobUpdated {
        job: Value,
    },
    /// Schema v3+. Typed JobNotification with structured fields.
    /// Was folded into `Snapshot{snapshot_kind:"job_notification"}`
    /// in v2.
    JobNotification {
        job_id: u64,
        job_kind: String,
        status: String,
        title: String,
        summary: String,
        ts_unix_ms: u64,
    },
    /// Schema v3+. Typed CostWarning carrying the broker's structured
    /// CostCapStatus (`spent_usd_micros`, `cap_usd_micros`, `percent`).
    /// Was folded into `Snapshot{snapshot_kind:"cost_warning"}` in v2.
    CostWarning {
        spent_usd_micros: u64,
        cap_usd_micros: u64,
        percent: u8,
    },
    /// Schema v3+. Typed AiReviewerTripped event. Was folded into
    /// `Snapshot{snapshot_kind:"ai_reviewer_tripped"}` in v2.
    AiReviewerTripped {
        reason: String,
    },
    /// Legacy / forward-compat catch-all kept so old v2 traces still
    /// load through `serde`. v3 producers should not emit `Snapshot`
    /// — use the typed variants above. `view` and `diff` accept this
    /// when replaying older runs.
    Snapshot {
        #[serde(rename = "snapshot_kind")]
        snapshot_kind: String,
        payload: Value,
    },
    PerfSample {
        label: String,
        ms: u64,
    },
    Finding {
        rule_id: String,
        severity: String,
        summary: String,
    },
    /// Per-turn cost progress emitted by squeezy every few tool calls.
    CostUpdate {
        tool_count: u64,
        input_tokens: u64,
        micro_usd: u64,
    },
    /// Heartbeat for an in-flight tool call. Lets the live printer
    /// reassure a watcher that a slow tool is still running.
    ToolProgress {
        call_id: String,
        tool_name: String,
        elapsed_ms: u64,
    },
}

/// Trace schema version. Bumped to 3 in 2026-05 to add typed variants
/// for `ReasoningDelta`, `ReasoningSegment`, `ShellSandboxDegraded`,
/// `McpStatusUpdated`, `JobUpdated`, `JobNotification`, `CostWarning`,
/// `AiReviewerTripped`, and to widen `TurnCompleted` with `message` /
/// `response_id` / `context_estimate` and `TaskStateUpdated.snapshot`
/// to the full `TaskStateSnapshot`. v2 traces still deserialize: every
/// new field is `#[serde(default)]` or `Option`, and the legacy
/// `Snapshot { snapshot_kind, payload }` catch-all variant is kept so
/// old `snapshot_kind = "mcp_status"|"job"|"job_notification"|
/// "cost_warning"|"ai_reviewer_tripped"` records keep parsing.
pub const EVAL_TRACE_SCHEMA_VERSION: u32 = 3;

fn default_tool_origin() -> String {
    "model".to_string()
}

/// Append-only JSONL trace writer.
pub struct Capture {
    inner: Mutex<CaptureInner>,
    live: Option<Arc<LivePrinter>>,
}

struct CaptureInner {
    path: PathBuf,
    file: std::fs::File,
    sequence: u64,
}

impl Capture {
    pub fn create(dir: &Path) -> Result<Self, EvalError> {
        Self::create_with_live(dir, None)
    }

    pub fn create_with_live(dir: &Path, live: Option<Arc<LivePrinter>>) -> Result<Self, EvalError> {
        std::fs::create_dir_all(dir)
            .map_err(|err| EvalError::Io(format!("create_dir_all {dir:?}: {err}")))?;
        let path = dir.join("trace.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
        Ok(Self {
            inner: Mutex::new(CaptureInner {
                path,
                file,
                sequence: 0,
            }),
            live,
        })
    }

    pub fn record(&self, turn_id: Option<String>, kind: EvalEventKind) -> Result<(), EvalError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|err| EvalError::Internal(format!("capture mutex poisoned: {err}")))?;
        let sequence = guard.sequence;
        guard.sequence += 1;
        let event = EvalEvent {
            schema_version: EVAL_TRACE_SCHEMA_VERSION,
            ts_unix_ms: now_ms(),
            sequence,
            turn_id: turn_id.clone(),
            kind,
        };
        let line = serde_json::to_string(&event)
            .map_err(|err| EvalError::Internal(format!("serialize trace event: {err}")))?;
        writeln!(guard.file, "{line}")
            .map_err(|err| EvalError::Io(format!("append trace event: {err}")))?;
        // Mirror to the live printer so a watching user sees activity
        // as squeezy runs, not just the final summary line.
        if let Some(printer) = &self.live {
            printer.event(&event.kind, turn_id.as_deref());
        }
        Ok(())
    }

    pub fn path(&self) -> PathBuf {
        self.inner.lock().expect("capture lock").path.clone()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lightweight summary derived from a trace file, for `squeezy-eval replay`.
#[derive(Debug, Default, Clone, Serialize)]
pub struct TraceSummary {
    pub event_count: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub tool_error_count: u64,
    pub wall_clock_ms: u64,
}

pub fn summarize_trace(path: &Path) -> Result<TraceSummary, EvalError> {
    let file =
        std::fs::File::open(path).map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
    let reader = BufReader::new(file);
    let mut summary = TraceSummary::default();
    let mut first_ts: Option<u64> = None;
    let mut last_ts: Option<u64> = None;
    for line in reader.lines() {
        let line = line.map_err(|err| EvalError::Io(format!("read trace line: {err}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: EvalEvent = serde_json::from_str(&line).map_err(|err| {
            EvalError::Internal(format!("parse trace event {}: {err}", summary.event_count))
        })?;
        summary.event_count += 1;
        first_ts.get_or_insert(event.ts_unix_ms);
        last_ts = Some(event.ts_unix_ms);
        match event.kind {
            EvalEventKind::TurnStarted => summary.turn_count += 1,
            EvalEventKind::ToolCallStarted { .. } => summary.tool_call_count += 1,
            EvalEventKind::ToolCallCompleted { result }
                if result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(|s| matches!(s, "Error" | "Cancelled"))
                    .unwrap_or(false) =>
            {
                summary.tool_error_count += 1;
            }
            _ => {}
        }
    }
    summary.wall_clock_ms = match (first_ts, last_ts) {
        (Some(start), Some(end)) if end >= start => end - start,
        _ => 0,
    };
    Ok(summary)
}
