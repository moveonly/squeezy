use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime},
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, CostSnapshot, PermissionAction, PermissionCapability, PermissionRequest,
    PermissionRisk, PermissionVerdict, Role, TranscriptItem, TurnId,
};
use squeezy_llm::{
    LlmEvent, LlmInputItem, LlmOutputSchema, LlmProvider, LlmRequest, estimate_cost,
    provider_honors_output_schema,
};
use squeezy_skills::{APPROVAL_POLICY_DOC_PATH, bundled_doc};
use squeezy_telemetry::{TelemetryClient, TelemetryEvent};
use tokio_util::sync::CancellationToken;

/// Whether the cheap reviewer model may auto-approve an allowlisted request of
/// this capability and risk. Critical is never auto-approved (destructive ops
/// always reach a human). Network/Mcp are capped at Medium — a cheap model
/// rubber-stamping a High-risk reach-out (`curl … -d @secret`) is an
/// exfil/SSRF risk. Workspace-mutation capabilities (edit/shell/git/compiler)
/// are blast-radius-limited to the workspace — outside-workspace writes are
/// escalated separately (see the `outside_workspace` guard in the Allow arm) —
/// so they may auto-approve up to High.
fn within_auto_allow_ceiling(capability: PermissionCapability, risk: PermissionRisk) -> bool {
    if risk >= PermissionRisk::Critical {
        return false;
    }
    match capability {
        PermissionCapability::Network | PermissionCapability::Mcp => risk <= PermissionRisk::Medium,
        _ => risk <= PermissionRisk::High,
    }
}

/// Whether `request` targets a path outside the workspace (set by the tool
/// layer for file/shell writes). Such requests are never auto-approved — they
/// always reach a human even when the capability is allowlisted.
fn reviewer_request_outside_workspace(request: &PermissionRequest) -> bool {
    request
        .metadata
        .get("outside_workspace")
        .is_some_and(|value| value == "true")
}

/// Whether the reviewer may auto-approve an `Allow` verdict for `request`: the
/// capability must be allowlisted, the target in-workspace, and the risk within
/// the per-capability ceiling.
fn reviewer_may_auto_allow(
    allow_capabilities: &[PermissionCapability],
    request: &PermissionRequest,
) -> bool {
    allow_capabilities.contains(&request.capability)
        && !reviewer_request_outside_workspace(request)
        && within_auto_allow_ceiling(request.capability, request.risk)
}

fn default_policy() -> &'static str {
    bundled_doc(APPROVAL_POLICY_DOC_PATH).expect("APPROVAL_POLICY.md missing from bundled docs")
}
/// Number of most-recent transcript turns kept whole in the sliding window.
/// Older entries collapse into a single summary line so the reviewer always
/// sees the user's original intent even when the request lands many turns
/// downstream.
const RECENT_WINDOW_ITEMS: usize = 8;
const MAX_USER_TOKENS: usize = 2_000;
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

/// A reviewer decision plus the cost of the reviewer's LLM call, so the caller
/// can fold the (real, billable) review spend into session accounting instead
/// of dropping it. `cost`/`model` are populated only when a review call
/// actually billed; they default to an empty snapshot / empty model on paths
/// that never reached the provider (reviewer disabled, circuit-bypassed,
/// policy load failed, call errored, or timed out before a `Completed`).
pub(crate) struct AiReviewerResult {
    pub outcome: AiReviewerOutcome,
    pub cost: CostSnapshot,
    pub model: String,
}

