use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextCompactionRecord, ContextCompactionState,
    ContextCompactionTrigger, ContextEstimate, ContextPin, Redactor, SqueezyError,
    context_attachment_preview,
};
use squeezy_llm::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest};
use squeezy_store::{
    ResumeItem, SessionHandle, SqueezyStore, StoredReadSnapshot, StoredToolReceipt,
};
use squeezy_tools::{ToolCostHint, ToolReceipt, ToolResult, ToolStatus, sha256_hex};
use tokio_util::sync::CancellationToken;

use crate::{
    COMPACTION_PIN_SUMMARY_MAX_CHARS, collapse_status_text, compact_text, llm_input_to_resume_item,
    log_session_event, tool_result_summary, unix_timestamp_millis,
};

// Compaction summary truncation budgets. These are character (not byte)
// caps because they pass through `compact_text` → `truncate_chars`. They
// stay collocated so a future audit can read the total summary growth
// in one place rather than chasing literals across `build_compaction_summary`.
const COMPACTION_PREVIOUS_SUMMARY_MAX_CHARS: usize = 1_200;
const COMPACTION_DURABLE_LINE_MAX_CHARS: usize = 320;
const COMPACTION_TOOL_ARGS_MAX_CHARS: usize = 260;
const COMPACTION_TOOL_OUTPUT_MAX_CHARS: usize = 260;
const COMPACTION_RECEIPT_MAX_CHARS: usize = 260;
const COMPACTION_UNRESOLVED_MAX_CHARS: usize = 240;
const COMPACTION_ATTACHMENT_PREVIEW_MAX_CHARS: usize = 220;
const COMPACTION_DURABLE_LINES_LIMIT: usize = 24;
const COMPACTION_UNRESOLVED_LINES_LIMIT: usize = 8;
const COMPACTION_RECEIPT_LINES_LIMIT: usize = 12;
const COMPACTION_MAX_HISTORY: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCompactionReport {
    pub record: ContextCompactionRecord,
    pub summary: String,
    /// Pre-compaction conversation slice. Stamped into the
    /// `context_compacted` session event so replay can snap to this
    /// checkpoint without re-reading the redb mirror.
    pub dropped: Vec<ResumeItem>,
}

/// Trigger compaction mid-turn when the configured context window is in
/// danger. Returns the produced compaction report when it fired; `None`
/// when the feature is disabled, the window isn't configured, or the
/// threshold hasn't been crossed.
pub(crate) fn maybe_compact_mid_turn(
    conversation: &mut Vec<LlmInputItem>,
    state: &mut ContextCompactionState,
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
    last_total_tokens: Option<u64>,
) -> Option<ContextCompactionReport> {
    if !config.context_compaction.enabled_mid_turn {
        return None;
    }
    let window = config.context_compaction.model_context_window?;
    if window == 0 {
        return None;
    }
    let threshold = window
        .saturating_mul(config.context_compaction.threshold_percent.min(100) as u64)
        .saturating_div(100);
    let observed =
        last_total_tokens.unwrap_or_else(|| estimate_context(conversation).estimated_tokens);
    if observed < threshold {
        return None;
    }
    compact_conversation(
        conversation,
        state,
        attachments,
        store,
        config,
        ContextCompactionTrigger::Auto,
        true,
    )
}

pub(crate) fn maybe_compact_conversation(
    conversation: &mut Vec<LlmInputItem>,
    state: &mut ContextCompactionState,
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
    trigger: ContextCompactionTrigger,
) -> Option<ContextCompactionReport> {
    if !config.context_compaction.enabled {
        return None;
    }
    let estimate = estimate_context(conversation);
    if estimate.items < config.context_compaction.min_items
        || estimate.estimated_tokens < config.context_compaction.estimated_tokens
    {
        return None;
    }
    compact_conversation(
        conversation,
        state,
        attachments,
        store,
        config,
        trigger,
        false,
    )
}

