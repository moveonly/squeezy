//! Auto-derived findings.
//!
//! After a scenario finishes, the driver runs a small set of named
//! pattern-matchers over `trace.jsonl`. Each match becomes a
//! [`Finding`] with a stable `rule_id`, written to `findings.jsonl`,
//! embedded back into the trace, and surfaced as a ticket so reviewers
//! see common regressions without authoring them per-scenario.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capture::{EvalEvent, EvalEventKind};
use crate::driver::EvalError;
use crate::scenario::Scenario;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Minor,
    Major,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Minor => "minor",
            Severity::Major => "major",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePointer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_event: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub rule_id: String,
    pub severity: Severity,
    pub summary: String,
    pub category: String,
    #[serde(default)]
    pub evidence: Vec<EvidencePointer>,
}

/// In-memory view of a finished trace.
pub struct TraceContext {
    pub events: Vec<EvalEvent>,
    /// Per-turn-id rollup of `ToolCallStarted` events. Stored as a vector
    /// of `(name, args_sha256, sequence)` triples.
    pub tool_calls_by_turn: BTreeMap<String, Vec<(String, String, u64)>>,
    /// `(sequence, error_text)` for each turn_failed event in order.
    pub turn_failures: Vec<(u64, String)>,
    pub action_steps: Vec<(u64, String, String)>,
    pub approvals: Vec<(u64, String)>,
    pub total_input_tokens: u64,
    pub wall_clock_ms: u64,
    pub turn_count: u64,
    /// Concatenated assistant text for the *final* turn (whichever turn
    /// emitted the last `TurnCompleted` event). Used by
    /// `final_text_contains` expectations.
    pub last_assistant_text: String,
    /// Number of `ToolCallCompleted` events with status Error/Cancelled.
    pub tool_error_count: u64,
}

impl TraceContext {
    pub fn load(path: &Path) -> Result<Self, EvalError> {
        let file = std::fs::File::open(path)
            .map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(|err| EvalError::Io(format!("read trace: {err}")))?;
            if line.trim().is_empty() {
                continue;
            }
            let event: EvalEvent = serde_json::from_str(&line)
                .map_err(|err| EvalError::Internal(format!("parse trace: {err}")))?;
            events.push(event);
        }
        let mut tool_calls_by_turn: BTreeMap<String, Vec<(String, String, u64)>> = BTreeMap::new();
        let mut turn_failures = Vec::new();
        let mut action_steps = Vec::new();
        let mut approvals = Vec::new();
        let mut total_input_tokens = 0u64;
        let mut turn_count = 0u64;
        let mut tool_error_count = 0u64;
        let mut first_ts: Option<u64> = None;
        let mut last_ts: Option<u64> = None;
        let mut per_turn_text: BTreeMap<String, String> = BTreeMap::new();
        let mut last_completed_turn: Option<String> = None;
        for event in &events {
            first_ts.get_or_insert(event.ts_unix_ms);
            last_ts = Some(event.ts_unix_ms);
            match &event.kind {
                EvalEventKind::TurnStarted => turn_count += 1,
                EvalEventKind::ToolCallStarted { call } => {
                    let turn = event.turn_id.clone().unwrap_or_default();
                    let name = call
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = call
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "null".into());
                    let sha = sha256_hex(args_str.as_bytes());
                    tool_calls_by_turn
                        .entry(turn)
                        .or_default()
                        .push((name, sha, event.sequence));
                }
                EvalEventKind::AssistantDelta { delta } => {
                    if let Some(turn) = &event.turn_id {
                        per_turn_text
                            .entry(turn.clone())
                            .or_default()
                            .push_str(delta);
                    }
                }
                EvalEventKind::TurnFailed { error } => {
                    turn_failures.push((event.sequence, error.clone()));
                }
                EvalEventKind::TurnCompleted { cost, .. } => {
                    if let Some(v) = cost.get("input_tokens").and_then(|v| v.as_u64()) {
                        total_input_tokens += v;
                    }
                    if let Some(turn) = &event.turn_id {
                        last_completed_turn = Some(turn.clone());
                    }
                }
                EvalEventKind::ToolCallCompleted { result }
                    if result
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| matches!(s, "Error" | "Cancelled"))
                        .unwrap_or(false) =>
                {
                    tool_error_count += 1;
                }
                EvalEventKind::ActionStep { action, status } => {
                    let kind = action
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    action_steps.push((event.sequence, kind, status.clone()));
                }
                EvalEventKind::Approval { decision, .. } => {
                    approvals.push((event.sequence, decision.clone()));
                }
                _ => {}
            }
        }
        let wall_clock_ms = match (first_ts, last_ts) {
            (Some(s), Some(e)) if e >= s => e - s,
            _ => 0,
        };
        let last_assistant_text = last_completed_turn
            .as_ref()
            .and_then(|t| per_turn_text.get(t).cloned())
            .or_else(|| per_turn_text.values().last().cloned())
            .unwrap_or_default();
        Ok(Self {
            events,
            tool_calls_by_turn,
            turn_failures,
            action_steps,
            approvals,
            total_input_tokens,
            wall_clock_ms,
            turn_count,
            last_assistant_text,
            tool_error_count,
        })
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut out = String::with_capacity(64);
    for b in d {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub trait Rule {
    fn rule_id(&self) -> &'static str;
    fn check(&self, ctx: &TraceContext, scenario: &Scenario) -> Vec<Finding>;
}

// ---------- bundled rules ----------

pub struct DuplicateToolCall;
impl Rule for DuplicateToolCall {
    fn rule_id(&self) -> &'static str {
        "duplicate_tool_call"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for (turn, calls) in &ctx.tool_calls_by_turn {
            let mut by_key: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
            for (name, sha, seq) in calls {
                by_key
                    .entry((name.clone(), sha.clone()))
                    .or_default()
                    .push(*seq);
            }
            for ((name, sha), seqs) in by_key {
                if seqs.len() >= 2 {
                    out.push(Finding {
                        rule_id: "duplicate_tool_call".into(),
                        severity: Severity::Major,
                        category: "perf".into(),
                        summary: format!(
                            "Turn {turn}: {} fired {} times with identical args (sha256 {}…)",
                            name,
                            seqs.len(),
                            &sha[..8.min(sha.len())]
                        ),
                        evidence: seqs
                            .into_iter()
                            .map(|s| EvidencePointer {
                                trace_event: Some(s),
                                frame: None,
                            })
                            .collect(),
                    });
                }
            }
        }
        out
    }
}

