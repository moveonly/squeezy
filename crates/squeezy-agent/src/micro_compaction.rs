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
//! large outputs (file reads, shell, search, web) are in scope. Tools that
//! already return receipt-stubbed or otherwise small payloads
//! (`notes_recall`, `checkpoint_*`, MCP control calls) are intentionally
//! excluded — there is nothing worth clearing.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use squeezy_core::AppConfig;
use squeezy_llm::LlmInputItem;

use crate::context_compaction::estimate_context;

/// Tools whose `FunctionCallOutput.output` payload may be rewritten to a
/// placeholder once the conversation crosses the micro-compaction
/// threshold. Limited to tools that can plausibly emit large outputs
/// (file/shell/search/web). Tools whose outputs are already capped by
/// receipts or by aggregate budgets stay out of the set so the model
/// keeps their full content.
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
    let window = cc.model_context_window?;
    if window == 0 {
        return None;
    }
    let percent = cc.micro_compaction_threshold_percent.min(100) as u64;
    let threshold = window.saturating_mul(percent).saturating_div(100);
    let before = estimate_context(conversation);
    let observed = last_total_tokens.unwrap_or(before.estimated_tokens);
    if observed < threshold {
        return None;
    }

    let compactable_ids = collect_compactable_call_ids(conversation);
    if compactable_ids.len() <= cc.micro_compaction_keep_recent {
        return None;
    }
    let keep_recent = cc.micro_compaction_keep_recent.max(1);
    let clear_count = compactable_ids.len().saturating_sub(keep_recent);
    let clear_set: BTreeSet<&str> = compactable_ids
        .iter()
        .take(clear_count)
        .map(|(call_id, _)| call_id.as_str())
        .collect();

    let mut tool_names: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for (call_id, name) in &compactable_ids {
        tool_names.insert(call_id.as_str(), name.as_str());
    }

    let mut cleared_call_ids = Vec::new();
    let mut bytes_saved: usize = 0;
    for item in conversation.iter_mut() {
        if let LlmInputItem::FunctionCallOutput { call_id, output } = item
            && clear_set.contains(call_id.as_str())
            && !is_already_cleared(output)
        {
            let original_bytes = output.len();
            let tool_name = tool_names.get(call_id.as_str()).copied().unwrap_or("?");
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
fn collect_compactable_call_ids(conversation: &[LlmInputItem]) -> Vec<(String, String)> {
    let mut tool_for_call: std::collections::BTreeMap<&str, &str> =
        std::collections::BTreeMap::new();
    for item in conversation {
        if let LlmInputItem::FunctionCall { call_id, name, .. } = item
            && COMPACTABLE_TOOL_NAMES.contains(&name.as_str())
        {
            tool_for_call.insert(call_id.as_str(), name.as_str());
        }
    }
    let mut pairs = Vec::new();
    for item in conversation {
        if let LlmInputItem::FunctionCallOutput { call_id, .. } = item
            && let Some(name) = tool_for_call.get(call_id.as_str())
        {
            pairs.push((call_id.clone(), (*name).to_string()));
        }
    }
    pairs
}

fn is_already_cleared(output: &str) -> bool {
    output.starts_with(MICRO_COMPACT_CLEARED_PREFIX)
}

#[cfg(test)]
#[path = "micro_compaction_tests.rs"]
mod tests;
