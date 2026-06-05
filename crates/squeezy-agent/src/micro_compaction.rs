//! Mid-tier "micro" compaction: rewrite older `FunctionCallOutput` bodies
//! to a structured placeholder while leaving the surrounding
//! `FunctionCall`/`FunctionCallOutput` wrapper structure intact. The full
//! compaction tier in `context_compaction.rs` is all-or-nothing — it
//! replaces N older items with a single synthetic summary head — so once
//! it fires the model loses individual tool-call shape for the dropped
//! slice. Micro-compaction sits between the no-op tier and that nuclear
//! tier: at ~60% of the configured token window it reclaims the heavy
//! tool-result bodies (long shell stdout, large file reads, web fetches)
//! while preserving the call-id pairings the provider needs to keep the
//! conversation well-formed. The model still sees which tools ran in what
//! order; only the bulky payload is replaced.
//!
//! Compactable tools are a closed set: only the tools that plausibly emit
//! large outputs (file reads, shell, search, web, and the graph navigation
//! tools whose packets carry full symbol/span payloads) are in scope. Tools
//! that already return receipt-stubbed or otherwise small payloads
//! (`notes_recall`, `checkpoint_*`, MCP control calls) are intentionally
//! excluded — there is nothing worth clearing.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use squeezy_core::AppConfig;
use squeezy_llm::LlmInputItem;

use crate::context_compaction::estimate_context;

/// Tools whose `FunctionCallOutput.output` carries a snapshot of file P's
/// content (or a slice/grep over P) that an in-place edit to P can render
/// stale. Narrower than [`COMPACTABLE_TOOL_NAMES`]: only the read/search
/// observations whose body literally embeds the pre-edit text are eligible
/// for expired-context masking. Edit tools (`apply_patch`, `write_file`)
/// and bulk shell/web outputs are excluded — they are not point-in-time
/// views of a single file's source we can substring-match an edit against.
pub(crate) const EXPIRABLE_READ_TOOL_NAMES: &[&str] = &["read_file", "read_slice", "grep"];

/// A successful in-place mutation of file `path`. `changed_spans` is the
/// set of *old* (pre-edit) text fragments that the edit removed or
/// rewrote — for a search/replace patch these are the `search` strings,
/// which are exactly the bytes that no longer exist in P after the write.
/// `whole_file` marks a full-file overwrite (`write_file`), where every
/// prior snapshot of P is stale and no sub-span scoping is possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SuccessfulEdit {
    pub(crate) path: String,
    pub(crate) changed_spans: Vec<String>,
    pub(crate) whole_file: bool,
}

/// Result of an expired-context masking pass (idea M2). `masked_call_ids`
/// reports the read/grep observations whose stale spans were rewritten in
/// place; `bytes_saved` is the net byte reduction. Conversation length and
/// every `FunctionCall`/`FunctionCallOutput` pairing are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredContextReport {
    pub masked_call_ids: Vec<String>,
    pub bytes_saved: usize,
    pub spans_masked: usize,
}

/// Tools whose `FunctionCallOutput.output` payload may be rewritten to a
/// placeholder once the conversation crosses the micro-compaction
/// threshold. Covers tools that can plausibly emit large outputs
/// (file/shell/search/web) plus the graph navigation tools — a single
/// `definition_search`/`symbol_context`/`hierarchy` packet routinely runs
/// tens of KB of symbol, span, and signature data, and once emitted it
/// pins into context and is re-billed on every subsequent prefill of the
/// turn. Those packets are exactly the kind of stale bulk this tier exists
/// to reclaim; `micro_compaction_keep_recent` keeps the newest result
/// verbatim, and the placeholder preserves the call-id so the model can
/// re-issue the lookup if it still needs the detail. Tools whose outputs
/// are already capped by receipts or aggregate budgets stay out of the set.
pub(crate) const COMPACTABLE_TOOL_NAMES: &[&str] = &[
    "read_file",
    "read_slice",
    "shell",
    "grep",
    "glob",
    "webfetch",
    "websearch",
    "apply_patch",
    "write_file",
    // Graph navigation packets (mirror `is_graph_navigation_tool`); these
    // carry the heaviest structured payloads in a code-audit turn.
    "repo_map",
    "decl_search",
    "definition_search",
    "reference_search",
    "upstream_flow",
    "downstream_flow",
    "symbol_context",
    "hierarchy",
];

