use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime},
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, PermissionAction, PermissionCapability, PermissionRequest, PermissionVerdict, Role,
    TranscriptItem, TurnId,
};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest};
use squeezy_skills::{APPROVAL_POLICY_DOC_PATH, bundled_doc};
use squeezy_telemetry::{TelemetryClient, TelemetryEvent};
use tokio_util::sync::CancellationToken;

fn default_policy() -> &'static str {
    bundled_doc(APPROVAL_POLICY_DOC_PATH).expect("APPROVAL_POLICY.md missing from bundled docs")
}
/// Number of most-recent transcript turns kept whole in the sliding window.
/// Older entries collapse into a single summary line so the reviewer always
/// sees the user's original intent even when the request lands many turns
/// downstream.
const RECENT_WINDOW_ITEMS: usize = 8;
const MAX_USER_TOKENS: usize = 800;
const MAX_OTHER_TOKENS: usize = 400;
const SUMMARY_TOKEN_RESERVE: usize = 200;
const CHARS_PER_TOKEN: usize = 4;
const CONSECUTIVE_DENIAL_TRIP: usize = 2;
const RECENT_WINDOW: usize = 20;
const RECENT_DENIAL_TRIP: usize = 5;
const AUDIT_RING_CAPACITY: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiReviewerTranscriptSnapshot {
    pub(crate) items: Vec<TranscriptItem>,
    pub(crate) history_version: u64,
    pub(crate) entry_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewerAuditVerdict {
    Allow,
    Deny,
    NoDecision,
    CircuitTripped,
}

impl ReviewerAuditVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::NoDecision => "no-decision",
            Self::CircuitTripped => "circuit-tripped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerAuditEntry {
    pub recorded_at: SystemTime,
    pub turn_id: u64,
    pub tool_name: String,
    pub capability: PermissionCapability,
    pub target: String,
    pub verdict: ReviewerAuditVerdict,
    pub reason: String,
}

#[derive(Debug, Default)]
pub(crate) struct AiReviewerState {
    turn_circuits: BTreeMap<u64, TurnCircuit>,
    transcript_cursor: Option<TranscriptCursor>,
    audit: VecDeque<ReviewerAuditEntry>,
}

#[derive(Debug, Clone, Copy)]
struct TranscriptCursor {
    history_version: u64,
    entry_count: usize,
}

#[derive(Debug, Default)]
struct TurnCircuit {
    tripped: bool,
    consecutive_denials: usize,
    recent: VecDeque<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AiReviewerOutcome {
    Verdict(PermissionVerdict),
    NoDecision { reason: String },
    CircuitTripped { reason: String },
}

pub(crate) struct AiReviewerInput<'a> {
    pub(crate) config: &'a AppConfig,
    pub(crate) provider: Arc<dyn LlmProvider>,
    pub(crate) request: &'a PermissionRequest,
    pub(crate) transcript: Option<AiReviewerTranscriptSnapshot>,
    pub(crate) state: Arc<StdMutex<AiReviewerState>>,
    pub(crate) turn_id: TurnId,
    pub(crate) cancel: CancellationToken,
    pub(crate) telemetry: TelemetryClient,
}