pub(crate) fn compact_conversation(
    conversation: &mut Vec<LlmInputItem>,
    state: &mut ContextCompactionState,
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
    trigger: ContextCompactionTrigger,
    force: bool,
) -> Option<ContextCompactionReport> {
    let before = estimate_context(conversation);
    let keep = config.context_compaction.recent_items.max(1);
    if !force && before.items <= keep {
        return None;
    }
    let initial_split = conversation.len().saturating_sub(keep);
    if initial_split == 0 {
        return None;
    }
    // Tool calls and their outputs are pushed as contiguous pairs in the
    // turn loop. If the naive split falls between a `FunctionCall` and
    // its matching `FunctionCallOutput`, the recent slice would start
    // with an orphan output whose `call_id` is no longer declared on the
    // wire — the OpenAI Responses provider rejects that input. Snap the
    // boundary forward so any leading `FunctionCallOutput` in `recent`
    // whose `FunctionCall` lives in `older` is absorbed back into older.
    let split = snap_compaction_split(conversation, initial_split);
    if split == 0 || split >= conversation.len() {
        return None;
    }

    let older = conversation[..split].to_vec();
    let recent = conversation[split..].to_vec();
    let generation = state.generation.saturating_add(1);
    let summary = build_compaction_summary(generation, state, &older, attachments, store, config);
    // `snap_compaction_split` handles *consecutive* leading orphan outputs,
    // but parallel tool calls produce a `[FC(A), FC(B), FCO(A), FCO(B)]`
    // shape. If the split lands between the two calls, snap stops at the
    // leading `FC(B)` and `FCO(A)` survives in `recent` as an orphan whose
    // declaring `FC(A)` is in the dropped `older` slice. Drop any such
    // orphan outputs before they reach the next provider request.
    let recent = drop_orphan_function_call_outputs(recent);
    let mut compacted = Vec::with_capacity(recent.len() + 1);
    compacted.push(LlmInputItem::UserText(summary.clone()));
    compacted.extend(recent);
    let after = estimate_context(&compacted);
    if !force && after.bytes >= before.bytes {
        return None;
    }
    *conversation = compacted;

    let dropped: Vec<squeezy_store::ResumeItem> =
        older.into_iter().map(llm_input_to_resume_item).collect();

    // Persist the dropped slice as a checkpoint so a future
    // `compact_context_undo` can hydrate it. The write is best-effort:
    // failing to persist must not abort the compaction itself, otherwise
    // a transient redb hiccup would mean losing the summary as well.
    let compacted_at_ms = unix_timestamp_millis();
    let replacement_id = store.and_then(|store| {
        if dropped.is_empty() {
            return None;
        }
        let id = format!("ckpt-{generation}-{compacted_at_ms}");
        let checkpoint = squeezy_store::CompactionCheckpoint {
            replacement_id: id.clone(),
            session_id: String::new(),
            generation,
            items: dropped.clone(),
            created_unix_millis: compacted_at_ms as u128,
        };
        match store.put_compaction_checkpoint(&checkpoint) {
            Ok(()) => Some(id),
            Err(err) => {
                tracing::warn!(
                    target: "squeezy::compaction",
                    error = %err,
                    "failed to persist compaction checkpoint; undo will be unavailable",
                );
                None
            }
        }
    });

    let record = ContextCompactionRecord {
        generation,
        trigger,
        compacted_at_ms,
        before,
        after,
        dropped_items: split,
        summary_bytes: summary.len(),
        replacement_id,
    };
    state.generation = generation;
    state.summary = Some(summary.clone());
    state.last = Some(record.clone());
    state.history.push(record.clone());
    if state.history.len() > COMPACTION_MAX_HISTORY {
        let excess = state.history.len() - COMPACTION_MAX_HISTORY;
        state.history.drain(0..excess);
    }
    Some(ContextCompactionReport {
        record,
        summary,
        dropped,
    })
}