/// Placeholder substituted for cleared `FunctionCallOutput.output` bodies.
/// The string carries enough metadata (`call_id`, tool `name`, original
/// `bytes`) for the model to look back up the matching `FunctionCall` and
/// re-issue it if the result is still needed. Keep the prefix stable so
/// downstream consumers can detect already-cleared outputs.
pub(crate) const MICRO_COMPACT_CLEARED_PREFIX: &str = "[Old tool output cleared";

/// Result of a micro-compaction pass. `cleared_call_ids` reports the
/// `FunctionCallOutput` payloads that were rewritten in place;
/// `bytes_saved` is the sum of `output.len()` removed from the
/// conversation. The conversation length is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicroCompactionReport {
    pub cleared_call_ids: Vec<String>,
    pub bytes_saved: usize,
    pub before_estimated_tokens: u64,
    pub after_estimated_tokens: u64,
}

/// Run a mid-turn micro-compaction pass when the configured token window
/// is approaching pressure. Returns `Some(report)` when at least one
/// `FunctionCallOutput` body was rewritten; `None` when the feature is
/// disabled, the window isn't configured, the threshold hasn't been
/// crossed, or there's nothing left to clear.
///
/// The placeholder rewrite is *in place*: `conversation.len()` does not
/// change, every `FunctionCall` still pairs with its `FunctionCallOutput`
/// by `call_id`. Downstream `compact_conversation` (the full tier) sees
/// the cleared bodies as already-shrunk text and can decide on its own
/// whether to fold them into a summary head.
pub(crate) fn maybe_micro_compact_mid_turn(
    conversation: &mut [LlmInputItem],
    config: &AppConfig,
    last_total_tokens: Option<u64>,
) -> Option<MicroCompactionReport> {
    let cc = &config.context_compaction;
    if !cc.enabled_mid_turn || !cc.micro_compaction_enabled {
        return None;
    }
    let threshold = cc.mid_turn_micro_threshold()?;
    let before = estimate_context(conversation);
    let observed = last_total_tokens.unwrap_or(before.estimated_tokens);
    if observed < threshold {
        return None;
    }

    let clear_targets: BTreeMap<String, String> = {
        let compactable_ids = collect_compactable_call_ids(conversation);
        if compactable_ids.len() <= cc.micro_compaction_keep_recent {
            return None;
        }
        let keep_recent = cc.micro_compaction_keep_recent.max(1);
        let clear_count = compactable_ids.len().saturating_sub(keep_recent);
        compactable_ids
            .iter()
            .take(clear_count)
            .map(|(call_id, name)| ((*call_id).to_string(), (*name).to_string()))
            .collect()
    };

    let mut cleared_call_ids = Vec::with_capacity(clear_targets.len());
    let mut bytes_saved: usize = 0;
    for item in conversation.iter_mut() {
        if let LlmInputItem::FunctionCallOutput {
            call_id, output, ..
        } = item
            && let Some(tool_name) = clear_targets.get(call_id.as_str())
            && !is_already_cleared(output)
        {
            let original_bytes = output.len();
            let replacement = format!(
                "{prefix} — call_id={call_id}, name={tool_name}, original_bytes={original_bytes}]",
                prefix = MICRO_COMPACT_CLEARED_PREFIX,
            );
            bytes_saved =
                bytes_saved.saturating_add(original_bytes.saturating_sub(replacement.len()));
            cleared_call_ids.push(call_id.clone());
            *output = replacement;
        }
    }

    if cleared_call_ids.is_empty() {
        return None;
    }
    let after = estimate_context(conversation);
    Some(MicroCompactionReport {
        cleared_call_ids,
        bytes_saved,
        before_estimated_tokens: before.estimated_tokens,
        after_estimated_tokens: after.estimated_tokens,
    })
}

/// Walk `conversation` in order and collect `(call_id, tool_name)` pairs
/// for every `FunctionCallOutput` whose declaring `FunctionCall` names a
/// compactable tool. Order is preserved so the caller can drop the
/// trailing `keep_recent` and clear the rest.
fn collect_compactable_call_ids(conversation: &[LlmInputItem]) -> Vec<(&str, &str)> {
    let mut tool_for_call: BTreeMap<&str, &str> = BTreeMap::new();
    for item in conversation {
        if let LlmInputItem::FunctionCall { call_id, name, .. } = item
            && COMPACTABLE_TOOL_NAMES.contains(&name.as_str())
        {
            tool_for_call.insert(call_id.as_str(), name.as_str());
        }
    }
    let mut pairs = Vec::with_capacity(tool_for_call.len());
    for item in conversation {
        if let LlmInputItem::FunctionCallOutput { call_id, .. } = item
            && let Some(name) = tool_for_call.get(call_id.as_str())
        {
            pairs.push((call_id.as_str(), *name));
        }
    }
    pairs
}