pub(crate) async fn review_permission(input: AiReviewerInput<'_>) -> AiReviewerOutcome {
    let reviewer = &input.config.permissions.ai_reviewer;
    if !reviewer.enabled {
        return AiReviewerOutcome::NoDecision {
            reason: "ai reviewer disabled".to_string(),
        };
    }

    if let Some(reason) = {
        let mut state = input.state.lock().expect("ai reviewer state");
        let reason = state.bypass_reason(input.turn_id);
        if let Some(reason) = reason.as_deref() {
            state.record_audit(
                input.turn_id,
                input.request,
                ReviewerAuditVerdict::CircuitTripped,
                reason,
            );
        }
        reason
    } {
        return AiReviewerOutcome::CircuitTripped { reason };
    }

    let policy = match load_policy(input.config) {
        Ok(policy) => policy,
        Err(reason) => {
            input.state.lock().expect("ai reviewer state").record_audit(
                input.turn_id,
                input.request,
                ReviewerAuditVerdict::NoDecision,
                &reason,
            );
            return AiReviewerOutcome::NoDecision { reason };
        }
    };
    let prompt = {
        let mut state = input.state.lock().expect("ai reviewer state");
        build_review_prompt(
            input.config,
            input.request,
            input.transcript.as_ref(),
            &policy,
            &mut state,
        )
    };
    let model = reviewer
        .model
        .clone()
        .or_else(|| input.config.resolved_small_fast_model())
        .unwrap_or_else(|| input.config.model.clone());
    let request = LlmRequest {
        model: Arc::from(model.as_str()),
        instructions: Arc::from(
            "Review one Squeezy permission request. Return only compact JSON with action and reason.",
        ),
        input: Arc::from(vec![LlmInputItem::UserText(prompt)]),
        max_output_tokens: Some(120),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: squeezy_llm::CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };
    let timeout = Duration::from_secs(reviewer.timeout_secs);
    let response = match tokio::time::timeout(
        timeout,
        collect_reviewer_text(input.provider.clone(), request, input.cancel.clone()),
    )
    .await
    {
        Ok(Ok(text)) => text,
        Ok(Err(reason)) => {
            input.state.lock().expect("ai reviewer state").record_audit(
                input.turn_id,
                input.request,
                ReviewerAuditVerdict::NoDecision,
                &reason,
            );
            return AiReviewerOutcome::NoDecision { reason };
        }
        Err(_) => {
            let reason = "ai reviewer timed out".to_string();
            input.state.lock().expect("ai reviewer state").record_audit(
                input.turn_id,
                input.request,
                ReviewerAuditVerdict::NoDecision,
                &reason,
            );
            return AiReviewerOutcome::NoDecision { reason };
        }
    };
    let Some(decision) = parse_reviewer_response(&response) else {
        let reason = "ai reviewer returned invalid decision JSON".to_string();
        input.state.lock().expect("ai reviewer state").record_audit(
            input.turn_id,
            input.request,
            ReviewerAuditVerdict::NoDecision,
            &reason,
        );
        return AiReviewerOutcome::NoDecision { reason };
    };
    match decision.action {
        PermissionAction::Allow => {
            if reviewer
                .allow_capabilities
                .contains(&input.request.capability)
            {
                let reason = format!("AI reviewer approved: {}", decision.reason);
                {
                    let mut state = input.state.lock().expect("ai reviewer state");
                    state.record_non_denial(input.turn_id);
                    state.record_audit(
                        input.turn_id,
                        input.request,
                        ReviewerAuditVerdict::Allow,
                        &reason,
                    );
                }
                AiReviewerOutcome::Verdict(PermissionVerdict {
                    action: PermissionAction::Allow,
                    matched_rule: None,
                    reason,
                    silent: false,
                })
            } else {
                input
                    .telemetry
                    .spawn(TelemetryEvent::ai_reviewer_allow_downgrade(
                        input.request.capability.as_str(),
                    ));
                let reason = format!(
                    "ai reviewer allow ignored for non-allowlisted {} capability",
                    input.request.capability.as_str()
                );
                {
                    let mut state = input.state.lock().expect("ai reviewer state");
                    state.record_non_denial(input.turn_id);
                    state.record_audit(
                        input.turn_id,
                        input.request,
                        ReviewerAuditVerdict::NoDecision,
                        &reason,
                    );
                }
                AiReviewerOutcome::NoDecision { reason }
            }
        }
        PermissionAction::Ask => {
            let reason = decision.reason;
            {
                let mut state = input.state.lock().expect("ai reviewer state");
                state.record_non_denial(input.turn_id);
                state.record_audit(
                    input.turn_id,
                    input.request,
                    ReviewerAuditVerdict::NoDecision,
                    &reason,
                );
            }
            AiReviewerOutcome::NoDecision { reason }
        }
        PermissionAction::Deny => {
            let tripped = {
                let mut state = input.state.lock().expect("ai reviewer state");
                state.record_denial(input.turn_id)
            };
            if let Some(reason) = tripped {
                input.state.lock().expect("ai reviewer state").record_audit(
                    input.turn_id,
                    input.request,
                    ReviewerAuditVerdict::CircuitTripped,
                    &reason,
                );
                return AiReviewerOutcome::CircuitTripped { reason };
            }
            let reason = format!("AI reviewer denied: {}", decision.reason);
            input.state.lock().expect("ai reviewer state").record_audit(
                input.turn_id,
                input.request,
                ReviewerAuditVerdict::Deny,
                &reason,
            );
            // Anti-pattern guard (audit
            // 04-sandboxing-and-permissions.md, "Do not silent-deny by default
            // on the AI reviewer"): keep the explained-deny shape so the user
            // can evaluate the model's reasoning. Silent is only for explicit
            // `silent = true` rules loaded from TOML.
            AiReviewerOutcome::Verdict(PermissionVerdict {
                action: PermissionAction::Deny,
                matched_rule: None,
                reason,
                silent: false,
            })
        }
    }
}

