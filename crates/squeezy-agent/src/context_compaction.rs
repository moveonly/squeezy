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

#[cfg(test)]
#[path = "context_compaction_tests.rs"]
mod tests;

// Compaction summary truncation budgets — survivor policy chunks.
//
// These constants are character (not byte) caps because they pass through
// `compact_text` → `truncate_chars`. They group into four named families
// matching the structure of `build_compaction_summary`; each family bounds
// one section of the synthetic head item that replaces the dropped slice.
// A rough total budget is `previous + pin + (durable_line * durable_lines)
// + (receipt * receipts) + (unresolved * unresolved_lines) + attachment_preview
// ≈ 1200 + 400 + 320*24 + 260*12 + 240*8 + 220 ≈ 15.7K chars ≈ 3.9K tokens`,
// then bounded again by `config.context_compaction.max_summary_bytes` at the
// end of `build_compaction_summary`.
//
// Squeezy's summary is a single concatenated string rather than a set of
// post-compact attachments, so the caps below sit inside one family rather
// than across many message slots.

// --- SUMMARY_BLOCK family: prose carrying over from the prior summary chain ---

/// Cap on the previous-summary block re-inserted at the head of each new
/// summary. ≈ 300 tokens. Holds ~3 lines of "what mattered last compaction"
/// after `compact_text` strips whitespace; the chain depth across repeated
/// compactions ends up fitting comfortably under 4K chars total because each
/// generation re-truncates here.
const COMPACTION_PREVIOUS_SUMMARY_MAX_CHARS: usize = 1_200;

// --- DURABLE_FACTS family: per-item lines mined from the dropped slice ---

/// Per-line cap for durable facts (decisions, plans, assumptions, tool
/// calls). ≈ 80 tokens — wide enough to keep a one-sentence decision intact
/// without bleeding mid-paragraph text into the summary.
const COMPACTION_DURABLE_LINE_MAX_CHARS: usize = 320;
/// Per-line cap for `tool call <name> args=<json>` entries; matches the
/// shape of a typical 1–2 arg invocation after JSON whitespace collapses.
const COMPACTION_TOOL_ARGS_MAX_CHARS: usize = 260;
/// Per-line cap for `tool output <call_id>: <text>` entries. Receipts table
/// already carries the full output via sha; the line is a teaser.
const COMPACTION_TOOL_OUTPUT_MAX_CHARS: usize = 260;
/// Total lines of durable facts emitted. 24 covers a deep multi-turn
/// session that still produces ~1 useful decision/tool-call per turn before
/// auto-compact fires.
const COMPACTION_DURABLE_LINES_LIMIT: usize = 24;

// --- TOOL_RECEIPTS family: cross-session receipts from the store ---

/// Per-line cap for tool/file receipt entries in the summary. Same shape
/// as durable tool outputs above; kept symmetrical so future readers do not
/// wonder why the two diverge.
const COMPACTION_RECEIPT_MAX_CHARS: usize = 260;
/// Cap on the count of receipt lines emitted, newest-first. 12 holds
/// roughly one round of `read_file` / `grep` / `glob` against a small repo
/// without dominating the summary.
const COMPACTION_RECEIPT_LINES_LIMIT: usize = 12;

// --- UNRESOLVED + ATTACHMENT family: open questions + active attachment previews ---

/// Per-line cap for "unresolved questions" (lines containing `?`). ≈ 60
/// tokens; one open question per architecture layer in a typical 3-tier
/// app keeps the section readable.
const COMPACTION_UNRESOLVED_MAX_CHARS: usize = 240;
/// Maximum unresolved questions surfaced; 8 lines floor matches the
/// per-layer budget above.
const COMPACTION_UNRESOLVED_LINES_LIMIT: usize = 8;
/// Cap on attachment preview text. ≈ 55 tokens — just the first paragraph
/// of an attached file/output so the model can recall its presence and
/// re-request the full body via `read_file` if needed.
const COMPACTION_ATTACHMENT_PREVIEW_MAX_CHARS: usize = 220;

// --- FILE_LINEAGE family: per-session path map of read vs modified files ---