fn is_already_cleared(output: &str) -> bool {
    output.starts_with(MICRO_COMPACT_CLEARED_PREFIX)
}

/// Expired-context masking by file-mutation lineage (cost-reduction idea
/// M2). When a turn lands one or more *successful* in-place edits, the
/// earlier `read_file`/`read_slice`/grep observations of the same files
/// still sitting in the trajectory now show pre-edit text. They are dead
/// weight: the model re-reads the same input tokens on every subsequent
/// turn (the quadratic N(N+1)/2 input term threshold-compaction and
/// SHA-dedup never reclaim). This pass rewrites only the *changed spans*
/// inside those stale observations to a recovery stub, in place, with no
/// extra model call.
///
/// Safety invariants (see the M2 review entry):
/// - **Successful edits only.** Errored/denied/reverted edits never reach
///   this function — the caller filters on [`SuccessfulEdit`].
/// - **Changed spans only.** For search/replace edits we splice out just
///   the `search` text (the bytes that no longer exist in P); content
///   outside the changed range stays byte-for-byte intact. A full-file
///   `write_file` overwrite has no sub-span, so its prior snapshots are
///   masked whole.
/// - **Recovery stub preserved.** Each masked span becomes the existing
///   [`MICRO_COMPACT_CLEARED_PREFIX`] placeholder carrying `call_id`,
///   tool name, and byte count so the model can re-read P if the span is
///   still load-bearing.
/// - **Freshest read kept.** Honoring the same policy as
///   `micro_compaction_keep_recent`, the most recent `keep_recent`
///   observations of P — the reads that plausibly informed the edit — are
///   left verbatim; only strictly older snapshots are masked.
///
/// Returns `Some(report)` when at least one span was rewritten.
pub(crate) fn mask_expired_reads_after_edits(
    conversation: &mut [LlmInputItem],
    edits: &[SuccessfulEdit],
    keep_recent: usize,
) -> Option<ExpiredContextReport> {
    if edits.is_empty() {
        return None;
    }

    // Index every expirable read/grep observation by the file path its
    // `FunctionCall` arguments target, preserving conversation order so we
    // can keep the freshest `keep_recent` snapshots of each path verbatim.
    // Owned maps so the mutation loop below can borrow `conversation`
    // mutably without aliasing these indices.
    let mut call_meta: BTreeMap<String, (String, String)> = BTreeMap::new();
    for item in conversation.iter() {
        if let LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } = item
            && EXPIRABLE_READ_TOOL_NAMES.contains(&name.as_str())
            && let Some(path) = arguments_path(arguments)
        {
            call_meta.insert(call_id.clone(), (name.clone(), path));
        }
    }
    let call_path = |call_id: &str| call_meta.get(call_id).map(|(_, path)| path.as_str());

    // Per edited path, the index (into `output_positions`) up to which an
    // observation is "old enough" to mask. Newest `keep_recent` snapshots
    // of that path are excluded.
    let mut maskable_positions: BTreeMap<&str, usize> = BTreeMap::new();
    {
        // For each path, collect the positions (in conversation order) of
        // its read outputs, then drop the trailing `keep_recent`.
        let mut positions_for_path: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (idx, item) in conversation.iter().enumerate() {
            if let LlmInputItem::FunctionCallOutput { call_id, .. } = item
                && let Some(path) = call_path(call_id.as_str())
            {
                positions_for_path.entry(path).or_default().push(idx);
            }
        }
        for (path, positions) in &positions_for_path {
            let keep = keep_recent.min(positions.len());
            let maskable = positions.len().saturating_sub(keep);
            // Boundary position: outputs before `positions[maskable]` are
            // maskable; `usize::MAX` when every snapshot is old enough.
            let boundary = positions.get(maskable).copied().unwrap_or(usize::MAX);
            maskable_positions.insert(path, boundary);
        }
    }

    // Group the changed spans by edited path. A full-file overwrite is
    // recorded with an empty span list + `whole_file`.
    let mut spans_by_path: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut whole_file_paths: BTreeMap<&str, ()> = BTreeMap::new();
    for edit in edits {
        if edit.whole_file {
            whole_file_paths.insert(edit.path.as_str(), ());
        }
        let bucket = spans_by_path.entry(edit.path.as_str()).or_default();
        for span in &edit.changed_spans {
            if !span.is_empty() {
                bucket.push(span.as_str());
            }
        }
    }

    let mut masked_call_ids = Vec::new();
    let mut bytes_saved: usize = 0;
    let mut spans_masked: usize = 0;
    for (idx, item) in conversation.iter_mut().enumerate() {
        let LlmInputItem::FunctionCallOutput {
            call_id, output, ..
        } = item
        else {
            continue;
        };
        let Some((tool_name, path)) = call_meta.get(call_id.as_str()) else {
            continue;
        };
        let tool_name = tool_name.as_str();
        let path = path.as_str();
        // Skip the freshest `keep_recent` snapshots of this path.
        let Some(boundary) = maskable_positions.get(path) else {
            continue;
        };
        if idx >= *boundary {
            continue;
        }

        let original_bytes = output.len();
        let mut spans_here = 0usize;

        if whole_file_paths.contains_key(path) && !is_already_cleared(output) {
            // Full-file overwrite: the entire snapshot is stale. Mask the
            // whole body to the recovery stub.
            *output = whole_file_replacement(call_id, tool_name, original_bytes);
            spans_here = 1;
        } else if let Some(spans) = spans_by_path.get(path) {
            let (rewritten, replaced) = splice_changed_spans(output, spans, call_id, tool_name);
            if replaced > 0 {
                *output = rewritten;
                spans_here = replaced;
            }
        }

        if spans_here > 0 {
            bytes_saved = bytes_saved.saturating_add(original_bytes.saturating_sub(output.len()));
            masked_call_ids.push(call_id.clone());
            spans_masked = spans_masked.saturating_add(spans_here);
        }
    }

    if spans_masked == 0 {
        return None;
    }
    Some(ExpiredContextReport {
        masked_call_ids,
        bytes_saved,
        spans_masked,
    })
}