async fn collect_reviewer_text(
    provider: Arc<dyn LlmProvider>,
    request: LlmRequest,
    cancel: CancellationToken,
) -> Result<String, String> {
    let mut stream = provider.stream_response(request, cancel);
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event.map_err(|err| err.to_string())? {
            LlmEvent::TextDelta(delta) => text.push_str(&delta),
            LlmEvent::Completed { .. } | LlmEvent::Cancelled => break,
            LlmEvent::Started
            | LlmEvent::ToolCall(_)
            | LlmEvent::ReasoningDelta { .. }
            | LlmEvent::ReasoningDone(_)
            | LlmEvent::ContextOverflow { .. }
            | LlmEvent::ServerModel(_) => {}
        }
    }
    Ok(text)
}

fn load_policy(config: &AppConfig) -> Result<String, String> {
    let Some(policy_file) = &config.permissions.ai_reviewer.policy_file else {
        return Ok(default_policy().to_string());
    };
    let path = if policy_file.is_absolute() {
        policy_file.clone()
    } else {
        config.workspace_root.join(policy_file)
    };
    fs::read_to_string(&path).map_err(|err| {
        format!(
            "failed to read AI reviewer policy {}: {err}",
            path.display()
        )
    })
}

fn build_review_prompt(
    config: &AppConfig,
    request: &PermissionRequest,
    transcript: Option<&AiReviewerTranscriptSnapshot>,
    policy: &str,
    state: &mut AiReviewerState,
) -> String {
    let max_transcript_tokens = config.permissions.ai_reviewer.max_transcript_tokens;
    let transcript_text = transcript
        .map(|snapshot| {
            let delta_marker = state.transcript_delta_marker(snapshot);
            bounded_transcript(snapshot, delta_marker.as_deref(), max_transcript_tokens)
        })
        .unwrap_or_else(|| "No transcript snapshot is available.".to_string());
    let payload = json!({
        "tool_name": request.tool_name,
        "capability": request.capability.as_str(),
        "target": request.target,
        "risk": request.risk.as_str(),
        "summary": request.summary,
        "metadata": request.metadata,
        "allow_capabilities": config
            .permissions
            .ai_reviewer
            .allow_capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
    });
    format!(
        "Approval policy:\n{policy}\n\nBounded transcript:\n{transcript_text}\n\nPermission request:\n{payload}\n\nReturn JSON only."
    )
}