impl AiReviewerResult {
    /// Decision reached without (or before) a billable LLM call.
    fn no_cost(outcome: AiReviewerOutcome) -> Self {
        Self {
            outcome,
            cost: CostSnapshot::default(),
            model: String::new(),
        }
    }
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

pub(crate) async fn review_permission(input: AiReviewerInput<'_>) -> AiReviewerResult {
    let reviewer = &input.config.permissions.ai_reviewer;
    if !reviewer.enabled {
        return AiReviewerResult::no_cost(AiReviewerOutcome::NoDecision {
            reason: "ai reviewer disabled".to_string(),
        });
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
        return AiReviewerResult::no_cost(AiReviewerOutcome::CircuitTripped { reason });
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
            return AiReviewerResult::no_cost(AiReviewerOutcome::NoDecision { reason });
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
    let output_schema =
        provider_honors_output_schema(input.provider.name(), &model).then(reviewer_output_schema);
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
        output_schema,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };
    let timeout = Duration::from_secs(reviewer.timeout_secs);
    let (response, mut reviewer_cost) = match tokio::time::timeout(
        timeout,
        collect_reviewer_text(input.provider.clone(), request, input.cancel.clone()),
    )
    .await
    {
        Ok(Ok((text, cost))) => (text, cost),
        Ok(Err(reason)) => {
            input.state.lock().expect("ai reviewer state").record_audit(
                input.turn_id,
                input.request,
                ReviewerAuditVerdict::NoDecision,
                &reason,
            );
            return AiReviewerResult::no_cost(AiReviewerOutcome::NoDecision { reason });
        }
        Err(_) => {
            let reason = "ai reviewer timed out".to_string();
            input.state.lock().expect("ai reviewer state").record_audit(
                input.turn_id,
                input.request,
                ReviewerAuditVerdict::NoDecision,
                &reason,
            );
            return AiReviewerResult::no_cost(AiReviewerOutcome::NoDecision { reason });
        }
    };
    // The reviewer LLM call is real billable spend; price it the same way the
    // main loop does when the provider stays silent on usage, then carry it on
    // every downstream return so the caller can fold it into session cost.
    if reviewer_cost.estimated_usd_micros.is_none() {
        reviewer_cost.estimated_usd_micros =
            estimate_cost(input.provider.name(), &model, &reviewer_cost);
    }
    let reviewer_result = |outcome| AiReviewerResult {
        outcome,
        cost: reviewer_cost.clone(),
        model: model.clone(),
    };
    let Some(decision) = parse_reviewer_response(&response) else {
        let reason = "ai reviewer returned invalid decision JSON".to_string();
        input.state.lock().expect("ai reviewer state").record_audit(
            input.turn_id,
            input.request,
            ReviewerAuditVerdict::NoDecision,
            &reason,
        );
        return reviewer_result(AiReviewerOutcome::NoDecision { reason });
    };
    reviewer_result(match decision.action {
        PermissionAction::Allow => {
            if reviewer_may_auto_allow(&reviewer.allow_capabilities, input.request) {
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
                // The reviewer judged Allow but auto-approval is withheld; record
                // why so /reviewer can explain the fall-through to a human prompt.
                input
                    .telemetry
                    .spawn(TelemetryEvent::ai_reviewer_allow_downgrade(
                        input.request.capability.as_str(),
                    ));
                let cap = input.request.capability.as_str();
                let reason = if !reviewer
                    .allow_capabilities
                    .contains(&input.request.capability)
                {
                    format!("ai reviewer allow ignored for non-allowlisted {cap} capability")
                } else if reviewer_request_outside_workspace(input.request) {
                    format!("ai reviewer allow withheld for out-of-workspace {cap} request")
                } else {
                    format!(
                        "ai reviewer allow withheld for {}-risk {cap} request (exceeds auto-allow ceiling)",
                        input.request.risk.as_str()
                    )
                };
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
            if reviewer_denial_requests_human_escalation(&decision.reason) {
                let reason = format!("AI reviewer escalated to human: {}", decision.reason);
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
                return reviewer_result(AiReviewerOutcome::NoDecision { reason });
            }
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
                return reviewer_result(AiReviewerOutcome::CircuitTripped { reason });
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
    })
}

fn reviewer_denial_requests_human_escalation(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    (lower.contains("escalat") && lower.contains("human"))
        || lower.contains("reach a human")
        || lower.contains("route to a human")
        || lower.contains("ask the human")
        || lower.contains("must ask")
        || lower.contains("never auto-approved")
        || lower.contains("not auto-approved")
        || lower.contains("cannot auto-approve")
        || lower.contains("can't auto-approve")
        || lower.contains("must not auto-approve")
}

async fn collect_reviewer_text(
    provider: Arc<dyn LlmProvider>,
    request: LlmRequest,
    cancel: CancellationToken,
) -> Result<(String, CostSnapshot), String> {
    let mut stream = provider.stream_response(request, cancel);
    let mut text = String::new();
    let mut cost = CostSnapshot::default();
    while let Some(event) = stream.next().await {
        match event.map_err(|err| err.to_string())? {
            LlmEvent::TextDelta(delta) => text.push_str(&delta),
            // Capture the provider-reported usage so the caller can bill the
            // review round; a mid-stream cancel leaves the default snapshot.
            LlmEvent::Completed {
                cost: completed, ..
            } => {
                cost = completed;
                break;
            }
            LlmEvent::Cancelled => break,
            LlmEvent::Started
            | LlmEvent::ToolCall(_)
            | LlmEvent::ReasoningDelta { .. }
            | LlmEvent::ReasoningDone(_)
            | LlmEvent::ContextOverflow { .. }
            | LlmEvent::ServerModel(_) => {}
            // `LlmEvent` is `#[non_exhaustive]`; unknown future variants
            // contribute nothing to the reviewer's collected text.
            _ => { /* future variant */ }
        }
    }
    Ok((text, cost))
}

fn load_policy(config: &AppConfig) -> Result<String, String> {
    let reviewer = &config.permissions.ai_reviewer;
    let base = match &reviewer.policy_file {
        None => default_policy().to_string(),
        Some(policy_file) => {
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
            })?
        }
    };
    // Append the project's extra instructions to whichever base policy applies,
    // so a project can extend the policy without replacing it wholesale.
    Ok(match &reviewer.policy {
        Some(extra) if !extra.trim().is_empty() => {
            format!("{base}\n\n## Additional project policy\n\n{}", extra.trim())
        }
        _ => base,
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
    let mut rendered = String::new();
    if let Some(delta_marker) = delta_marker {
        push_transcript_output_line(&mut rendered, delta_marker);
    }
    if let Some(summary) = summary_line {
        push_transcript_output_line(&mut rendered, &summary);
    }
    for (_, line) in kept {
        push_transcript_output_line(&mut rendered, &line);
    }
    if rendered.is_empty() {
        "No recent transcript entries fit the review budget.".to_string()
    } else {
        rendered
    }
}

fn push_transcript_output_line(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
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
    const TRUNCATED_SUFFIX: &str = " [truncated]";
    const TRUNCATED_SUFFIX_CHARS: usize = 12;

    let take_chars = max_chars.saturating_sub(TRUNCATED_SUFFIX_CHARS);
    let mut cutoff = value.len();
    for (count, (index, _)) in value.char_indices().enumerate() {
        if count == take_chars {
            cutoff = index;
        }
        if count == max_chars {
            let mut output = String::with_capacity(cutoff + TRUNCATED_SUFFIX.len());
            output.push_str(&value[..cutoff]);
            output.push_str(TRUNCATED_SUFFIX);
            return output;
        }
    }
    value.to_string()
}

fn approx_tokens(value: &str) -> usize {
    value.chars().count().div_ceil(CHARS_PER_TOKEN)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewerDecision {
    action: PermissionAction,
    reason: String,
}

/// Strict JSON-schema contract mirroring [`ReviewerDecision`]: the
/// `action` enum carries the three canonical values
/// (`PermissionMode::as_str`) the policy doc instructs the model to emit,
/// and `reason` matches the free-text field. Attached only on providers
/// that forward `output_schema` ([`provider_honors_output_schema`]) so the
/// cheap reviewer model returns a schema-valid object instead of fenced or
/// prose-wrapped JSON that `parse_reviewer_response` rejects and bills a
/// retry round on — providers that drop the schema keep the loose path.
fn reviewer_output_schema() -> LlmOutputSchema {
    LlmOutputSchema {
        name: "permission_review".to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        PermissionAction::Allow.as_str(),
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