pub struct RepeatedTurnFailure;
impl Rule for RepeatedTurnFailure {
    fn rule_id(&self) -> &'static str {
        "repeated_turn_failure"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        let mut prev: Option<&(u64, String)> = None;
        for entry in &ctx.turn_failures {
            if let Some(p) = prev
                && p.1 == entry.1
            {
                out.push(Finding {
                    rule_id: "repeated_turn_failure".into(),
                    severity: Severity::Major,
                    category: "correctness".into(),
                    summary: format!(
                        "Consecutive turns failed with identical error: {}",
                        entry.1.chars().take(160).collect::<String>()
                    ),
                    evidence: vec![
                        EvidencePointer {
                            trace_event: Some(p.0),
                            frame: None,
                        },
                        EvidencePointer {
                            trace_event: Some(entry.0),
                            frame: None,
                        },
                    ],
                });
            }
            prev = Some(entry);
        }
        out
    }
}

pub struct StaleFunctionCallOutput;
impl Rule for StaleFunctionCallOutput {
    fn rule_id(&self) -> &'static str {
        "stale_function_call_output"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for (seq, err) in &ctx.turn_failures {
            if err.contains("No tool call found for function call output") {
                out.push(Finding {
                    rule_id: "stale_function_call_output".into(),
                    severity: Severity::Critical,
                    category: "correctness".into(),
                    summary:
                        "Provider rejected the request: a function_call_output is missing its \
                         matching function_call (likely a compaction or transcript-assembly bug)."
                            .into(),
                    evidence: vec![EvidencePointer {
                        trace_event: Some(*seq),
                        frame: None,
                    }],
                });
            }
        }
        out
    }
}

pub struct HighToolBurst;
impl Rule for HighToolBurst {
    fn rule_id(&self) -> &'static str {
        "high_tool_burst"
    }
    fn check(&self, ctx: &TraceContext, scenario: &Scenario) -> Vec<Finding> {
        let threshold = scenario.expect.max_tools_per_turn.unwrap_or(10);
        let mut out = Vec::new();
        for (turn, calls) in &ctx.tool_calls_by_turn {
            if (calls.len() as u64) > threshold {
                out.push(Finding {
                    rule_id: "high_tool_burst".into(),
                    severity: Severity::Minor,
                    category: "perf".into(),
                    summary: format!(
                        "Turn {turn} fired {} tool calls (> threshold {threshold})",
                        calls.len()
                    ),
                    evidence: calls
                        .iter()
                        .take(8)
                        .map(|(_, _, seq)| EvidencePointer {
                            trace_event: Some(*seq),
                            frame: None,
                        })
                        .collect(),
                });
            }
        }
        out
    }
}