/// Render a transcript snapshot under `max_transcript_tokens`, keeping the
/// last `RECENT_WINDOW_ITEMS` turns whole and collapsing older turns into one
/// compacted summary line. The last user turn is always preserved so the
/// reviewer never loses intent context even when the permission request lands
/// many turns downstream of the original ask.
fn bounded_transcript(
    snapshot: &AiReviewerTranscriptSnapshot,
    delta_marker: Option<&str>,
    max_transcript_tokens: usize,
) -> String {
    if max_transcript_tokens == 0 || snapshot.items.is_empty() {
        return "No recent transcript entries fit the review budget.".to_string();
    }
    let items = snapshot.items.as_slice();
    let recent_start = items.len().saturating_sub(RECENT_WINDOW_ITEMS);
    let older = &items[..recent_start];
    let recent = &items[recent_start..];
    let last_user_global = items.iter().rposition(|item| item.role == Role::User);

    let summary_budget = if older.is_empty() {
        0
    } else {
        SUMMARY_TOKEN_RESERVE.min(max_transcript_tokens / 4)
    };
    let recent_budget = max_transcript_tokens.saturating_sub(summary_budget);

    let mut kept: Vec<(usize, String)> = Vec::new();
    let mut total_tokens = 0usize;

    if let Some(index) = last_user_global
        && index >= recent_start
        && let Some(line) = format_transcript_line(index, &items[index], MAX_USER_TOKENS)
    {
        total_tokens = total_tokens.saturating_add(approx_tokens(&line));
        kept.push((index, line));
    }

    for (offset, item) in recent.iter().enumerate().rev() {
        let global_index = recent_start + offset;
        if Some(global_index) == last_user_global {
            continue;
        }
        if total_tokens >= recent_budget {
            break;
        }
        let remaining = recent_budget.saturating_sub(total_tokens);
        let role_cap = if item.role == Role::User {
            MAX_USER_TOKENS
        } else {
            MAX_OTHER_TOKENS
        };
        let cap = role_cap.min(remaining);
        if let Some(line) = format_transcript_line(global_index, item, cap) {
            total_tokens = total_tokens.saturating_add(approx_tokens(&line));
            kept.push((global_index, line));
        }
    }

    if let Some(index) = last_user_global
        && index < recent_start
        && let Some(line) = format_transcript_line(index, &items[index], MAX_USER_TOKENS)
    {
        let _ = total_tokens.saturating_add(approx_tokens(&line));
        kept.push((index, line));
    }

    let summary_line = if older.is_empty() {
        None
    } else {
        compact_older_summary(older, last_user_global, summary_budget)
    };

    kept.sort_by_key(|(index, _)| *index);
    let mut lines = Vec::new();
    if let Some(delta_marker) = delta_marker {
        lines.push(delta_marker.to_string());
    }
    if let Some(summary) = summary_line {
        lines.push(summary);
    }
    lines.extend(kept.into_iter().map(|(_, line)| line));
    if lines.is_empty() {
        "No recent transcript entries fit the review budget.".to_string()
    } else {
        lines.join("\n")
    }
}

/// Collapse pre-window transcript items into a single budgeted summary line.
/// Counts roles and concatenates the head of each entry up to the per-summary
/// budget so the reviewer retains a synopsis of older intent without paying
/// per-entry overhead.
fn compact_older_summary(
    older: &[TranscriptItem],
    last_user_global: Option<usize>,
    summary_budget_tokens: usize,
) -> Option<String> {
    if older.is_empty() || summary_budget_tokens == 0 {
        return None;
    }
    let mut users = 0usize;
    let mut assistants = 0usize;
    let mut systems = 0usize;
    for item in older {
        match item.role {
            Role::User => users += 1,
            Role::Assistant => assistants += 1,
            Role::System => systems += 1,
        }
    }

    let header = format!(
        "[summary of {} earlier turn(s): {users} user, {assistants} assistant, {systems} system]",
        older.len()
    );
    let header_tokens = approx_tokens(&header);
    if header_tokens >= summary_budget_tokens {
        return Some(header);
    }
    let body_budget_tokens = summary_budget_tokens.saturating_sub(header_tokens);
    let body_budget_chars = body_budget_tokens.saturating_mul(CHARS_PER_TOKEN);
    if body_budget_chars == 0 {
        return Some(header);
    }
    // Prefer the last user turn (intent) when it falls outside the recent
    // window, otherwise fall back to the most recent older entry.
    let pick_index = match last_user_global {
        Some(index) if index < older.len() => Some(index),
        _ => older.iter().rposition(|item| item.role == Role::User),
    };
    let pick = pick_index
        .map(|index| &older[index])
        .or_else(|| older.last())?;
    let role = match pick.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    };
    let snippet = truncate_chars(&pick.content, body_budget_chars);
    Some(format!("{header} {role}: {snippet}"))
}