/// Adjusts a proposed compaction split point so `recent` does not start
/// with a `FunctionCallOutput` whose declaring `FunctionCall` has been
/// dropped into `older`. The OpenAI Responses provider serializes each
/// `function_call_output` with a bare `call_id` and the API rejects any
/// payload where a `call_id` is not also present as a `function_call`.
///
/// The strategy is to scan forward from `initial_split` and skip past
/// any `FunctionCallOutput` items whose `call_id` was already declared
/// by a `FunctionCall` in the older slice. We stop once the next item
/// in the recent slice is either a non-tool item (text) or a fresh
/// `FunctionCall` that begins a new pair. The split may grow up to
/// `conversation.len()`; the caller treats `>= conversation.len()` as
/// "nothing left to compact" and bails out without bumping generation.
fn snap_compaction_split(conversation: &[LlmInputItem], initial_split: usize) -> usize {
    let mut split = initial_split;
    while split < conversation.len() {
        match &conversation[split] {
            LlmInputItem::FunctionCallOutput { call_id, .. } => {
                let declared_in_older = conversation[..split].iter().any(|item| match item {
                    LlmInputItem::FunctionCall {
                        call_id: declared, ..
                    } => declared == call_id,
                    _ => false,
                });
                if declared_in_older {
                    split += 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    split
}

/// Drop any `FunctionCallOutput` whose `call_id` is not declared by a
/// `FunctionCall` somewhere in `items`. Used post-compaction to ensure
/// the kept slice cannot reference a tool call that lived only in the
/// summarized older slice. Order is preserved.
pub(crate) fn drop_orphan_function_call_outputs(items: Vec<LlmInputItem>) -> Vec<LlmInputItem> {
    use std::collections::BTreeSet;
    let declared: BTreeSet<&str> = items
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    items
        .iter()
        .filter(|item| match item {
            LlmInputItem::FunctionCallOutput { call_id, .. } => declared.contains(call_id.as_str()),
            _ => true,
        })
        .cloned()
        .collect()
}

pub(crate) fn estimate_context(conversation: &[LlmInputItem]) -> ContextEstimate {
    let bytes = conversation
        .iter()
        .map(llm_item_estimated_bytes)
        .fold(0usize, usize::saturating_add);
    ContextEstimate {
        bytes,
        estimated_tokens: estimated_tokens(bytes),
        items: conversation.len(),
    }
}

fn estimated_tokens(bytes: usize) -> u64 {
    bytes.saturating_add(3).saturating_div(4) as u64
}

fn llm_item_estimated_bytes(item: &LlmInputItem) -> usize {
    match item {
        LlmInputItem::UserText(text) | LlmInputItem::AssistantText(text) => text.len(),
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => call_id.len() + name.len() + arguments.to_string().len(),
        LlmInputItem::FunctionCallOutput { call_id, output } => call_id.len() + output.len(),
        LlmInputItem::Reasoning(payload) => payload.display_text().len(),
    }
}

/// Strategy-aware compaction. Always runs the extractive pipeline first;
/// when the configured strategy is `ModelAssisted` (or `LayeredFallback`
/// over its threshold) and a cheap model is configured, the synthetic
/// summary head is then re-written by that model with a hard timeout.
/// Any error, timeout, or empty response falls back to the extractive
/// summary verbatim — the extractive contract is load-bearing.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compact_conversation_with_strategy(
    conversation: &mut Vec<LlmInputItem>,
    state: &mut ContextCompactionState,
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    provider: &Arc<dyn LlmProvider>,
    session: Option<&SessionHandle>,
    redactor: &Redactor,
    config: &AppConfig,
    trigger: ContextCompactionTrigger,
    force: bool,
) -> Option<ContextCompactionReport> {
    let report = compact_conversation(
        conversation,
        state,
        attachments,
        store,
        config,
        trigger,
        force,
    )?;
    let strategy = config.context_compaction.strategy;
    if strategy == squeezy_core::CompactionStrategy::Extractive {
        return Some(report);
    }
    let dropped_estimated_tokens = report
        .record
        .before
        .estimated_tokens
        .saturating_sub(report.record.after.estimated_tokens);
    let threshold = config
        .context_compaction
        .layered_fallback_extractive_threshold_tokens as u64;
    if strategy == squeezy_core::CompactionStrategy::LayeredFallback
        && dropped_estimated_tokens < threshold
    {
        return Some(report);
    }
    let Some(model) = config.context_compaction.model_assisted_model.clone() else {
        log_session_event(
            session,
            redactor,
            "compaction_fallback",
            None,
            Some("model_assisted_model not configured; using extractive output".to_string()),
            json!({ "reason": "missing_model", "strategy": strategy.as_str() }),
        );
        return Some(report);
    };
    let max_output = config.context_compaction.model_assisted_max_output_tokens;
    let timeout_secs = config.context_compaction.model_assisted_timeout_secs;
    let extractive_summary = report.summary.clone();
    let prompt = format!(
        "Rewrite the conversation summary below verbatim in <= {max_output} tokens. \
         Keep every decision, plan, dead-end, attachment, receipt, and unresolved \
         question. Do not invent new facts. Output the summary only.\n\n{extractive_summary}"
    );
    let request = LlmRequest {
        model: Arc::from(model.as_str()),
        instructions: Arc::from(
            "You compact conversation summaries faithfully. Never add new facts; never omit decisions.",
        ),
        input: Arc::from(vec![LlmInputItem::UserText(prompt)]),
        max_output_tokens: Some(max_output),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: Arc::from(Vec::new()),
        store: false,
        cache_key: None,
    };
    let cancel = CancellationToken::new();
    let mut stream = provider.stream_response(request, cancel);
    let mut buffer = String::new();
    let collected = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        while let Some(event) = stream.next().await {
            match event {
                Ok(LlmEvent::TextDelta(delta)) => buffer.push_str(&delta),
                Ok(LlmEvent::Completed { .. }) => return Ok::<(), SqueezyError>(()),
                Ok(LlmEvent::Cancelled) => {
                    return Err(SqueezyError::Agent(
                        "model-assisted compaction cancelled".to_string(),
                    ));
                }
                Ok(_) => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(())
    })
    .await;
    let reason = match collected {
        Err(_) => "model_assisted_timeout",
        Ok(Err(err)) => {
            tracing::warn!(
                target: "squeezy::compaction",
                error = %err,
                "model-assisted compaction failed; falling back to extractive",
            );
            "model_assisted_error"
        }
        Ok(Ok(())) if buffer.trim().is_empty() => "model_assisted_empty",
        Ok(Ok(())) => {
            let new_summary = buffer.trim().to_string();
            if let Some(LlmInputItem::UserText(slot)) = conversation.first_mut() {
                *slot = new_summary.clone();
            }
            state.summary = Some(new_summary.clone());
            let mut patched_record = report.record.clone();
            patched_record.summary_bytes = new_summary.len();
            if let Some(last) = state.last.as_mut() {
                last.summary_bytes = new_summary.len();
            }
            if let Some(last) = state.history.last_mut() {
                last.summary_bytes = new_summary.len();
            }
            return Some(ContextCompactionReport {
                summary: new_summary,
                record: patched_record,
                dropped: report.dropped,
            });
        }
    };
    log_session_event(
        session,
        redactor,
        "compaction_fallback",
        None,
        Some(format!(
            "model-assisted compaction fell back to extractive ({reason})"
        )),
        json!({ "reason": reason, "strategy": strategy.as_str() }),
    );
    Some(report)
}

pub(crate) fn build_compaction_summary(
    generation: u64,
    state: &ContextCompactionState,
    older: &[LlmInputItem],
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Squeezy compacted conversation context (generation {generation})."
    ));
    lines.push(
        "Preserve these durable facts, decisions, pinned entries, seen-file receipts, and unresolved questions; do not ask for raw output already summarized here unless it is needed again."
            .to_string(),
    );
    if let Some(summary) = &state.summary {
        lines.push(format!(
            "Previous compacted summary: {}",
            compact_text(summary, COMPACTION_PREVIOUS_SUMMARY_MAX_CHARS)
        ));
    }
    if !state.pinned.is_empty() {
        lines.push("Pinned context:".to_string());
        for pin in &state.pinned {
            lines.push(format!(
                "- {} {}: {}",
                pin.id,
                pin.label,
                compact_text(&pin.summary, COMPACTION_PIN_SUMMARY_MAX_CHARS)
            ));
        }
    }
    // Cross-session observations carry decisions/conventions/dead-ends the
    // user (or a prior session) explicitly persisted via the `notes_*`
    // tools. Surface the most recent few so compaction never silently
    // discards them. Empty query falls through to a recency-ordered
    // listing inside the store.
    if let Some(store) = store
        && let Ok(recent) = store.list_recent_observations(5)
        && !recent.is_empty()
    {
        lines.push("Prior decisions and notes (notes_recall):".to_string());
        for obs in recent.iter().take(5) {
            lines.push(format!(
                "- [{}] {}",
                format!("{:?}", obs.kind).to_ascii_lowercase(),
                compact_text(&obs.text, COMPACTION_DURABLE_LINE_MAX_CHARS),
            ));
        }
    }
    let decisions = durable_context_lines(older);
    if !decisions.is_empty() {
        lines.push("Durable conversation facts and decisions:".to_string());
        lines.extend(decisions);
    }
    let unresolved = unresolved_question_lines(older);
    if !unresolved.is_empty() {
        lines.push("Unresolved questions:".to_string());
        lines.extend(unresolved);
    }
    let active_attachments = attachments
        .iter()
        .filter(|attachment| attachment.is_active())
        .collect::<Vec<_>>();
    if !active_attachments.is_empty() {
        lines.push("Active attached context:".to_string());
        for attachment in active_attachments {
            lines.push(format!(
                "- {} {} {}B preview={}",
                attachment.id,
                attachment.kind.as_str(),
                attachment.original_bytes,
                compact_text(
                    &collapse_status_text(&attachment.preview),
                    COMPACTION_ATTACHMENT_PREVIEW_MAX_CHARS
                )
            ));
        }
    }
    if let Some(receipts) = receipt_summary_lines(store) {
        lines.push("Tool/file output receipts already seen:".to_string());
        lines.extend(receipts);
    }
    lines.push(format!(
        "Compacted {} older model-visible item(s); the most recent context remains verbatim after this summary.",
        older.len()
    ));
    let summary = lines.join("\n");
    context_attachment_preview(&summary, config.context_compaction.max_summary_bytes).0
}

fn durable_context_lines(items: &[LlmInputItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::UserText(text) => {
                let compact = compact_text(text, COMPACTION_DURABLE_LINE_MAX_CHARS);
                (!compact.is_empty()).then(|| format!("- user: {compact}"))
            }
            LlmInputItem::AssistantText(text) => {
                let compact = compact_text(text, COMPACTION_DURABLE_LINE_MAX_CHARS);
                let lower = compact.to_ascii_lowercase();
                (lower.contains("decision")
                    || lower.contains("decided")
                    || lower.contains("plan")
                    || lower.contains("assumption")
                    || lower.contains("must")
                    || lower.contains("should"))
                .then(|| format!("- assistant: {compact}"))
            }
            LlmInputItem::FunctionCall {
                name, arguments, ..
            } => Some(format!(
                "- tool call {name} args={}",
                compact_text(&arguments.to_string(), COMPACTION_TOOL_ARGS_MAX_CHARS)
            )),
            LlmInputItem::FunctionCallOutput { call_id, output } => Some(format!(
                "- tool output {call_id}: {}",
                compact_text(output, COMPACTION_TOOL_OUTPUT_MAX_CHARS)
            )),
            // Reasoning items are durable context only insofar as the
            // assistant text that follows captures the conclusion; the raw
            // chain-of-thought is intentionally excluded from the summary.
            LlmInputItem::Reasoning(_) => None,
        })
        .take(COMPACTION_DURABLE_LINES_LIMIT)
        .collect()
}