/// Pull the workspace-relative file path out of a read/grep
/// `FunctionCall`'s arguments. `read_file`/`read_slice` carry `path`
/// directly; grep carries an optional `path` scope (repo-wide greps with
/// no `path` are never lineage-masked — they have no single edited file).
fn arguments_path(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn whole_file_replacement(call_id: &str, tool_name: &str, original_bytes: usize) -> String {
    format!(
        "{prefix} (expired by edit) — call_id={call_id}, name={tool_name}, original_bytes={original_bytes}]",
        prefix = MICRO_COMPACT_CLEARED_PREFIX,
    )
}

/// Replace each occurrence of a changed `span` inside `output` with the
/// recovery stub, leaving all surrounding bytes untouched. The read
/// tool's `output` is the JSON-serialized tool result, so the raw source
/// `span` is escaped to its JSON in-string form before matching (newlines
/// → `\n`, quotes → `\"`, etc.) — otherwise multi-line search text would
/// never match the escaped body. Returns the rewritten string and the
/// number of span occurrences replaced.
fn splice_changed_spans(
    output: &str,
    spans: &[&str],
    call_id: &str,
    tool_name: &str,
) -> (String, usize) {
    let mut result = output.to_string();
    let mut replaced = 0usize;
    for span in spans {
        let needle = json_escaped_inner(span);
        let stub = format!(
            "{prefix} (expired span) call_id={call_id} name={tool_name} bytes={bytes}]",
            prefix = MICRO_COMPACT_CLEARED_PREFIX,
            bytes = needle.len(),
        );
        // Only splice when the recovery stub is strictly smaller than the
        // span it replaces — masking is a *cost* win, and a stub longer
        // than the changed text would grow the payload and add noise. This
        // also rules out pathologically short fragments that could collide
        // with unrelated text: a tiny span can never beat the stub length.
        if needle.len() <= stub.len() {
            continue;
        }
        while let Some(pos) = result.find(&needle) {
            result.replace_range(pos..pos + needle.len(), &stub);
            replaced += 1;
        }
    }
    (result, replaced)
}

/// Serialize `text` as a JSON string and strip the surrounding quotes,
/// yielding the exact byte sequence that the same text occupies inside an
/// already-serialized JSON tool-result body.
fn json_escaped_inner(text: &str) -> String {
    let encoded = serde_json::Value::String(text.to_string()).to_string();
    // `to_string` always wraps a JSON string in `"`; drop both quotes.
    encoded
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_string)
        .unwrap_or(encoded)
}

#[cfg(test)]
#[path = "micro_compaction_tests.rs"]
mod tests;
