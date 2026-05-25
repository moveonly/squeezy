use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, PermissionAction, PermissionRequest, PermissionVerdict, Role, TranscriptItem, TurnId,
};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest};
use tokio_util::sync::CancellationToken;

const DEFAULT_POLICY: &str = include_str!("../../../docs/external/APPROVAL_POLICY.md");
const MAX_RECENT_TRANSCRIPT_ITEMS: usize = 15;
const MAX_USER_TOKENS: usize = 800;
const MAX_OTHER_TOKENS: usize = 400;
const MAX_TRANSCRIPT_TOKENS: usize = 2_000;
const CHARS_PER_TOKEN: usize = 4;
const CONSECUTIVE_DENIAL_TRIP: usize = 2;
const RECENT_WINDOW: usize = 20;
const RECENT_DENIAL_TRIP: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiReviewerTranscriptSnapshot {
    pub(crate) items: Vec<TranscriptItem>,
    pub(crate) history_version: u64,
    pub(crate) entry_count: usize,
}

#[derive(Debug, Default)]
pub(crate) struct AiReviewerState {
    turn_circuits: BTreeMap<u64, TurnCircuit>,
    transcript_cursor: Option<TranscriptCursor>,
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
}

pub(crate) async fn review_permission(input: AiReviewerInput<'_>) -> AiReviewerOutcome {
    let reviewer = &input.config.permissions.ai_reviewer;
    if !reviewer.enabled {
        return AiReviewerOutcome::NoDecision {
            reason: "ai reviewer disabled".to_string(),
        };
    }

    if let Some(reason) = input
        .state
        .lock()
        .expect("ai reviewer state")
        .bypass_reason(input.turn_id)
    {
        return AiReviewerOutcome::CircuitTripped { reason };
    }

    let policy = match load_policy(input.config) {
        Ok(policy) => policy,
        Err(reason) => return AiReviewerOutcome::NoDecision { reason },
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
        tools: Arc::from(Vec::new()),
        store: false,
    };
    let timeout = Duration::from_secs(reviewer.timeout_secs);
    let response = match tokio::time::timeout(
        timeout,
        collect_reviewer_text(input.provider.clone(), request, input.cancel.clone()),
    )
    .await
    {
        Ok(Ok(text)) => text,
        Ok(Err(reason)) => return AiReviewerOutcome::NoDecision { reason },
        Err(_) => {
            return AiReviewerOutcome::NoDecision {
                reason: "ai reviewer timed out".to_string(),
            };
        }
    };
    let Some(decision) = parse_reviewer_response(&response) else {
        return AiReviewerOutcome::NoDecision {
            reason: "ai reviewer returned invalid decision JSON".to_string(),
        };
    };
    match decision.action {
        PermissionAction::Allow => {
            input
                .state
                .lock()
                .expect("ai reviewer state")
                .record_non_denial(input.turn_id);
            if reviewer
                .allow_capabilities
                .contains(&input.request.capability)
            {
                AiReviewerOutcome::Verdict(PermissionVerdict {
                    action: PermissionAction::Allow,
                    matched_rule: None,
                    reason: format!("AI reviewer approved: {}", decision.reason),
                })
            } else {
                AiReviewerOutcome::NoDecision {
                    reason: format!(
                        "ai reviewer allow ignored for non-allowlisted {} capability",
                        input.request.capability.as_str()
                    ),
                }
            }
        }
        PermissionAction::Ask => {
            input
                .state
                .lock()
                .expect("ai reviewer state")
                .record_non_denial(input.turn_id);
            AiReviewerOutcome::NoDecision {
                reason: decision.reason,
            }
        }
        PermissionAction::Deny => {
            let tripped = input
                .state
                .lock()
                .expect("ai reviewer state")
                .record_denial(input.turn_id);
            if let Some(reason) = tripped {
                return AiReviewerOutcome::CircuitTripped { reason };
            }
            AiReviewerOutcome::Verdict(PermissionVerdict {
                action: PermissionAction::Deny,
                matched_rule: None,
                reason: format!("AI reviewer denied: {}", decision.reason),
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
            LlmEvent::Started | LlmEvent::ToolCall(_) => {}
        }
    }
    Ok(text)
}

fn load_policy(config: &AppConfig) -> Result<String, String> {
    let Some(policy_file) = &config.permissions.ai_reviewer.policy_file else {
        return Ok(DEFAULT_POLICY.to_string());
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
    let transcript_text = transcript
        .map(|snapshot| {
            let delta_marker = state.transcript_delta_marker(snapshot);
            bounded_transcript(snapshot, delta_marker.as_deref())
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

fn bounded_transcript(
    snapshot: &AiReviewerTranscriptSnapshot,
    delta_marker: Option<&str>,
) -> String {
    let recent_start = snapshot
        .items
        .len()
        .saturating_sub(MAX_RECENT_TRANSCRIPT_ITEMS);
    let recent = &snapshot.items[recent_start..];
    let last_user = recent.iter().rposition(|item| item.role == Role::User);
    let mut kept = Vec::new();
    let mut total_tokens = 0usize;

    if let Some(index) = last_user
        && let Some(line) = format_transcript_line(index, &recent[index], MAX_USER_TOKENS)
    {
        total_tokens = total_tokens.saturating_add(approx_tokens(&line));
        kept.push((index, line));
    }

    for (index, item) in recent.iter().enumerate().rev() {
        if Some(index) == last_user || total_tokens >= MAX_TRANSCRIPT_TOKENS {
            continue;
        }
        let remaining = MAX_TRANSCRIPT_TOKENS.saturating_sub(total_tokens);
        let role_cap = if item.role == Role::User {
            MAX_USER_TOKENS
        } else {
            MAX_OTHER_TOKENS
        };
        let cap = role_cap.min(remaining);
        if let Some(line) = format_transcript_line(index, item, cap) {
            total_tokens = total_tokens.saturating_add(approx_tokens(&line));
            kept.push((index, line));
        }
    }

    kept.sort_by_key(|(index, _)| *index);
    let mut lines = Vec::new();
    if let Some(delta_marker) = delta_marker {
        lines.push(delta_marker.to_string());
    }
    lines.extend(kept.into_iter().map(|(_, line)| line));
    if lines.is_empty() {
        "No recent transcript entries fit the review budget.".to_string()
    } else {
        lines.join("\n")
    }
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
}

fn trim_recent(recent: &mut VecDeque<bool>) {
    while recent.len() > RECENT_WINDOW {
        recent.pop_front();
    }
}

#[cfg(test)]
#[path = "ai_reviewer_tests.rs"]
mod tests;