fn format_transcript_line(
    index: usize,
    item: &TranscriptItem,
    max_tokens: usize,
) -> Option<String> {
    if max_tokens == 0 {
        return None;
    }
    let max_chars = max_tokens.saturating_mul(CHARS_PER_TOKEN);
    let content = truncate_chars(&item.content, max_chars);
    let role = match item.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    };
    Some(format!("{index}:{role}: {content}"))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(12))
        .collect::<String>();
    output.push_str(" [truncated]");
    output
}

fn approx_tokens(value: &str) -> usize {
    value.chars().count().div_ceil(CHARS_PER_TOKEN)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewerDecision {
    action: PermissionAction,
    reason: String,
}

fn parse_reviewer_response(text: &str) -> Option<ReviewerDecision> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let value: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let action = value
        .get("action")?
        .as_str()
        .and_then(PermissionAction::parse)?;
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .unwrap_or("no reason supplied")
        .to_string();
    Some(ReviewerDecision { action, reason })
}

impl AiReviewerState {
    fn bypass_reason(&mut self, turn_id: TurnId) -> Option<String> {
        let circuit = self.turn_circuits.entry(turn_id.get()).or_default();
        circuit
            .tripped
            .then(|| "ai reviewer circuit breaker is tripped for this turn".to_string())
    }

    fn record_non_denial(&mut self, turn_id: TurnId) {
        let circuit = self.turn_circuits.entry(turn_id.get()).or_default();
        circuit.consecutive_denials = 0;
        circuit.recent.push_back(false);
        trim_recent(&mut circuit.recent);
    }

    fn record_denial(&mut self, turn_id: TurnId) -> Option<String> {
        let circuit = self.turn_circuits.entry(turn_id.get()).or_default();
        circuit.consecutive_denials = circuit.consecutive_denials.saturating_add(1);
        circuit.recent.push_back(true);
        trim_recent(&mut circuit.recent);
        let recent_denials = circuit.recent.iter().filter(|denied| **denied).count();
        if circuit.consecutive_denials >= CONSECUTIVE_DENIAL_TRIP {
            circuit.tripped = true;
            return Some(format!(
                "ai reviewer denied {CONSECUTIVE_DENIAL_TRIP} requests consecutively"
            ));
        }
        if recent_denials >= RECENT_DENIAL_TRIP {
            circuit.tripped = true;
            return Some(format!(
                "ai reviewer denied {recent_denials} of the last {} reviewed requests",
                circuit.recent.len()
            ));
        }
        None
    }

    fn transcript_delta_marker(
        &mut self,
        snapshot: &AiReviewerTranscriptSnapshot,
    ) -> Option<String> {
        let marker = self.transcript_cursor.and_then(|cursor| {
            (cursor.history_version == snapshot.history_version
                && cursor.entry_count < snapshot.entry_count)
                .then(|| {
                    format!(
                        "[{} earlier entries reviewed previously and unchanged]",
                        cursor.entry_count
                    )
                })
        });
        self.transcript_cursor = Some(TranscriptCursor {
            history_version: snapshot.history_version,
            entry_count: snapshot.entry_count,
        });
        marker
    }

    fn record_audit(
        &mut self,
        turn_id: TurnId,
        request: &PermissionRequest,
        verdict: ReviewerAuditVerdict,
        reason: &str,
    ) {
        if self.audit.len() == AUDIT_RING_CAPACITY {
            self.audit.pop_front();
        }
        self.audit.push_back(ReviewerAuditEntry {
            recorded_at: SystemTime::now(),
            turn_id: turn_id.get(),
            tool_name: request.tool_name.clone(),
            capability: request.capability,
            target: request.target.clone(),
            verdict,
            reason: reason.to_string(),
        });
    }

    pub fn recent_decisions(&self) -> Vec<ReviewerAuditEntry> {
        self.audit.iter().cloned().collect()
    }
}

fn trim_recent(recent: &mut VecDeque<bool>) {
    while recent.len() > RECENT_WINDOW {
        recent.pop_front();
    }
}

#[cfg(test)]
#[path = "ai_reviewer_tests.rs"]
mod tests;