fn unresolved_question_lines(items: &[LlmInputItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::UserText(text) | LlmInputItem::AssistantText(text) => Some(text),
            _ => None,
        })
        .flat_map(|text| text.lines())
        .filter(|line| line.contains('?'))
        .map(|line| {
            format!(
                "- {}",
                compact_text(&collapse_status_text(line), COMPACTION_UNRESOLVED_MAX_CHARS)
            )
        })
        .take(COMPACTION_UNRESOLVED_LINES_LIMIT)
        .collect()
}

fn receipt_summary_lines(store: Option<&SqueezyStore>) -> Option<Vec<String>> {
    let store = store?;
    let mut receipts = store.tool_receipts().ok()?;
    if receipts.is_empty() {
        return None;
    }
    receipts.sort_by_key(|receipt| std::cmp::Reverse(receipt.created_unix_millis));
    let lines = receipts
        .into_iter()
        .take(COMPACTION_RECEIPT_LINES_LIMIT)
        .map(|receipt| {
            let summary = receipt.summary.unwrap_or_else(|| {
                format!(
                    "{} output {}B sha={}",
                    receipt.tool_name, receipt.model_output_bytes, receipt.stable_output_sha256
                )
            });
            format!("- {}", compact_text(&summary, COMPACTION_RECEIPT_MAX_CHARS))
        })
        .collect::<Vec<_>>();
    Some(lines)
}