pub struct UnsupportedSlashCommand;
impl Rule for UnsupportedSlashCommand {
    fn rule_id(&self) -> &'static str {
        "unsupported_slash_command"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for (seq, _kind, status) in &ctx.action_steps {
            if status.starts_with("unsupported_slash_command:") {
                out.push(Finding {
                    rule_id: "unsupported_slash_command".into(),
                    severity: Severity::Minor,
                    category: "tooling".into(),
                    summary: format!("Slash command not handled by the driver: {status}"),
                    evidence: vec![EvidencePointer {
                        trace_event: Some(*seq),
                        frame: None,
                    }],
                });
            }
        }
        out
    }
}

pub struct ApprovalUnanswered;
impl Rule for ApprovalUnanswered {
    fn rule_id(&self) -> &'static str {
        "approval_unanswered"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for (seq, decision) in &ctx.approvals {
            if decision.starts_with("denied_no_action") {
                out.push(Finding {
                    rule_id: "approval_unanswered".into(),
                    severity: Severity::Major,
                    category: "tooling".into(),
                    summary: "ApprovalRequested arrived without a matching scenario action; \
                              the driver auto-denied."
                        .into(),
                    evidence: vec![EvidencePointer {
                        trace_event: Some(*seq),
                        frame: None,
                    }],
                });
            }
        }
        out
    }
}

pub struct ExpectationsAsFindings;
impl Rule for ExpectationsAsFindings {
    fn rule_id(&self) -> &'static str {
        "expectations"
    }
    fn check(&self, ctx: &TraceContext, scenario: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        if let Some(max_secs) = scenario.expect.max_wall_clock_seconds
            && ctx.wall_clock_ms / 1000 > max_secs
        {
            out.push(Finding {
                rule_id: "expect_wall_clock".into(),
                severity: Severity::Minor,
                category: "perf".into(),
                summary: format!(
                    "wall clock {}s exceeded max {max_secs}s",
                    ctx.wall_clock_ms / 1000
                ),
                evidence: vec![],
            });
        }
        if let Some(max_tok) = scenario.expect.max_input_tokens
            && ctx.total_input_tokens > max_tok
        {
            out.push(Finding {
                rule_id: "expect_input_tokens".into(),
                severity: Severity::Minor,
                category: "perf".into(),
                summary: format!(
                    "input tokens {} exceeded max {max_tok}",
                    ctx.total_input_tokens
                ),
                evidence: vec![],
            });
        }
        for required in &scenario.expect.final_text_contains {
            if !ctx.last_assistant_text.contains(required) {
                out.push(Finding {
                    rule_id: "expect_final_text_contains".into(),
                    severity: Severity::Minor,
                    category: "correctness".into(),
                    summary: format!("final assistant output missing required text: {required:?}"),
                    evidence: vec![],
                });
            }
        }
        if scenario.expect.no_tool_errors && ctx.tool_error_count > 0 {
            out.push(Finding {
                rule_id: "expect_no_tool_errors".into(),
                severity: Severity::Minor,
                category: "correctness".into(),
                summary: format!("encountered {} tool errors", ctx.tool_error_count),
                evidence: vec![],
            });
        }
        out
    }
}

pub fn default_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(DuplicateToolCall),
        Box::new(RepeatedTurnFailure),
        Box::new(StaleFunctionCallOutput),
        Box::new(HighToolBurst),
        Box::new(UnsupportedSlashCommand),
        Box::new(ApprovalUnanswered),
        Box::new(ExpectationsAsFindings),
    ]
}

pub struct FindingsLog {
    path: PathBuf,
    file: std::fs::File,
}

impl FindingsLog {
    pub fn create(dir: &Path) -> Result<Self, EvalError> {
        std::fs::create_dir_all(dir)
            .map_err(|err| EvalError::Io(format!("create_dir_all {dir:?}: {err}")))?;
        let path = dir.join("findings.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| EvalError::Io(format!("open {path:?}: {err}")))?;
        Ok(Self { path, file })
    }

    pub fn write(&mut self, finding: &Finding) -> Result<(), EvalError> {
        let line = serde_json::to_string(finding)
            .map_err(|err| EvalError::Internal(format!("serialize finding: {err}")))?;
        writeln!(self.file, "{line}")
            .map_err(|err| EvalError::Io(format!("append finding: {err}")))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
#[path = "findings_tests.rs"]
mod tests;