/// Per-list cap on the `<read-files>` / `<modified-files>` blocks
/// appended to the summary. Pi (the reference) emits unbounded sets;
/// Squeezy adds a hard ceiling so a long session that touches hundreds
/// of files cannot blow past the summary budget. 50 covers the working
/// set of a typical multi-turn debugging or refactor session. When a
/// list overflows the cap the *chronologically oldest* entries are
/// dropped first — the older slice is walked in order so the most
/// recent file touches survive.
///
/// The XML-tag shape (`<read-files>` / `<modified-files>`) stays stable
/// so a swap-in summarizer can re-extract the lists with the same
/// regex.
const COMPACTION_FILE_LINEAGE_LIMIT: usize = 50;

// --- STATE retention (non-summary) ---

/// In-memory history of compaction records retained on
/// `ContextCompactionState.history`. 20 entries is enough to render a
/// session-timeline UI without unbounded growth across long sessions; older
/// entries fall off via `drain(..excess)`.
const COMPACTION_MAX_HISTORY: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCompactionReport {
    pub record: ContextCompactionRecord,
    pub summary: String,
    /// Pre-compaction conversation slice (the items removed by the
    /// compaction pass). Persisted via the undo-checkpoint write so a
    /// future `compact_context_undo` can restore them verbatim. Not
    /// stamped into the session-event `conversation` field — that field
    /// carries `post_compact` so replay snaps to the post-compact base.
    pub dropped: Vec<ResumeItem>,
    /// Post-compaction conversation (summary head + kept recent items).
    /// Stamped into the `context_compacted` session event's
    /// `conversation` field so `replay_resume_state` snaps to the
    /// correct post-compact checkpoint and forward-replays only the
    /// strictly-newer events.
    pub post_compact: Vec<ResumeItem>,
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
    // Replace embedded base64 image/document data URIs in tool outputs
    // with `[image]`/`[document]` markers before any summarizer (extractive
    // or model-assisted) reads them. The summary feeds the model-assisted
    // prompt at `compact_conversation_with_strategy`, and chains forward
    // into the next compaction's `Previous compacted summary` block; a
    // base64 PNG that survives `compact_text` truncation can otherwise
    // bloat the summary chain and push the compaction API call itself
    // toward prompt-too-long. The substitution is local to the summary
    // input; the persisted `dropped` checkpoint still receives `older`
    // verbatim so undo restores the original bytes.
    let older_for_summary = strip_media_for_compaction(&older);
    let summary = build_compaction_summary(
        generation,
        state,
        &older_for_summary,
        attachments,
        store,
        config,
    );
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
    let post_compact: Vec<squeezy_store::ResumeItem> = conversation
        .iter()
        .cloned()
        .map(llm_input_to_resume_item)
        .collect();
    Some(ContextCompactionReport {
        record,
        summary,
        dropped,
        post_compact,
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
    let declared_in_older: BTreeSet<&str> = conversation[..initial_split]
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    while split < conversation.len() {
        match &conversation[split] {
            LlmInputItem::FunctionCallOutput { call_id, .. } => {
                if declared_in_older.contains(call_id.as_str()) {
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
    let declared: BTreeSet<String> = items
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    items
        .into_iter()
        .filter(|item| match item {
            LlmInputItem::FunctionCallOutput { call_id, .. } => declared.contains(call_id.as_str()),
            _ => true,
        })
        .collect()
}

/// Insert a synthetic `FunctionCallOutput` after any `FunctionCall`
/// whose `call_id` is never answered by a later `FunctionCallOutput`.
/// Mirrors `drop_orphan_function_call_outputs` in the opposite direction:
/// a cancel mid-tool-call, an executor panic, or an externally-recorded
/// resume tape can leave a bare `FunctionCall` in the conversation; the
/// Anthropic Messages API then rejects the whole turn with
/// *"tool_use blocks must be followed by a tool_result"* and the failure
/// is sticky until `/clear`. Order is preserved.
pub(crate) fn repair_orphan_function_calls(items: Vec<LlmInputItem>) -> Vec<LlmInputItem> {
    let answered: BTreeSet<String> = items
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCallOutput { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let mut repaired = Vec::with_capacity(items.len());
    for item in items.iter() {
        repaired.push(item.clone());
        if let LlmInputItem::FunctionCall { call_id, .. } = item
            && !answered.contains(call_id.as_str())
        {
            repaired.push(LlmInputItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output: "{\"error\":\"tool call interrupted\",\"is_error\":true}".to_string(),
                content_parts: None,
                is_error: true,
            });
        }
    }
    repaired
}

/// Substring marker the summarizer should see in place of a stripped
/// image data URI. A short opaque token keeps the model from trying to
/// reason about the stripped bytes while still preserving the slot.
const IMAGE_DATA_URI_PLACEHOLDER: &str = "[image]";
/// Same, for PDF or other document data URIs.
const DOCUMENT_DATA_URI_PLACEHOLDER: &str = "[document]";
/// Skip scanning outputs shorter than this. A real base64 data URI prefix
/// (`data:image/png;base64,` alone is 22 bytes) cannot fit a meaningful
/// payload below this threshold; scanning every short tool output would
/// be pure overhead.
const STRIP_MEDIA_MIN_LEN: usize = 100;

/// Replace `data:image/<png|jpg|jpeg|gif|webp|bmp|svg+xml>;base64,...` and
/// `data:application/pdf;base64,...` substrings inside every
/// `FunctionCallOutput.output` with a short placeholder before the slice
/// reaches a summarizer (extractive or model-assisted). Returns a fresh
/// `Vec<LlmInputItem>`; the input is not mutated. Items other than
/// `FunctionCallOutput` are cloned through unchanged.
///
/// The strip is prophylactic. Squeezy's `LlmInputItem` is text-only today,
/// but Anthropic Vision, Gemini, and most MCP browser/screenshot tools
/// deliver images on-wire as base64 data URIs inside a tool result string.
/// Carrying that string through `build_compaction_summary` and onward to
/// the model-assisted summarizer is the failure mode CC-1180 catalogues:
/// the compaction request itself hits prompt-too-long because a 200-300 KB
/// PNG was inlined verbatim. We strip before the summarizer reads.
pub(crate) fn strip_media_for_compaction(items: &[LlmInputItem]) -> Vec<LlmInputItem> {
    items
        .iter()
        .map(|item| match item {
            LlmInputItem::FunctionCallOutput {
                call_id,
                output,
                content_parts,
                is_error,
            } => {
                // A `ToolResultPart::Image` carries raw image bytes that
                // `strip_media_data_uris` (which scans the text `output`)
                // never sees, so an unstripped array slot would smuggle a
                // full screenshot into the compaction payload. Replace each
                // image part with a short text placeholder; this mirrors the
                // `[image]` data-URI substitution and keeps the slot count
                // stable. Text parts are scrubbed for inline data URIs too.
                let stripped_parts = content_parts
                    .as_ref()
                    .map(|parts| strip_media_content_parts(parts));
                // Skip the text scan only when both the `output` string is
                // too short to hold a data URI *and* there were no parts to
                // shrink; otherwise rebuild the item with the cleaned parts.
                if output.len() < STRIP_MEDIA_MIN_LEN && stripped_parts.is_none() {
                    item.clone()
                } else {
                    LlmInputItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: if output.len() < STRIP_MEDIA_MIN_LEN {
                            output.clone()
                        } else {
                            strip_media_data_uris(output)
                        },
                        content_parts: stripped_parts,
                        is_error: *is_error,
                    }
                }
            }
            _ => item.clone(),
        })
        .collect()
}

/// Placeholder substituted for a stripped `ToolResultPart::Image` so the
/// summarizer still sees that a tool returned an image without carrying the
/// raw bytes through compaction.
const IMAGE_PART_PLACEHOLDER: &str = "[image]";

/// Shrink a structured tool-result array for compaction: drop each
/// `ToolResultPart::Image`'s raw bytes (replacing it with a short text
/// placeholder) and scrub inline data URIs out of every text part. The
/// input is borrowed; a fresh `Vec` is returned. Empty inputs collapse to
/// an empty `Vec`, preserving the `Some(_)` shape so the caller's slot
/// rebuild stays unconditional.
fn strip_media_content_parts(
    parts: &[squeezy_llm::ToolResultPart],
) -> Vec<squeezy_llm::ToolResultPart> {
    parts
        .iter()
        .map(|part| match part {
            squeezy_llm::ToolResultPart::Text { text } => squeezy_llm::ToolResultPart::Text {
                text: strip_media_data_uris(text),
            },
            squeezy_llm::ToolResultPart::Image { .. } => squeezy_llm::ToolResultPart::Text {
                text: IMAGE_PART_PLACEHOLDER.to_string(),
            },
        })
        .collect()
}

/// Replace every `data:<mime>;base64,<payload>` substring in `text` with
/// a short placeholder. The base64 payload is scanned greedily over the
/// standard base64 alphabet (`A-Za-z0-9+/=`); the scan stops at the first
/// character outside that alphabet, so trailing prose survives intact.
fn strip_media_data_uris(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(prefix_end) = match_data_uri_prefix(bytes, i) {
            let payload_end = scan_base64_payload(bytes, prefix_end);
            // Pick the placeholder based on the MIME family. Anything not
            // `application/...` is treated as an image-class media block;
            // PDFs and other application/* documents are tagged separately
            // so downstream summarisers can tell them apart.
            let placeholder = if bytes[i..prefix_end].starts_with(b"data:application/") {
                DOCUMENT_DATA_URI_PLACEHOLDER
            } else {
                IMAGE_DATA_URI_PLACEHOLDER
            };
            out.push_str(placeholder);
            i = payload_end;
        } else {
            // Push a single UTF-8 char so we never split a multi-byte
            // scalar. `bytes` is borrowed from `text`, so `text[i..]` is
            // always at a valid char boundary on entry to this branch.
            let ch = text[i..].chars().next().expect("non-empty remainder");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// If `bytes[start..]` begins with a `data:<mime>;base64,` prefix, return
/// the index just past the comma; otherwise return `None`. The `<mime>`
/// segment must be ASCII without whitespace and contain a `/`.
fn match_data_uri_prefix(bytes: &[u8], start: usize) -> Option<usize> {
    const HEAD: &[u8] = b"data:";
    if !bytes[start..].starts_with(HEAD) {
        return None;
    }
    let mut idx = start + HEAD.len();
    let mut saw_slash = false;
    while idx < bytes.len() {
        match bytes[idx] {
            b';' => break,
            b'/' => {
                saw_slash = true;
                idx += 1;
            }
            c if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.') => {
                idx += 1;
            }
            _ => return None,
        }
    }
    if !saw_slash {
        return None;
    }
    const TAIL: &[u8] = b";base64,";
    if bytes[idx..].starts_with(TAIL) {
        Some(idx + TAIL.len())
    } else {
        None
    }
}

/// Advance past the base64 payload that follows a `;base64,` marker. The
/// scan terminates at the first character outside the standard base64
/// alphabet (`A-Za-z0-9+/=`) or at end-of-input.
fn scan_base64_payload(bytes: &[u8], start: usize) -> usize {
    let mut idx = start;
    while idx < bytes.len() {
        let c = bytes[idx];
        if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=') {
            idx += 1;
        } else {
            break;
        }
    }
    idx
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
        LlmInputItem::FunctionCallOutput {
            call_id,
            output,
            content_parts,
            ..
        } => {
            // Bill the structured-result array too: a `Text` part's chars and
            // an `Image` part's raw bytes are on-the-wire payload that the
            // bare `output` string never accounts for, so without this they
            // stay invisible to the context-pressure signal.
            let parts_bytes = content_parts
                .as_ref()
                .map(|parts| {
                    parts.iter().fold(0usize, |acc, part| {
                        let part_len = match part {
                            squeezy_llm::ToolResultPart::Text { text } => text.len(),
                            squeezy_llm::ToolResultPart::Image { media_type, bytes } => {
                                media_type.len() + bytes.len()
                            }
                        };
                        acc.saturating_add(part_len)
                    })
                })
                .unwrap_or(0);
            call_id.len() + output.len() + parts_bytes
        }
        LlmInputItem::Reasoning(payload) => payload.display_text().len(),
        // Image bytes don't consume model context tokens directly (the
        // provider's vision encoder charges its own per-image token
        // budget). Bill the raw byte count here so compaction's "context
        // pressure" signal still reflects payload size on the wire.
        LlmInputItem::Image { bytes, .. } => bytes.len(),
        // Documents follow the same wire-billing rule as images: count
        // the raw payload bytes so compaction sees pressure when the
        // user attaches a large PDF.
        LlmInputItem::Document { bytes, .. } => bytes.len(),
        // `LlmInputItem` is `#[non_exhaustive]`; an unknown future variant
        // contributes zero bytes to the heuristic until a dedicated arm
        // exists. Compaction will still fire on the items it understands.
        _ => 0,
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
    // Capture the prior compaction's summary BEFORE `compact_conversation`
    // overwrites `state.summary` with the new extractive blob. The
    // structured-template prompt surfaces this prior chain as a separate
    // `<previous-summary>` block so the model can update slot contents
    // iteratively instead of re-truncating the entire summary every round.
    // Without this capture the model only sees the prior summary embedded
    // inline in the new extractive output — the same chained-truncate
    // shape that loses ~60% of high-signal slots after a handful of
    // compactions (F12-pi-iterative-summary-update).
    let previous_summary_before = state.summary.clone();
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
    let Some(model) = config
        .context_compaction
        .model_assisted_model
        .clone()
        .or_else(|| config.resolved_small_fast_model())
    else {
        log_session_event(
            session,
            redactor,
            "compaction_fallback",
            None,
            Some(
                "model_assisted_model not configured and no small_fast_model default; \
                 using extractive output"
                    .to_string(),
            ),
            json!({ "reason": "missing_model", "strategy": strategy.as_str() }),
        );
        return Some(report);
    };
    let max_output = config.context_compaction.model_assisted_max_output_tokens;
    let timeout_secs = config.context_compaction.model_assisted_timeout_secs;
    let extractive_summary = report.summary.clone();
    let prompt = build_structured_compaction_prompt(
        previous_summary_before.as_deref(),
        &extractive_summary,
        max_output,
    );
    let request = LlmRequest {
        model: Arc::from(model.as_str()),
        instructions: Arc::from(STRUCTURED_COMPACTION_SYSTEM_PROMPT),
        input: Arc::from(vec![LlmInputItem::UserText(prompt)]),
        max_output_tokens: Some(max_output),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: Arc::from(Vec::new()),
        store: false,
        cache_key: None,
        cache: squeezy_llm::CacheSpec::default(),
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
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
        Ok(Ok(())) if !is_structured_compaction_summary(buffer.trim()) => {
            // The model returned text but it does not carry all four
            // required slots (`## Goal`, `## Progress`, `## Decisions`,
            // `## Next`). A partial output is strictly worse than the
            // deterministic extractive baseline because slot detection
            // upstream — and the file-lineage append pass — both rely on
            // the named-section shape. Fall back verbatim.
            "model_assisted_missing_slots"
        }
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
            // The post-compact head is the summary slot; reflect the
            // model-assisted rewrite in the report so the persisted
            // checkpoint matches the in-memory conversation.
            let mut post_compact = report.post_compact;
            if let Some(ResumeItem::UserText { text }) = post_compact.first_mut() {
                *text = new_summary.clone();
            }
            return Some(ContextCompactionReport {
                summary: new_summary,
                record: patched_record,
                dropped: report.dropped,
                post_compact,
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

/// System prompt for the model-assisted compaction call. Pinned to the
/// "compact a context checkpoint" framing so the model never tries to
/// continue the embedded conversation, and so the four-slot output shape
/// stays stable across calls and providers.
pub(crate) const STRUCTURED_COMPACTION_SYSTEM_PROMPT: &str = "You compact conversation context into a structured checkpoint. \
Update the existing summary in place — preserve every prior decision, \
progress entry, and next-step. Never invent new facts. Output only the \
four required sections in this exact order: `## Goal`, `## Progress`, \
`## Decisions`, `## Next`.";

/// Slot headers the model-assisted compaction output MUST carry. The
/// names are kept short and lowercase here for case-insensitive matching;
/// see `is_structured_compaction_summary` for the detection contract.
/// File-lineage tags (`<read-files>` / `<modified-files>`) are appended
/// by a sibling pass below `## Next`; they intentionally sit outside
/// this slot set so the file-lineage pass can land without conflict.
const REQUIRED_COMPACTION_SLOTS: [&str; 4] = ["goal", "progress", "decisions", "next"];

/// Build the model-assisted compaction prompt. Asks the model to emit
/// four named slots — `## Goal`, `## Progress`, `## Decisions`,
/// `## Next` — that survive across N compactions. The legacy "rewrite
/// this summary verbatim" prompt chain-truncated the same blob every
/// round and lost roughly 60% of high-signal content after a handful of
/// generations (audit finding `F12-pi-iterative-summary-update`);
/// pinning the model to a fixed slot shape gives it an explicit
/// "preserve these" target instead.
///
/// When the caller has a prior compaction summary it is surfaced as a
/// dedicated `<previous-summary>` block alongside the freshly built
/// extractive output (`<new-conversation>`). The model updates the
/// slots iteratively: carry forward every entry from the prior block,
/// fold in new actions/decisions from the new conversation, and emit
/// the merged result. The instructions are deterministic-ish — the
/// section order, names, and rules are pinned text, even though the
/// model's exact wording inside each slot will vary.
fn build_structured_compaction_prompt(
    previous_summary: Option<&str>,
    new_conversation: &str,
    max_output_tokens: u32,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "Update the structured project context checkpoint below. Emit only the four \
         sections in this EXACT order: `## Goal`, `## Progress`, `## Decisions`, `## Next`.\n\n",
    );
    prompt.push_str("<new-conversation>\n");
    prompt.push_str(new_conversation);
    if !new_conversation.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("</new-conversation>\n\n");
    if let Some(prev) = previous_summary
        && !prev.trim().is_empty()
    {
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(prev);
        if !prev.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("</previous-summary>\n\n");
    }
    prompt.push_str(
        "Template:\n\n\
         ## Goal\n\
         <one-paragraph statement of what the user is trying to accomplish>\n\n\
         ## Progress\n\
         - <what's been done; preserve prior items and append newly completed actions, decisions, file edits>\n\n\
         ## Decisions\n\
         - <decisions made — chosen approach, rejected options, constraints discovered; preserve every prior decision>\n\n\
         ## Next\n\
         - <remaining steps to complete the task; update based on new progress>\n\n",
    );
    prompt.push_str(&format!(
        "Rules:\n\
         - PRESERVE every entry from `<previous-summary>` unless `<new-conversation>` explicitly invalidates it.\n\
         - ADD new entries from `<new-conversation>` into the matching slot.\n\
         - UPDATE the `## Next` slot to drop steps that `<new-conversation>` shows are complete; add steps it surfaces as outstanding.\n\
         - KEEP exact file paths, function names, error messages, tool call names, and SHA prefixes verbatim.\n\
         - Do NOT invent new facts. Do NOT omit prior decisions.\n\
         - Token budget: <= {max_output_tokens} tokens.\n\
         - Output only the four sections (`## Goal`, `## Progress`, `## Decisions`, `## Next`). No preamble, no commentary, no trailing prose.\n"
    ));
    prompt
}

/// Verify a model-assisted compaction output carries every required
/// structured slot. Returns `true` when all of `## Goal`, `## Progress`,
/// `## Decisions`, and `## Next` are present as markdown headings; the
/// caller falls back to the extractive summary verbatim otherwise.
///
/// Detection is intentionally lenient: any markdown heading line (one or
/// more leading `#` characters) that contains the slot keyword as a
/// whole word counts. This accepts model variations like `### Goal`,
/// `## Key Decisions`, `## Next Steps`, and `## Goal:` while still
/// catching outputs that drop a section entirely (which is the failure
/// mode the structured template exists to prevent). Sibling passes may
/// append additional XML-tagged sections (e.g. `<read-files>`,
/// `<modified-files>`) below `## Next` without breaking this check —
/// the validator only cares that the four slots are present, not that
/// the document ends with them.
fn is_structured_compaction_summary(text: &str) -> bool {
    let mut found = [false; REQUIRED_COMPACTION_SLOTS.len()];
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            continue;
        }
        let body = trimmed.trim_start_matches('#').trim().to_ascii_lowercase();
        for (idx, keyword) in REQUIRED_COMPACTION_SLOTS.iter().enumerate() {
            if found[idx] {
                continue;
            }
            if body
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word == *keyword)
            {
                found[idx] = true;
            }
        }
    }
    found.iter().all(|f| *f)
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
    let mut active_attachments = attachments
        .iter()
        .filter(|attachment| attachment.is_active())
        .peekable();
    if active_attachments.peek().is_some() {
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
    // File lineage blocks are emitted last so they survive cleanly when a
    // structured summary template (## Goal / ## Progress / ## Next, …)
    // lands above. Sibling finding F12-pi-iterative-summary-update is
    // expected to introduce that template; this commit is stack-safe
    // either way because the blocks are simply tacked onto the final
    // line list.
    lines.extend(file_lineage_blocks(older, state.summary.as_deref()));
    let summary = lines.join("\n");
    context_attachment_preview(&summary, config.context_compaction.max_summary_bytes).0
}

/// Build the `<read-files>` / `<modified-files>` block pair that pi emits at
/// the end of every compaction summary (see
/// `others/pi/packages/coding-agent/src/core/compaction/utils.ts:62-82`).
///
/// Inputs:
/// - `older`: the dropped conversation slice; walked in chronological
///   order so `oldest-dropped` semantics work when the per-list cap fires.
/// - `previous_summary`: the prior compaction's summary text. Lineage
///   that survives across compactions is recovered by re-parsing the
///   `<read-files>` / `<modified-files>` blocks out of that string. The
///   alternative — adding new fields to `ContextCompactionState` — would
///   force a redb schema bump and a session-replay migration for one
///   isolated piece of derived metadata.
///
/// Rules:
/// - A file appearing in both read- and modify-class tool calls is
///   reported only under `<modified-files>` (modification dominates).
/// - Paths are emitted alphabetically and de-duplicated.
/// - Each list is capped at `COMPACTION_FILE_LINEAGE_LIMIT`; when the
///   cap fires, the chronologically oldest paths are dropped (head of
///   the chronological vec) before sorting.
///
/// Returns 0, 1, or 2 lines depending on which sets ended up non-empty.
/// The caller `.extend()`s the result into the summary line list.
fn file_lineage_blocks(older: &[LlmInputItem], previous_summary: Option<&str>) -> Vec<String> {
    let mut read = Vec::<String>::new();
    let mut modified = Vec::<String>::new();
    let mut read_set = BTreeSet::<String>::new();
    let mut modified_set = BTreeSet::<String>::new();

    // Carry forward the prior summary's lineage so the chain accumulates
    // across compaction generations. Prior paths appear chronologically
    // *before* the current `older` slice, so we seed them first.
    if let Some(previous) = previous_summary {
        for path in parse_file_lineage_block(previous, "read-files") {
            if read_set.insert(path.clone()) {
                read.push(path);
            }
        }
        for path in parse_file_lineage_block(previous, "modified-files") {
            if modified_set.insert(path.clone()) {
                modified.push(path);
            }
        }
    }

    for item in older {
        let LlmInputItem::FunctionCall {
            name, arguments, ..
        } = item
        else {
            continue;
        };
        match classify_file_tool(name) {
            FileOpClass::Read => {
                visit_tool_paths(name, arguments, |path| {
                    push_unique_path(&mut read, &mut read_set, path);
                });
            }
            FileOpClass::Modified => {
                visit_tool_paths(name, arguments, |path| {
                    push_unique_path(&mut modified, &mut modified_set, path);
                });
            }
            FileOpClass::None => {}
        }
    }

    // Modification dominates: a file that was both read and modified is
    // reported only in `<modified-files>` to avoid double-listing.
    read.retain(|path| !modified_set.contains(path));

    if read.len() > COMPACTION_FILE_LINEAGE_LIMIT {
        let excess = read.len() - COMPACTION_FILE_LINEAGE_LIMIT;
        read.drain(0..excess);
    }
    if modified.len() > COMPACTION_FILE_LINEAGE_LIMIT {
        let excess = modified.len() - COMPACTION_FILE_LINEAGE_LIMIT;
        modified.drain(0..excess);
    }

    read.sort();
    modified.sort();

    let mut blocks = Vec::with_capacity(2);
    if !read.is_empty() {
        blocks.push(file_lineage_block("read-files", &read));
    }
    if !modified.is_empty() {
        blocks.push(file_lineage_block("modified-files", &modified));
    }
    blocks
}

fn push_unique_path(paths: &mut Vec<String>, seen: &mut BTreeSet<String>, path: &str) {
    if !seen.contains(path) {
        let path = path.to_string();
        seen.insert(path.clone());
        paths.push(path);
    }
}

fn file_lineage_block(tag: &str, paths: &[String]) -> String {
    let paths_len = paths.iter().map(String::len).sum::<usize>();
    let separators = paths.len().saturating_sub(1);
    let mut block = String::with_capacity(tag.len() * 2 + paths_len + separators + 7);
    block.push('<');
    block.push_str(tag);
    block.push_str(">\n");
    for (index, path) in paths.iter().enumerate() {
        if index > 0 {
            block.push('\n');
        }
        block.push_str(path);
    }
    block.push_str("\n</");
    block.push_str(tag);
    block.push('>');
    block
}

#[derive(Debug, Clone, Copy)]
enum FileOpClass {
    Read,
    Modified,
    None,
}

fn classify_file_tool(name: &str) -> FileOpClass {
    // The classification mirrors `permission_scope_for` in
    // `crates/squeezy-tools/src/lib.rs`: `Read` scope tools that target a
    // single file land in the read set; `Edit` scope tools that mutate
    // bytes land in the modified set. Search-class tools (grep, glob)
    // do *not* target a specific file — their `path` argument is a
    // starting directory — so they are intentionally excluded.
    match name {
        "read_file" | "read_slice" => FileOpClass::Read,
        "write_file" | "notebook_edit" | "apply_patch" => FileOpClass::Modified,
        _ => FileOpClass::None,
    }
}

/// Pull every workspace-relative file path out of a tool call's JSON
/// arguments. Only tools known to `classify_file_tool` should reach this
/// function; for `apply_patch` we also walk both the legacy `patches[]`
/// shape and the modern `operations[]` shape (including `MoveFile`'s
/// `from`/`to` pair) so the modified set is exhaustive.
fn visit_tool_paths(name: &str, arguments: &Value, mut visit: impl FnMut(&str)) {
    if let Some(path) = arguments.get("path").and_then(Value::as_str) {
        visit(path);
    }
    if name == "apply_patch" {
        if let Some(patches) = arguments.get("patches").and_then(Value::as_array) {
            for entry in patches {
                if let Some(path) = entry.get("path").and_then(Value::as_str) {
                    visit(path);
                }
            }
        }
        if let Some(ops) = arguments.get("operations").and_then(Value::as_array) {
            for op in ops {
                if let Some(path) = op.get("path").and_then(Value::as_str) {
                    visit(path);
                }
                if let Some(from) = op.get("from").and_then(Value::as_str) {
                    visit(from);
                }
                if let Some(to) = op.get("to").and_then(Value::as_str) {
                    visit(to);
                }
            }
        }
    }
}

/// Pull the line list out of the `<tag>...</tag>` block in
/// `summary`. Returns an empty vec when the block is missing, empty, or
/// malformed. The matcher is substring-based, not XML-parsed: pi emits
/// these tags verbatim, never nests them, and never wraps them in any
/// other markup, so a substring match is faithful and avoids dragging
/// in an XML dependency for two well-known tags.
fn parse_file_lineage_block(summary: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(open_pos) = summary.find(&open) else {
        return Vec::new();
    };
    let body_start = open_pos + open.len();
    let Some(close_rel) = summary[body_start..].find(&close) else {
        return Vec::new();
    };
    let body = &summary[body_start..body_start + close_rel];
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
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
            LlmInputItem::FunctionCallOutput {
                call_id, output, ..
            } => Some(format!(
                "- tool output {call_id}: {}",
                compact_text(output, COMPACTION_TOOL_OUTPUT_MAX_CHARS)
            )),
            // Reasoning items are durable context only insofar as the
            // assistant text that follows captures the conclusion; the raw
            // chain-of-thought is intentionally excluded from the summary.
            LlmInputItem::Reasoning(_) => None,
            // Image attachments don't carry summarisable text; mention the
            // MIME type so the summary preserves a hint that an image was
            // shown but skip the raw bytes.
            LlmInputItem::Image { media_type, .. } => Some(format!("- user image: {media_type}")),
            // Document attachments are similar: keep a one-line hint
            // (filename + MIME) so the summary records the upload, drop
            // the raw bytes.
            LlmInputItem::Document {
                name, media_type, ..
            } => Some(format!("- user document {name}: {media_type}")),
            // Unknown future variants contribute nothing to the durable
            // summary — preserves forward compatibility without polluting
            // the summary with an opaque placeholder.
            _ => None,
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
