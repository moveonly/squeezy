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
        if let Some(per_turn_cap) = scenario.expect.max_input_tokens_per_turn {
            for event in &ctx.events {
                let crate::capture::EvalEventKind::TurnCompleted { cost, .. } = &event.kind else {
                    continue;
                };
                let Some(input) = cost.get("input_tokens").and_then(|v| v.as_u64()) else {
                    continue;
                };
                if input > per_turn_cap {
                    let turn = event.turn_id.clone().unwrap_or_default();
                    out.push(Finding {
                        rule_id: "expect_input_tokens_per_turn".into(),
                        severity: Severity::Minor,
                        category: "perf".into(),
                        summary: format!(
                            "Turn {turn}: input tokens {input} exceeded per-turn max {per_turn_cap}"
                        ),
                        evidence: vec![EvidencePointer {
                            trace_event: Some(event.sequence),
                            frame: None,
                        }],
                    });
                }
            }
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

/// Detects when multiple graph-family tools fire in the same turn against
/// overlapping query strings — the "duplicate ask, different tool name"
/// pattern. `DuplicateToolCall` only catches byte-exact arg duplicates;
/// this one catches semantic overlap that `args_sha256` would miss
/// (e.g. `decl_search query=X` followed by `symbol_context query=X`).
pub struct RedundantGraphLookup;
impl Rule for RedundantGraphLookup {
    fn rule_id(&self) -> &'static str {
        "redundant_graph_lookup"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        const GRAPH_TOOLS: &[&str] = &[
            "definition_search",
            "symbol_context",
            "decl_search",
            "downstream_flow",
            "upstream_flow",
        ];
        let mut out = Vec::new();
        for (turn, calls) in &ctx.tool_calls_by_turn {
            // Group graph-family calls by extracted query string.
            let mut by_query: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
            for event in &ctx.events {
                if event.turn_id.as_deref() != Some(turn) {
                    continue;
                }
                let crate::capture::EvalEventKind::ToolCallStarted { call } = &event.kind else {
                    continue;
                };
                let name = call
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !GRAPH_TOOLS.contains(&name) {
                    continue;
                }
                let query = call
                    .get("arguments")
                    .and_then(|v| v.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if query.is_empty() {
                    continue;
                }
                by_query
                    .entry(query)
                    .or_default()
                    .push((name.to_string(), event.sequence));
            }
            for (query, hits) in by_query {
                let mut distinct_tools: Vec<&str> = hits.iter().map(|(n, _)| n.as_str()).collect();
                distinct_tools.sort();
                distinct_tools.dedup();
                if distinct_tools.len() >= 2 {
                    out.push(Finding {
                        rule_id: "redundant_graph_lookup".into(),
                        severity: Severity::Minor,
                        category: "perf".into(),
                        summary: format!(
                            "Turn {turn}: graph query {query:?} resolved via {} different tools \
                             ({}) — likely redundant.",
                            distinct_tools.len(),
                            distinct_tools.join(", ")
                        ),
                        evidence: hits
                            .into_iter()
                            .map(|(_, seq)| EvidencePointer {
                                trace_event: Some(seq),
                                frame: None,
                            })
                            .collect(),
                    });
                }
            }
            // Silence unused-variable for explicit shadowing in some compilers.
            let _ = calls;
        }
        out
    }
}

/// Detects the "deep chain trace" pattern: a single turn fires ≥ 4
/// `read_slice` or `grep` calls — the planner is following symbol edges
/// hop-by-hop without a depth cap.
pub struct DeepChainExpansion;
impl Rule for DeepChainExpansion {
    fn rule_id(&self) -> &'static str {
        "deep_chain_expansion"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        const CHAIN_TOOLS: &[&str] = &["read_slice", "grep"];
        let mut out = Vec::new();
        for (turn, calls) in &ctx.tool_calls_by_turn {
            let mut hits: Vec<(String, u64)> = Vec::new();
            for (name, _sha, seq) in calls {
                if CHAIN_TOOLS.contains(&name.as_str()) {
                    hits.push((name.clone(), *seq));
                }
            }
            if hits.len() >= 4 {
                out.push(Finding {
                    rule_id: "deep_chain_expansion".into(),
                    severity: Severity::Minor,
                    category: "perf".into(),
                    summary: format!(
                        "Turn {turn}: planner fired {} chain-trace calls ({}). \
                         Likely missing a depth cap on `from A to B` reasoning.",
                        hits.len(),
                        hits.iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    evidence: hits
                        .into_iter()
                        .take(8)
                        .map(|(_, seq)| EvidencePointer {
                            trace_event: Some(seq),
                            frame: None,
                        })
                        .collect(),
                });
            }
        }
        out
    }
}

/// Detects a heavy repo-wide tool (`repo_map`, `downstream_flow`, or
/// `upstream_flow`) firing alongside a targeted `read_slice` in the same
/// turn. The two together usually mean the planner asked the whole-repo
/// question and the local question — only one is needed.
pub struct HeavyAndTargetedRedundant;
impl Rule for HeavyAndTargetedRedundant {
    fn rule_id(&self) -> &'static str {
        "heavy_and_targeted_redundant"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        const HEAVY: &[&str] = &["repo_map", "downstream_flow", "upstream_flow"];
        const TARGETED: &[&str] = &["read_slice"];
        let mut out = Vec::new();
        for (turn, calls) in &ctx.tool_calls_by_turn {
            let heavy: Vec<&(String, String, u64)> = calls
                .iter()
                .filter(|(n, _, _)| HEAVY.contains(&n.as_str()))
                .collect();
            let targeted: Vec<&(String, String, u64)> = calls
                .iter()
                .filter(|(n, _, _)| TARGETED.contains(&n.as_str()))
                .collect();
            if !heavy.is_empty() && !targeted.is_empty() {
                out.push(Finding {
                    rule_id: "heavy_and_targeted_redundant".into(),
                    severity: Severity::Minor,
                    category: "perf".into(),
                    summary: format!(
                        "Turn {turn}: heavy repo-wide tool ({}) ran alongside targeted \
                         read_slice ({} call(s)) — the planner likely asked the \
                         whole-repo question when a single slice would have answered it.",
                        heavy
                            .iter()
                            .map(|(n, _, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        targeted.len(),
                    ),
                    evidence: heavy
                        .iter()
                        .chain(targeted.iter())
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

/// Detects "trivial answer over-fetch": a turn where the assistant
/// emitted ≤ 20 output tokens but the planner fired ≥ 2 tool calls.
/// Signals that the planner did not detect a one-word / one-sentence
/// answer was possible and over-fetched evidence.
pub struct TrivialAnswerOverFetch;
impl Rule for TrivialAnswerOverFetch {
    fn rule_id(&self) -> &'static str {
        "trivial_answer_over_fetch"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for event in &ctx.events {
            let crate::capture::EvalEventKind::TurnCompleted { cost, .. } = &event.kind else {
                continue;
            };
            let Some(turn) = event.turn_id.as_deref() else {
                continue;
            };
            let output_tokens = cost
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if output_tokens > 20 {
                continue;
            }
            let tool_count = ctx
                .tool_calls_by_turn
                .get(turn)
                .map(|v| v.len())
                .unwrap_or(0);
            if tool_count >= 2 {
                out.push(Finding {
                    rule_id: "trivial_answer_over_fetch".into(),
                    severity: Severity::Minor,
                    category: "perf".into(),
                    summary: format!(
                        "Turn {turn}: planner fired {tool_count} tool calls for an answer of \
                         {output_tokens} output tokens. One direct definition_search would \
                         likely suffice."
                    ),
                    evidence: ctx
                        .tool_calls_by_turn
                        .get(turn)
                        .map(|v| {
                            v.iter()
                                .map(|(_, _, seq)| EvidencePointer {
                                    trace_event: Some(*seq),
                                    frame: None,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
        }
        out
    }
}

/// Detects "ungrounded citation": a turn that fires zero tool calls but
/// whose assistant_text contains a file-path-shaped substring (looks
/// like `something/something.ext` or `path:line`). The agent answered
/// with a specific citation without proving it via retrieval — a trust
/// hazard even when the path happens to be right.
pub struct UngroundedCitation;
impl Rule for UngroundedCitation {
    fn rule_id(&self) -> &'static str {
        "ungrounded_citation"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        use std::collections::BTreeMap;
        // Group assistant_delta by turn.
        let mut text_per_turn: BTreeMap<String, String> = BTreeMap::new();
        let mut completed_turns: BTreeMap<String, u64> = BTreeMap::new();
        for event in &ctx.events {
            match &event.kind {
                crate::capture::EvalEventKind::AssistantDelta { delta } => {
                    if let Some(turn) = &event.turn_id {
                        text_per_turn
                            .entry(turn.clone())
                            .or_default()
                            .push_str(delta);
                    }
                }
                crate::capture::EvalEventKind::TurnCompleted { .. } => {
                    if let Some(turn) = &event.turn_id {
                        completed_turns.insert(turn.clone(), event.sequence);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for (turn, seq) in completed_turns {
            let tool_count = ctx
                .tool_calls_by_turn
                .get(&turn)
                .map(|v| v.len())
                .unwrap_or(0);
            if tool_count != 0 {
                continue;
            }
            let text = text_per_turn.get(&turn).cloned().unwrap_or_default();
            if looks_like_path_citation(&text) {
                out.push(Finding {
                    rule_id: "ungrounded_citation".into(),
                    severity: Severity::Major,
                    category: "correctness".into(),
                    summary: format!(
                        "Turn {turn}: fired 0 tool calls but emitted a file-path citation. \
                         The agent did not retrieve evidence before answering."
                    ),
                    evidence: vec![EvidencePointer {
                        trace_event: Some(seq),
                        frame: None,
                    }],
                });
            }
        }
        out
    }
}

/// Heuristic — does the text contain a substring that looks like a
/// `path/with/slashes.ext` file citation or `path:line` line citation?
fn looks_like_path_citation(text: &str) -> bool {
    // Quick filter: any `/` followed by something with a `.` (extension)
    // or any `:line` style.
    for token in
        text.split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | ',' | ';'))
    {
        let token = token.trim_matches('.');
        if token.contains('/')
            && let Some(dot) = token.rfind('.')
            && dot > token.rfind('/').unwrap_or(0)
            && token.len() - dot >= 2
            && token.len() - dot <= 6
            && token[dot + 1..].chars().all(|c| c.is_ascii_alphanumeric())
        {
            return true;
        }
    }
    false
}

/// Detects "incomplete confidence labeling": the scenario prompt asked
/// the model to cite each piece of evidence with a confidence label
/// (`exact_syntax` / `import_resolved` / `candidate_set` / `external` /
/// `unknown` / `label_missing`), but the assistant answer contains
/// claims that look like evidence (bulleted items or numbered lists)
/// without any label tag. Catches the partial-compliance pattern that
/// shipped in PR #98's "fix" but still misses some claims.
pub struct IncompleteConfidenceLabels;
impl Rule for IncompleteConfidenceLabels {
    fn rule_id(&self) -> &'static str {
        "incomplete_confidence_labels"
    }
    fn check(&self, ctx: &TraceContext, scenario: &Scenario) -> Vec<Finding> {
        // Only fire if at least one prompt mentioned confidence labels.
        let prompts_mention = scenario
            .steps
            .iter()
            .filter_map(|s| match s {
                crate::scenario::Step::Prompt { text, .. } => Some(text),
                _ => None,
            })
            .any(|t| t.contains("confidence label") || t.contains("exact_syntax"));
        if !prompts_mention {
            return vec![];
        }

        let label_tags = [
            "exact_syntax",
            "import_resolved",
            "candidate_set",
            "external",
            "unknown",
            "label_missing",
        ];

        let mut text_per_turn: std::collections::BTreeMap<String, String> = Default::default();
        let mut completed_turns: std::collections::BTreeMap<String, u64> = Default::default();
        for event in &ctx.events {
            match &event.kind {
                crate::capture::EvalEventKind::AssistantDelta { delta } => {
                    if let Some(turn) = &event.turn_id {
                        text_per_turn
                            .entry(turn.clone())
                            .or_default()
                            .push_str(delta);
                    }
                }
                crate::capture::EvalEventKind::TurnCompleted { .. } => {
                    if let Some(turn) = &event.turn_id {
                        completed_turns.insert(turn.clone(), event.sequence);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for (turn, seq) in completed_turns {
            let text = text_per_turn.get(&turn).cloned().unwrap_or_default();
            // Count claim-shaped lines: bulleted (- ...), numbered (1. ...),
            // or backtick-quoted citations on their own line.
            let claim_lines = text
                .lines()
                .filter(|l| {
                    let l = l.trim_start();
                    l.starts_with("- ")
                        || l.starts_with("* ")
                        || (l.len() >= 3
                            && l.starts_with(|c: char| c.is_ascii_digit())
                            && l.contains(". "))
                })
                .count();
            if claim_lines < 2 {
                continue;
            }
            let label_hits: usize = label_tags.iter().map(|tag| text.matches(tag).count()).sum();
            // Allow a small slack — only flag when the gap is meaningful.
            if claim_lines >= 2 && label_hits + 1 < claim_lines {
                out.push(Finding {
                    rule_id: "incomplete_confidence_labels".into(),
                    severity: Severity::Major,
                    category: "correctness".into(),
                    summary: format!(
                        "Turn {turn}: prompt asked for confidence labels on every claim, but \
                         only {label_hits} label tag(s) found for {claim_lines} claim-shaped \
                         lines. Partial compliance is a provenance hazard."
                    ),
                    evidence: vec![EvidencePointer {
                        trace_event: Some(seq),
                        frame: None,
                    }],
                });
            }
        }
        out
    }
}

/// Detects "exact_syntax without source evidence": the assistant tagged
/// a claim `[exact_syntax]` (the strongest confidence label) but the
/// turn never fired `read_slice` — the only tool that returns
/// literal source bytes. Without reading the source, the best the
/// model can honestly claim is `import_resolved` or `candidate_set`.
pub struct ExactSyntaxWithoutSource;
impl Rule for ExactSyntaxWithoutSource {
    fn rule_id(&self) -> &'static str {
        "exact_syntax_without_source"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut text_per_turn: std::collections::BTreeMap<String, String> = Default::default();
        let mut completed_turns: std::collections::BTreeMap<String, u64> = Default::default();
        for event in &ctx.events {
            match &event.kind {
                crate::capture::EvalEventKind::AssistantDelta { delta } => {
                    if let Some(turn) = &event.turn_id {
                        text_per_turn
                            .entry(turn.clone())
                            .or_default()
                            .push_str(delta);
                    }
                }
                crate::capture::EvalEventKind::TurnCompleted { .. } => {
                    if let Some(turn) = &event.turn_id {
                        completed_turns.insert(turn.clone(), event.sequence);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for (turn, seq) in completed_turns {
            let text = text_per_turn.get(&turn).cloned().unwrap_or_default();
            if !text.contains("exact_syntax") {
                continue;
            }
            let fired_read_slice = ctx
                .tool_calls_by_turn
                .get(&turn)
                .map(|v| v.iter().any(|(n, _, _)| n == "read_slice"))
                .unwrap_or(false);
            if fired_read_slice {
                continue;
            }
            out.push(Finding {
                rule_id: "exact_syntax_without_source".into(),
                severity: Severity::Major,
                category: "correctness".into(),
                summary: format!(
                    "Turn {turn}: assistant tagged a claim `[exact_syntax]` but the turn never \
                     called `read_slice`. The strongest label honestly available without source \
                     bytes is `import_resolved` or `candidate_set`."
                ),
                evidence: vec![EvidencePointer {
                    trace_event: Some(seq),
                    frame: None,
                }],
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
        Box::new(RedundantGraphLookup),
        Box::new(DeepChainExpansion),
        Box::new(HeavyAndTargetedRedundant),
        Box::new(TrivialAnswerOverFetch),
        Box::new(UngroundedCitation),
        Box::new(IncompleteConfidenceLabels),
        Box::new(ExactSyntaxWithoutSource),
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
