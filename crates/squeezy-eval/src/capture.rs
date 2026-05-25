use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::driver::EvalError;

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
    },
    TurnFailed {
        error: String,
    },
    TurnCancelled,
    AssistantDelta {
        delta: String,
    },
    ToolCallQueued {
        call: Value,
    },
    ToolCallStarted {
        call: Value,
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
    Snapshot {
        #[serde(rename = "snapshot_kind")]
        snapshot_kind: String,
        payload: Value,
    },
    PerfSample {
        label: String,
        ms: u64,
    },
}

pub const EVAL_TRACE_SCHEMA_VERSION: u32 = 1;

/// Append-only JSONL trace writer.
pub struct Capture {
    inner: Mutex<CaptureInner>,
}

struct CaptureInner {
    path: PathBuf,
    file: std::fs::File,
    sequence: u64,
}

impl Capture {
    pub fn create(dir: &Path) -> Result<Self, EvalError> {
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
            turn_id,
            kind,
        };
        let line = serde_json::to_string(&event)
            .map_err(|err| EvalError::Internal(format!("serialize trace event: {err}")))?;
        writeln!(guard.file, "{line}")
            .map_err(|err| EvalError::Io(format!("append trace event: {err}")))?;
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