pub(crate) fn next_context_pin_id(pins: &[ContextPin]) -> String {
    let next = pins
        .iter()
        .filter_map(|pin| pin.id.strip_prefix("pin-"))
        .filter_map(|raw| raw.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    format!("pin-{next:04}")
}

#[derive(Debug, Clone)]
struct SeenToolOutput {
    call_id: String,
    tool_name: String,
    stable_output_sha256: String,
    content_sha256: Option<String>,
    model_output_bytes: usize,
    summary: Option<String>,
}

impl SeenToolOutput {
    fn from_result(result: &ToolResult) -> Self {
        Self {
            call_id: result.call_id.clone(),
            tool_name: result.tool_name.clone(),
            stable_output_sha256: stable_output_sha256(result),
            content_sha256: result.receipt.content_sha256.clone(),
            model_output_bytes: result.model_output().len(),
            summary: Some(tool_result_summary(result)),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingToolResult {
    pub(crate) result: ToolResult,
    remember: Option<SeenToolOutput>,
    same_as_current_call_id: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct SeenToolOutputs {
    by_tool_output: BTreeMap<(String, String), SeenToolOutput>,
    store: Option<Arc<SqueezyStore>>,
}

impl SeenToolOutputs {
    pub(crate) fn from_store(store: Option<Arc<SqueezyStore>>) -> Self {
        let mut outputs = Self {
            by_tool_output: BTreeMap::new(),
            store,
        };
        if let Some(store) = outputs.store.as_deref()
            && let Ok(receipts) = store.tool_receipts()
        {
            for receipt in receipts {
                let seen = SeenToolOutput {
                    call_id: receipt.call_id,
                    tool_name: receipt.tool_name,
                    stable_output_sha256: receipt.stable_output_sha256,
                    content_sha256: receipt.content_sha256,
                    model_output_bytes: receipt.model_output_bytes,
                    summary: receipt.summary,
                };
                outputs
                    .by_tool_output
                    .entry((seen.tool_name.clone(), seen.stable_output_sha256.clone()))
                    .or_insert(seen);
            }
        }
        outputs
    }

    pub(crate) fn prepare_results(&self, results: Vec<ToolResult>) -> Vec<PendingToolResult> {
        let mut prepared = Vec::with_capacity(results.len());
        let mut seen = self
            .by_tool_output
            .iter()
            .map(|(key, seen)| {
                (
                    key.clone(),
                    RoundSeenToolOutput {
                        output: seen.clone(),
                        current_round: false,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        for result in results {
            prepared.push(Self::prepare_result(result, &mut seen));
        }
        prepared
    }

    fn prepare_result(
        result: ToolResult,
        seen: &mut BTreeMap<(String, String), RoundSeenToolOutput>,
    ) -> PendingToolResult {
        if !is_receipt_stub_candidate(&result) {
            return PendingToolResult {
                result,
                remember: None,
                same_as_current_call_id: None,
            };
        }

        let key = (result.tool_name.clone(), stable_output_sha256(&result));
        if let Some(seen) = seen.get(&key) {
            return PendingToolResult {
                result: receipt_stub_result(result, &seen.output),
                remember: None,
                same_as_current_call_id: seen.current_round.then(|| seen.output.call_id.clone()),
            };
        }

        let output = SeenToolOutput::from_result(&result);
        seen.insert(
            key,
            RoundSeenToolOutput {
                output: output.clone(),
                current_round: true,
            },
        );
        PendingToolResult {
            remember: Some(output),
            result,
            same_as_current_call_id: None,
        }
    }

    pub(crate) fn remember_results(&mut self, results: &[PendingToolResult]) {
        for result in results {
            if let Some(seen) = result.remember.clone() {
                self.by_tool_output
                    .entry((seen.tool_name.clone(), seen.stable_output_sha256.clone()))
                    .or_insert(seen.clone());
                if let Some(store) = self.store.as_deref() {
                    let _ = store.put_tool_receipt(&StoredToolReceipt {
                        tool_name: seen.tool_name.clone(),
                        stable_output_sha256: seen.stable_output_sha256.clone(),
                        call_id: seen.call_id.clone(),
                        content_sha256: seen.content_sha256.clone(),
                        model_output_bytes: seen.model_output_bytes,
                        created_unix_millis: unix_millis(),
                        summary: seen.summary.clone(),
                    });
                    if let Some(snapshot) = read_snapshot_from_result(&result.result, &seen) {
                        let _ = store.put_read_snapshot(&snapshot);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RoundSeenToolOutput {
    output: SeenToolOutput,
    current_round: bool,
}

fn is_receipt_stub_candidate(result: &ToolResult) -> bool {
    result.status == ToolStatus::Success
        && matches!(
            result.tool_name.as_str(),
            "decl_search"
                | "definition_search"
                | "downstream_flow"
                | "glob"
                | "grep"
                | "hierarchy"
                | "read_file"
                | "read_slice"
                | "read_tool_output"
                | "reference_search"
                | "repo_map"
                | "symbol_context"
                | "upstream_flow"
                | "webfetch"
                | "websearch"
        )
}

fn stable_output_sha256(result: &ToolResult) -> String {
    result
        .content
        .get("cache_receipt")
        .and_then(|value| value.get("stable_output_sha256"))
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .content
                .get("original_output_sha256")
                .and_then(Value::as_str)
        })
        .unwrap_or(&result.receipt.output_sha256)
        .to_string()
}

fn read_snapshot_from_result(
    result: &ToolResult,
    seen: &SeenToolOutput,
) -> Option<StoredReadSnapshot> {
    if !matches!(result.tool_name.as_str(), "read_file" | "read_slice") {
        return None;
    }
    if result.content.get("read_mode").and_then(Value::as_str) == Some("diff") {
        return None;
    }
    let path = result.content.get("path")?.as_str()?.to_string();
    let content = result.content.get("content")?.as_str()?.to_string();
    let start_byte = result
        .content
        .get("offset")
        .and_then(Value::as_u64)
        .or_else(|| result.content.get("start_byte").and_then(Value::as_u64))
        .unwrap_or(0);
    let bytes_returned = result.content.get("bytes_returned")?.as_u64()?;
    Some(StoredReadSnapshot {
        path,
        tool_name: seen.tool_name.clone(),
        call_id: seen.call_id.clone(),
        stable_output_sha256: seen.stable_output_sha256.clone(),
        content_sha256: seen.content_sha256.clone(),
        start_byte,
        end_byte: start_byte.saturating_add(bytes_returned),
        content,
        model_output_bytes: seen.model_output_bytes,
        created_unix_millis: unix_millis(),
    })
}

fn receipt_stub_result(result: ToolResult, seen: &SeenToolOutput) -> ToolResult {
    let negative_receipt_stub = is_negative_receipt_result(&result);
    let content = json!({
        "receipt_stub": true,
        "negative_receipt_stub": negative_receipt_stub,
        "message": "identical tool output already sent to the model in this turn",
        "same_as_call_id": &seen.call_id,
        "same_as_tool_name": &seen.tool_name,
        "original_output_sha256": &seen.stable_output_sha256,
        "original_content_sha256": &seen.content_sha256,
        "original_model_output_bytes": seen.model_output_bytes,
    });
    let output_bytes = serde_json::to_vec(&content).unwrap_or_default();
    let mut cost_hint = result.cost_hint;
    cost_hint.output_bytes = output_bytes.len() as u64;
    cost_hint.truncated = true;

    ToolResult {
        call_id: result.call_id,
        tool_name: result.tool_name,
        status: result.status,
        content,
        cost_hint,
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: result.receipt.content_sha256,
        },
        spill_model_output: None,
    }
}

fn is_negative_receipt_result(result: &ToolResult) -> bool {
    match result.tool_name.as_str() {
        "grep" => {
            result
                .content
                .get("matches")
                .and_then(Value::as_array)
                .is_some_and(|items| items.is_empty())
                || result
                    .content
                    .get("paths")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.is_empty())
                || result.content.get("count").and_then(Value::as_u64) == Some(0)
        }
        "glob" => result
            .content
            .get("paths")
            .and_then(Value::as_array)
            .is_some_and(|items| items.is_empty()),
        _ => false,
    }
}

pub(crate) fn pack_tool_results(
    results: Vec<PendingToolResult>,
    budget_bytes: usize,
) -> Vec<PendingToolResult> {
    if budget_bytes == 0 {
        return results;
    }

    let mut used = 0usize;
    let mut visible_current_call_ids = BTreeSet::new();
    results
        .into_iter()
        .map(|mut pending| {
            if pending
                .same_as_current_call_id
                .as_ref()
                .is_some_and(|call_id| !visible_current_call_ids.contains(call_id))
            {
                pending.result = receipt_stub_reference_omitted(pending.result);
                pending.remember = None;
                pending.same_as_current_call_id = None;
            }

            let bytes = pending.result.model_output().len();
            if used.saturating_add(bytes) <= budget_bytes {
                used += bytes;
                if pending.remember.is_some() {
                    visible_current_call_ids.insert(pending.result.call_id.clone());
                }
                pending
            } else {
                let compact = pending
                    .result
                    .aggregate_budget_exceeded(budget_bytes, bytes);
                used = used.saturating_add(compact.model_output().len());
                PendingToolResult {
                    result: compact,
                    remember: None,
                    same_as_current_call_id: None,
                }
            }
        })
        .collect()
}

fn receipt_stub_reference_omitted(result: ToolResult) -> ToolResult {
    let content = json!({
        "error": "tool result omitted because the identical result it references was omitted by the aggregate tool-result budget",
    });
    let output_bytes = serde_json::to_vec(&content).unwrap_or_default();

    ToolResult {
        call_id: result.call_id,
        tool_name: result.tool_name,
        status: ToolStatus::Error,
        content,
        cost_hint: ToolCostHint {
            output_bytes: output_bytes.len() as u64,
            truncated: true,
            ..Default::default()
        },
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output_bytes),
            content_sha256: result.receipt.content_sha256,
        },
        spill_model_output: None,
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
