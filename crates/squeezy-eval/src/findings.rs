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
    /// Per-turn-id assistant text accumulated from
    /// `EvalEventKind::AssistantDelta`. Populated for every turn that
    /// emitted any text; absent for tool-only or empty turns.
    pub assistant_text_by_turn: BTreeMap<String, String>,
    /// Per-turn finish state captured at `EvalEventKind::TurnCompleted`.
    /// `(sequence, stop_reason, reasoning_only_stop)` per completed
    /// turn, in order of arrival.
    pub turn_finish_states: BTreeMap<String, (u64, Option<squeezy_llm::StopReason>, bool)>,
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
        let mut turn_finish_states: BTreeMap<String, (u64, Option<squeezy_llm::StopReason>, bool)> =
            BTreeMap::new();
        for event in &events {
            first_ts.get_or_insert(event.ts_unix_ms);
            last_ts = Some(event.ts_unix_ms);
            match &event.kind {
                EvalEventKind::TurnStarted => turn_count += 1,
                EvalEventKind::ToolCallStarted { call, .. } => {
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
                EvalEventKind::TurnCompleted {
                    cost,
                    stop_reason,
                    reasoning_only_stop,
                    ..
                } => {
                    if let Some(v) = cost.get("input_tokens").and_then(|v| v.as_u64()) {
                        total_input_tokens += v;
                    }
                    if let Some(turn) = &event.turn_id {
                        last_completed_turn = Some(turn.clone());
                        turn_finish_states.insert(
                            turn.clone(),
                            (event.sequence, stop_reason.clone(), *reasoning_only_stop),
                        );
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
            assistant_text_by_turn: per_turn_text,
            turn_finish_states,
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
                let crate::capture::EvalEventKind::ToolCallStarted { call, .. } = &event.kind
                else {
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
        use std::collections::{BTreeMap, BTreeSet};
        // Group assistant_delta by turn.
        let mut text_per_turn: BTreeMap<String, String> = BTreeMap::new();
        let mut completed_turns: BTreeMap<String, u64> = BTreeMap::new();
        // Turns the agent labelled as Squeezy help — both the curated path
        // and the doc-help subagent path go through the same task-state
        // summary, so checking it covers both.
        let mut help_turns: BTreeSet<String> = BTreeSet::new();
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
                crate::capture::EvalEventKind::TaskStateUpdated { snapshot } => {
                    if let Some(turn) = &event.turn_id
                        && task_snapshot_marks_help(snapshot)
                    {
                        help_turns.insert(turn.clone());
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
            // Squeezy help turns answer from the bundled doc corpus that
            // ships compiled into the binary. They legitimately make zero
            // tool calls AND cite real bundled doc paths — both curated
            // answers (via `HelpCitation`) and doc-help subagent answers
            // (free-form inline citations from the inlined corpus).
            if help_turns.contains(&turn) {
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

/// True when an emitted `task_state_updated` snapshot is for a Squeezy
/// help turn. Both the curated path and the doc-help subagent path use
/// the literal terminal summary `"Squeezy help"` (see
/// `complete_squeezy_help_turn`); the driver mirrors that into a
/// top-level `summary` field on the captured snapshot Value.
fn task_snapshot_marks_help(snapshot: &serde_json::Value) -> bool {
    snapshot
        .get("summary")
        .and_then(|value| value.as_str())
        .is_some_and(|summary| summary == "Squeezy help")
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

/// Detects the canonical Qwen3 "I'll do X then stop" failure mode: a
/// turn ends with `stop_reason=EndTurn`, no tool calls, and the
/// assistant text contains an intent phrase that promised tool work
/// (`"let me X"`, `"i'll Y"`, `"i will Z"`, ...). The model committed
/// verbally to follow-up tool use, then never emitted the call.
///
/// Why this rule exists: the existing `finish_reason="stop"` handler in
/// `compatible.rs` only fires when *no* visible output surfaced
/// (`saw_visible_output == false`). The bug we're after is the opposite
/// — text emitted, intent stated, tool call missing — and that path
/// looks like a perfectly normal turn-ending stop on the wire. The
/// finding's presence in a run is the smoking gun that a stronger
/// retry-on-intent-without-action fix is worth shipping.
pub struct StopWithIntentTextNoToolCall;
impl Rule for StopWithIntentTextNoToolCall {
    fn rule_id(&self) -> &'static str {
        "stop_with_intent_text_no_tool_call"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        const INTENT_PATTERNS: &[&str] = &[
            "let me ",
            "let's ",
            "i'll ",
            "ill ",
            "i will ",
            "now i'll ",
            "now i ",
            "next i ",
            "next, i ",
            "i need to ",
            "i can ",
            "first, i ",
            "going to ",
        ];
        const ACTION_PATTERNS: &[&str] = &[
            "scan",
            "search",
            "explore",
            "find",
            "read",
            "look",
            "check",
            "inspect",
            "grep",
            "map",
            "list",
            "open",
            "fetch",
            "load",
            "fix",
            "edit",
            "modify",
            "write",
            "create",
            "rename",
            "investigate",
            "trace",
            "follow",
        ];
        let mut out = Vec::new();
        for (turn, (seq, stop_reason, _)) in &ctx.turn_finish_states {
            // Only check turns that actually finished with EndTurn (the
            // normalized "model voluntarily released the turn" signal).
            // Other stop reasons (MaxTokens, ToolUse, Refusal, …) are
            // out of scope — different failure modes with different
            // fixes.
            if *stop_reason != Some(squeezy_llm::StopReason::EndTurn) {
                continue;
            }
            // Must have zero tool calls in this turn.
            let tool_count = ctx
                .tool_calls_by_turn
                .get(turn)
                .map(|v| v.len())
                .unwrap_or(0);
            if tool_count > 0 {
                continue;
            }
            let text = ctx
                .assistant_text_by_turn
                .get(turn)
                .cloned()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if text.trim().is_empty() {
                continue;
            }
            // Look for `let me X` / `I'll X` / ... where X is an action verb.
            let intent_match = INTENT_PATTERNS.iter().any(|intent| {
                if let Some(idx) = text.find(intent) {
                    let tail = &text[idx + intent.len()..];
                    let next_30 = &tail[..tail.len().min(40)];
                    ACTION_PATTERNS
                        .iter()
                        .any(|action| next_30.contains(action))
                } else {
                    false
                }
            });
            if !intent_match {
                continue;
            }
            out.push(Finding {
                rule_id: "stop_with_intent_text_no_tool_call".into(),
                severity: Severity::Major,
                category: "correctness".into(),
                summary: format!(
                    "Turn {turn}: model finished with `stop_reason=EndTurn`, emitted intent text \
                     (\"let me / i'll <action>\") but fired zero tool calls. Likely the canonical \
                     Qwen3 chatty-preamble-then-stop pattern."
                ),
                evidence: vec![EvidencePointer {
                    trace_event: Some(*seq),
                    frame: None,
                }],
            });
        }
        out
    }
}

/// Detects when a completed turn's `stop_reason` matches an entry in
/// `expect.finish_reason_not`. Used to assert "no turn ended with
/// `length` truncation" or "no turn ended with `stop` and no tool call
/// (via the synthetic `stop_no_action` sentinel)". The sentinel is
/// resolved by checking `tool_count == 0` AND
/// `stop_reason == EndTurn`, so scenarios can write
/// `expect.finish_reason_not = ["stop_no_action"]` without having to
/// know the underlying mechanism. String literals in
/// `finish_reason_not` are matched against the lowercased
/// `StopReason` debug form (`"end_turn"`, `"max_tokens"`, `"tool_use"`,
/// `"context_window_exceeded"`, `"stop_sequence"`, `"refusal"`).
pub struct ExpectFinishReasonNot;
impl Rule for ExpectFinishReasonNot {
    fn rule_id(&self) -> &'static str {
        "expect_finish_reason"
    }
    fn check(&self, ctx: &TraceContext, scenario: &Scenario) -> Vec<Finding> {
        if scenario.expect.finish_reason_not.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (turn, (seq, stop_reason, _)) in &ctx.turn_finish_states {
            let actual_label = stop_reason_label(stop_reason.as_ref());
            let tool_count = ctx
                .tool_calls_by_turn
                .get(turn)
                .map(|v| v.len())
                .unwrap_or(0);
            let assistant_text = ctx
                .assistant_text_by_turn
                .get(turn)
                .map(String::as_str)
                .unwrap_or("");
            for forbidden in &scenario.expect.finish_reason_not {
                let hit = match forbidden.as_str() {
                    // `stop_no_action` means the model produced nothing
                    // actionable — no tool call AND no assistant text.
                    // A normal plan-mode finish (text only, no tool
                    // calls) is a legitimate "stop" and should NOT
                    // match the sentinel. The trailing
                    // `assistant_text.trim().is_empty()` check is what
                    // separates "model gave a final answer" from "model
                    // ate its budget on reasoning".
                    "stop_no_action" => {
                        *stop_reason == Some(squeezy_llm::StopReason::EndTurn)
                            && tool_count == 0
                            && assistant_text.trim().is_empty()
                    }
                    literal => actual_label.as_deref() == Some(literal),
                };
                if hit {
                    out.push(Finding {
                        rule_id: "expect_finish_reason".into(),
                        severity: Severity::Major,
                        category: "correctness".into(),
                        summary: format!(
                            "Turn {turn}: stop_reason={actual_label:?} matched forbidden \
                             `expect.finish_reason_not = {forbidden:?}` \
                             (tool_count={tool_count}, assistant_text_chars={})",
                            assistant_text.trim().chars().count()
                        ),
                        evidence: vec![EvidencePointer {
                            trace_event: Some(*seq),
                            frame: None,
                        }],
                    });
                }
            }
        }
        out
    }
}

/// Lowercase snake_case label for a `StopReason`, matching the strings
/// users write in `expect.finish_reason_not`. The `Other(s)` variant
/// surfaces the inner string verbatim so provider-specific stop
/// reasons stay reachable.
fn stop_reason_label(reason: Option<&squeezy_llm::StopReason>) -> Option<String> {
    reason.map(|r| match r {
        squeezy_llm::StopReason::EndTurn => "end_turn".to_string(),
        squeezy_llm::StopReason::ToolUse => "tool_use".to_string(),
        squeezy_llm::StopReason::MaxTokens => "max_tokens".to_string(),
        squeezy_llm::StopReason::ContextWindowExceeded => "context_window_exceeded".to_string(),
        squeezy_llm::StopReason::StopSequence => "stop_sequence".to_string(),
        squeezy_llm::StopReason::Refusal => "refusal".to_string(),
        squeezy_llm::StopReason::Other(other) => other.clone(),
    })
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
        Box::new(StopWithIntentTextNoToolCall),
        Box::new(ExpectFinishReasonNot),
        Box::new(ExpectationsAsFindings),
        // Phase 4 additions
        Box::new(CrossTurnDuplicateToolCall),
        Box::new(PostCompactionFailure),
        Box::new(EmptyAssistantText),
        Box::new(SubagentFailure),
        Box::new(McpServerFailure),
        Box::new(CostWarningRaised),
        Box::new(AiReviewerTrippedRule),
        Box::new(LengthTruncation),
        Box::new(SandboxDegraded),
        Box::new(SlowFirstToken),
        Box::new(CompactionLoop),
        Box::new(DeferredFindingFired),
        Box::new(ExpectDroppedToolCalls),
        // Phase 5 additions (TUI-coverage rules)
        Box::new(TuiOverlayUnhandled),
        Box::new(TuiUserInputAutoCancelled),
        // Phase 7 additions
        Box::new(PlatformMismatch),
    ]
}

/// Phase 7: scenario opted into a specific OS platform; the host
/// doesn't match. Soft-assertion: produces a finding but does not
/// abort the run.
pub struct PlatformMismatch;
impl Rule for PlatformMismatch {
    fn rule_id(&self) -> &'static str {
        "platform_mismatch"
    }
    fn check(&self, _ctx: &TraceContext, scenario: &Scenario) -> Vec<Finding> {
        let Some(expected) = scenario.platform.as_deref() else {
            return Vec::new();
        };
        let actual = match std::env::consts::OS {
            "linux" => "linux",
            "macos" => "macos",
            "windows" => "windows",
            other => other,
        };
        if !expected.eq_ignore_ascii_case(actual) {
            vec![Finding {
                rule_id: "platform_mismatch".into(),
                severity: Severity::Minor,
                category: "infra".into(),
                summary: format!(
                    "Scenario pinned `platform = \"{expected}\"` but the host is `{actual}` — \
                     OS-specific behavior (sandbox backends, shell semantics) won't match the \
                     scenario's intent."
                ),
                evidence: vec![],
            }]
        } else {
            Vec::new()
        }
    }
}

/// Phase 5: any overlay-triggering event (approval / MCP elicitation /
/// user-input) was auto-cancelled because the scenario didn't queue a
/// matching action. Complements the existing `approval_unanswered`
/// rule by extending the same signal to elicitation and user-input
/// surfaces.
pub struct TuiOverlayUnhandled;
impl Rule for TuiOverlayUnhandled {
    fn rule_id(&self) -> &'static str {
        "tui_overlay_unhandled"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for (seq, kind, status) in &ctx.action_steps {
            let is_overlay = matches!(kind.as_str(), "mcp_elicitation" | "request_user_input");
            if is_overlay && status == "auto_cancelled" {
                out.push(Finding {
                    rule_id: "tui_overlay_unhandled".into(),
                    severity: Severity::Major,
                    category: "ux".into(),
                    summary: format!(
                        "Overlay-triggering event `{kind}` was auto-cancelled — \
                         no scenario `RespondElicitation` / `RespondUserInput` matched."
                    ),
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

/// Phase 5: a typed RequestUserInput auto-cancelled. Split from
/// `tui_overlay_unhandled` so triage can route the two surfaces
/// separately when needed.
pub struct TuiUserInputAutoCancelled;
impl Rule for TuiUserInputAutoCancelled {
    fn rule_id(&self) -> &'static str {
        "tui_user_input_auto_cancelled"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for (seq, kind, status) in &ctx.action_steps {
            if kind == "request_user_input" && status == "auto_cancelled" {
                out.push(Finding {
                    rule_id: "tui_user_input_auto_cancelled".into(),
                    severity: Severity::Minor,
                    category: "ux".into(),
                    summary: "RequestUserInput overlay opened but the scenario auto-cancelled the prompt.".into(),
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

// ---------- Phase 4 rules ----------

/// Same `(name, args_sha256)` fires across ≥3 distinct turns. The
/// turn-local `DuplicateToolCall` rule only catches same-turn
/// repeats; this catches the "agent re-runs the identical grep
/// across turns 1, 2, 3" pattern that wastes provider input tokens.
pub struct CrossTurnDuplicateToolCall;
impl Rule for CrossTurnDuplicateToolCall {
    fn rule_id(&self) -> &'static str {
        "cross_turn_duplicate_tool_call"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut by_key: BTreeMap<(String, String), Vec<(String, u64)>> = BTreeMap::new();
        for (turn, calls) in &ctx.tool_calls_by_turn {
            for (name, sha, seq) in calls {
                by_key
                    .entry((name.clone(), sha.clone()))
                    .or_default()
                    .push((turn.clone(), *seq));
            }
        }
        let mut out = Vec::new();
        for ((name, sha), entries) in by_key {
            let distinct_turns = entries
                .iter()
                .map(|(t, _)| t.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            if distinct_turns >= 3 {
                out.push(Finding {
                    rule_id: "cross_turn_duplicate_tool_call".into(),
                    severity: Severity::Major,
                    category: "perf".into(),
                    summary: format!(
                        "{} fired with identical args (sha256 {}…) across {} distinct turns",
                        name,
                        &sha[..8.min(sha.len())],
                        distinct_turns
                    ),
                    evidence: entries
                        .into_iter()
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

/// The turn immediately after `ContextCompacted` ends in `TurnFailed`
/// OR emits empty assistant text with zero tool calls. Catches the
/// "compact left the conversation broken" regression.
pub struct PostCompactionFailure;
impl Rule for PostCompactionFailure {
    fn rule_id(&self) -> &'static str {
        "post_compaction_failure"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        let mut compaction_seen = false;
        let mut last_compaction_seq: Option<u64> = None;
        for event in &ctx.events {
            match &event.kind {
                EvalEventKind::ContextCompacted { .. } => {
                    compaction_seen = true;
                    last_compaction_seq = Some(event.sequence);
                }
                EvalEventKind::TurnFailed { error } if compaction_seen => {
                    out.push(Finding {
                        rule_id: "post_compaction_failure".into(),
                        severity: Severity::Critical,
                        category: "correctness".into(),
                        summary: format!(
                            "Turn after /compact failed: {}",
                            error.chars().take(160).collect::<String>()
                        ),
                        evidence: vec![
                            EvidencePointer {
                                trace_event: last_compaction_seq,
                                frame: None,
                            },
                            EvidencePointer {
                                trace_event: Some(event.sequence),
                                frame: None,
                            },
                        ],
                    });
                    compaction_seen = false;
                }
                EvalEventKind::TurnCompleted { .. } if compaction_seen => {
                    if let Some(turn) = &event.turn_id {
                        let text = ctx
                            .assistant_text_by_turn
                            .get(turn)
                            .cloned()
                            .unwrap_or_default();
                        let tool_count = ctx
                            .tool_calls_by_turn
                            .get(turn)
                            .map(|v| v.len())
                            .unwrap_or(0);
                        if text.trim().is_empty() && tool_count == 0 {
                            out.push(Finding {
                                rule_id: "post_compaction_failure".into(),
                                severity: Severity::Critical,
                                category: "correctness".into(),
                                summary: format!(
                                    "Turn {turn} after /compact emitted no assistant text and no tool calls"
                                ),
                                evidence: vec![
                                    EvidencePointer {
                                        trace_event: last_compaction_seq,
                                        frame: None,
                                    },
                                    EvidencePointer {
                                        trace_event: Some(event.sequence),
                                        frame: None,
                                    },
                                ],
                            });
                        }
                    }
                    compaction_seen = false;
                }
                _ => {}
            }
        }
        out
    }
}

/// A turn finished cleanly but produced no assistant text and no
/// tool calls. The user sees an empty answer.
pub struct EmptyAssistantText;
impl Rule for EmptyAssistantText {
    fn rule_id(&self) -> &'static str {
        "empty_assistant_text"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for event in &ctx.events {
            if let EvalEventKind::TurnCompleted { .. } = &event.kind
                && let Some(turn) = &event.turn_id
            {
                let text = ctx
                    .assistant_text_by_turn
                    .get(turn)
                    .cloned()
                    .unwrap_or_default();
                let tool_count = ctx
                    .tool_calls_by_turn
                    .get(turn)
                    .map(|v| v.len())
                    .unwrap_or(0);
                if text.trim().is_empty() && tool_count == 0 {
                    out.push(Finding {
                        rule_id: "empty_assistant_text".into(),
                        severity: Severity::Major,
                        category: "correctness".into(),
                        summary: format!(
                            "Turn {turn} completed with empty assistant text and zero tool calls"
                        ),
                        evidence: vec![EvidencePointer {
                            trace_event: Some(event.sequence),
                            frame: None,
                        }],
                    });
                }
            }
        }
        out
    }
}

/// Any `SubagentEvent` with `kind: failed` or `kind: rejected`.
pub struct SubagentFailure;
impl Rule for SubagentFailure {
    fn rule_id(&self) -> &'static str {
        "subagent_failure"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for event in &ctx.events {
            if let EvalEventKind::SubagentEvent { event: sub } = &event.kind {
                let kind = sub.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                if matches!(kind, "failed" | "rejected") {
                    let agent = sub.get("agent").and_then(|v| v.as_str()).unwrap_or("?");
                    let detail = match kind {
                        "failed" => sub
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error")
                            .to_string(),
                        "rejected" => format!(
                            "reason={}",
                            sub.get("reason")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                        ),
                        _ => String::new(),
                    };
                    out.push(Finding {
                        rule_id: "subagent_failure".into(),
                        severity: Severity::Major,
                        category: "correctness".into(),
                        summary: format!(
                            "Subagent `{agent}` {kind}: {}",
                            detail.chars().take(200).collect::<String>()
                        ),
                        evidence: vec![EvidencePointer {
                            trace_event: Some(event.sequence),
                            frame: None,
                        }],
                    });
                }
            }
        }
        out
    }
}

/// Any `McpStatusUpdated` (typed or legacy Snapshot) reports a server
/// in `Failed`. Catches MCP regressions silently degrading tool
/// availability.
pub struct McpServerFailure;
impl Rule for McpServerFailure {
    fn rule_id(&self) -> &'static str {
        "mcp_server_failure"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        let mut reported: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for event in &ctx.events {
            let servers = match &event.kind {
                EvalEventKind::McpStatusUpdated { servers, .. } => servers.clone(),
                EvalEventKind::Snapshot {
                    snapshot_kind,
                    payload,
                } if snapshot_kind == "mcp_status" => {
                    // v2 stored only `{"debug": "..."}`; nothing
                    // structured to scan. Skip silently — the same
                    // regression in a v3 run lights up.
                    let _ = payload;
                    continue;
                }
                _ => continue,
            };
            let Some(obj) = servers.as_object() else {
                continue;
            };
            for (server, status) in obj {
                let failed = status.is_object() && status.get("Failed").is_some()
                    || status.as_str() == Some("Failed");
                if failed && reported.insert(server.clone()) {
                    let err = status
                        .get("Failed")
                        .and_then(|v| v.get("error"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error message)");
                    out.push(Finding {
                        rule_id: "mcp_server_failure".into(),
                        severity: Severity::Major,
                        category: "tooling".into(),
                        summary: format!(
                            "MCP server `{server}` reported Failed: {}",
                            err.chars().take(160).collect::<String>()
                        ),
                        evidence: vec![EvidencePointer {
                            trace_event: Some(event.sequence),
                            frame: None,
                        }],
                    });
                }
            }
        }
        out
    }
}

/// Any `CostWarning` typed event fired (the broker crossed the
/// configured cap percentage).
pub struct CostWarningRaised;
impl Rule for CostWarningRaised {
    fn rule_id(&self) -> &'static str {
        "cost_warning_raised"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for event in &ctx.events {
            if let EvalEventKind::CostWarning {
                spent_usd_micros,
                cap_usd_micros,
                percent,
            } = &event.kind
            {
                out.push(Finding {
                    rule_id: "cost_warning_raised".into(),
                    severity: Severity::Minor,
                    category: "cost".into(),
                    summary: format!(
                        "Session crossed cost cap warning threshold: spent=${:.4} cap=${:.4} ({}%)",
                        *spent_usd_micros as f64 / 1_000_000.0,
                        *cap_usd_micros as f64 / 1_000_000.0,
                        percent
                    ),
                    evidence: vec![EvidencePointer {
                        trace_event: Some(event.sequence),
                        frame: None,
                    }],
                });
            }
        }
        out
    }
}

/// Any `AiReviewerTripped` event recorded. The reviewer's auto-veto
/// path is rare; a hit usually signals a regression worth surfacing.
pub struct AiReviewerTrippedRule;
impl Rule for AiReviewerTrippedRule {
    fn rule_id(&self) -> &'static str {
        "ai_reviewer_tripped"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for event in &ctx.events {
            if let EvalEventKind::AiReviewerTripped { reason } = &event.kind {
                out.push(Finding {
                    rule_id: "ai_reviewer_tripped".into(),
                    severity: Severity::Major,
                    category: "correctness".into(),
                    summary: format!(
                        "AI reviewer tripped: {}",
                        reason.chars().take(200).collect::<String>()
                    ),
                    evidence: vec![EvidencePointer {
                        trace_event: Some(event.sequence),
                        frame: None,
                    }],
                });
            }
        }
        out
    }
}

/// Any turn finished with `StopReason::MaxTokens` — provider hit the
/// output-token cap mid-response. Always-on; users who explicitly
/// opted into length truncation via `expect.finish_reason_not =
/// ["max_tokens"]` get a redundant hit, but that's intentional —
/// the rule surfaces the same regression from two angles.
pub struct LengthTruncation;
impl Rule for LengthTruncation {
    fn rule_id(&self) -> &'static str {
        "length_truncation"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for (turn, (seq, stop, _)) in &ctx.turn_finish_states {
            if matches!(stop, Some(squeezy_llm::StopReason::MaxTokens)) {
                out.push(Finding {
                    rule_id: "length_truncation".into(),
                    severity: Severity::Major,
                    category: "correctness".into(),
                    summary: format!("Turn {turn} ended with stop_reason=MaxTokens — provider truncated the response"),
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

/// Any `ShellSandboxDegraded` event recorded by the agent. Each
/// fallback indicates the OS sandbox failed for at least one shell
/// invocation and the run silently downgraded to best-effort
/// isolation.
pub struct SandboxDegraded;
impl Rule for SandboxDegraded {
    fn rule_id(&self) -> &'static str {
        "sandbox_degraded"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut out = Vec::new();
        for event in &ctx.events {
            if let EvalEventKind::ShellSandboxDegraded {
                backend,
                fallback_count,
            } = &event.kind
            {
                out.push(Finding {
                    rule_id: "sandbox_degraded".into(),
                    severity: Severity::Minor,
                    category: "tooling".into(),
                    summary: format!(
                        "Shell sandbox `{backend}` fell back to best-effort isolation ({fallback_count} time(s))"
                    ),
                    evidence: vec![EvidencePointer {
                        trace_event: Some(event.sequence),
                        frame: None,
                    }],
                });
            }
        }
        out
    }
}

/// The first `AssistantDelta` (or any tool/turn-ending event) arrived
/// more than 5 seconds after the matching `TurnStarted`. Catches
/// providers that are silently slow on the first byte; pair with
/// Phase 7's heartbeat-aware timeout for full coverage.
pub struct SlowFirstToken;
impl Rule for SlowFirstToken {
    fn rule_id(&self) -> &'static str {
        "slow_first_token"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        const THRESHOLD_MS: u64 = 5_000;
        let mut out = Vec::new();
        let mut turn_start_ts: BTreeMap<String, (u64, u64)> = BTreeMap::new(); // turn -> (ts, seq)
        for event in &ctx.events {
            match &event.kind {
                EvalEventKind::TurnStarted => {
                    if let Some(turn) = &event.turn_id {
                        turn_start_ts.insert(turn.clone(), (event.ts_unix_ms, event.sequence));
                    }
                }
                EvalEventKind::AssistantDelta { .. } | EvalEventKind::ToolCallStarted { .. } => {
                    if let Some(turn) = &event.turn_id
                        && let Some((start_ts, start_seq)) = turn_start_ts.remove(turn)
                    {
                        let elapsed = event.ts_unix_ms.saturating_sub(start_ts);
                        if elapsed > THRESHOLD_MS {
                            out.push(Finding {
                                rule_id: "slow_first_token".into(),
                                severity: Severity::Minor,
                                category: "perf".into(),
                                summary: format!(
                                    "Turn {turn} first byte arrived after {elapsed}ms (threshold {THRESHOLD_MS}ms)"
                                ),
                                evidence: vec![
                                    EvidencePointer {
                                        trace_event: Some(start_seq),
                                        frame: None,
                                    },
                                    EvidencePointer {
                                        trace_event: Some(event.sequence),
                                        frame: None,
                                    },
                                ],
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// Two `ContextCompacted` events within 5 turns. Tight back-to-back
/// compaction is a strong indicator that the threshold is misconfigured
/// or that the model keeps re-bloating context after each compaction.
pub struct CompactionLoop;
impl Rule for CompactionLoop {
    fn rule_id(&self) -> &'static str {
        "compaction_loop"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        const TURN_WINDOW: u64 = 5;
        let mut compactions: Vec<(u64, u64)> = Vec::new(); // (sequence, turn_count_at_time)
        let mut turns_so_far = 0u64;
        for event in &ctx.events {
            match &event.kind {
                EvalEventKind::TurnStarted => turns_so_far += 1,
                EvalEventKind::ContextCompacted { .. } => {
                    compactions.push((event.sequence, turns_so_far));
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for win in compactions.windows(2) {
            let (seq_a, turn_a) = win[0];
            let (seq_b, turn_b) = win[1];
            if turn_b.saturating_sub(turn_a) <= TURN_WINDOW {
                out.push(Finding {
                    rule_id: "compaction_loop".into(),
                    severity: Severity::Major,
                    category: "perf".into(),
                    summary: format!(
                        "Two /compact events landed within {} turn(s); window threshold is {TURN_WINDOW}",
                        turn_b - turn_a
                    ),
                    evidence: vec![
                        EvidencePointer {
                            trace_event: Some(seq_a),
                            frame: None,
                        },
                        EvidencePointer {
                            trace_event: Some(seq_b),
                            frame: None,
                        },
                    ],
                });
            }
        }
        out
    }
}

/// Validates the Phase 2 `Assertion::FindingFired { rule_id }`
/// deferred markers: for each `deferred_finding_fired:<rule_id>`
/// action step, check whether the named rule actually emitted a
/// finding in this run. Failure becomes a finding so the scenario's
/// expectation isn't silently dropped.
pub struct DeferredFindingFired;
impl Rule for DeferredFindingFired {
    fn rule_id(&self) -> &'static str {
        "deferred_finding_fired"
    }
    fn check(&self, ctx: &TraceContext, _: &Scenario) -> Vec<Finding> {
        let mut requested: Vec<(u64, String)> = Vec::new();
        for (seq, _kind, status) in &ctx.action_steps {
            if let Some(rest) = status.strip_prefix("deferred_finding_fired:") {
                requested.push((*seq, rest.to_string()));
            }
        }
        if requested.is_empty() {
            return Vec::new();
        }
        let mut fired: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for event in &ctx.events {
            if let EvalEventKind::Finding { rule_id, .. } = &event.kind {
                fired.insert(rule_id.clone());
            }
        }
        requested
            .into_iter()
            .filter_map(|(seq, rule_id)| {
                if fired.contains(&rule_id) {
                    None
                } else {
                    Some(Finding {
                        rule_id: "deferred_finding_fired".into(),
                        severity: Severity::Major,
                        category: "scenario".into(),
                        summary: format!(
                            "Scenario asserted `{rule_id}` would fire, but no Finding with that rule_id was emitted"
                        ),
                        evidence: vec![EvidencePointer {
                            trace_event: Some(seq),
                            frame: None,
                        }],
                    })
                }
            })
            .collect()
    }
}

/// `expect.max_dropped_tool_calls` is set and the total observed
/// `dropped_tool_calls` (read from per-turn frames via the metrics
/// JSON) exceeded it. The producer-side wiring for the counter lives
/// in `squeezy-llm`; until that lands the metric stays at 0 and this
/// rule is a no-op, but the scenario-side knob is now plumbed.
pub struct ExpectDroppedToolCalls;
impl Rule for ExpectDroppedToolCalls {
    fn rule_id(&self) -> &'static str {
        "expect_dropped_tool_calls"
    }
    fn check(&self, ctx: &TraceContext, scenario: &Scenario) -> Vec<Finding> {
        let Some(max) = scenario.expect.max_dropped_tool_calls else {
            return Vec::new();
        };
        let mut total: u64 = 0;
        let mut last_seq: Option<u64> = None;
        for event in &ctx.events {
            if let EvalEventKind::TurnCompleted { metrics, .. } = &event.kind {
                let n = metrics
                    .get("dropped_tool_calls")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                total = total.saturating_add(n);
                last_seq = Some(event.sequence);
            }
        }
        if total > max as u64 {
            vec![Finding {
                rule_id: "expect_dropped_tool_calls".into(),
                severity: Severity::Major,
                category: "correctness".into(),
                summary: format!(
                    "Observed {total} dropped tool-call frames across the run; max allowed {max}"
                ),
                evidence: last_seq
                    .into_iter()
                    .map(|s| EvidencePointer {
                        trace_event: Some(s),
                        frame: None,
                    })
                    .collect(),
            }]
        } else {
            Vec::new()
        }
    }
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
