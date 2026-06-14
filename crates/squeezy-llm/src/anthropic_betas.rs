//! Provider-aware routing for Anthropic beta opt-ins.
//!
//! Anthropic gates new capabilities (1M context, interleaved thinking,
//! advanced tool use, etc.) behind named beta flags. The 1P Anthropic
//! Messages API accepts them via the `anthropic-beta` HTTP header; on
//! Bedrock the AWS gateway strips non-standard headers, so a subset has
//! to go in `additional_model_request_fields.anthropic_beta` (an
//! application-layer body field) instead. Only the body-param-eligible
//! betas reach Bedrock; the rest are dropped on that transport.
//!
//! This module is the routing surface only. Specific beta constants and
//! per-model gating policy belong in a follow-up.
use std::sync::Arc;

/// Anthropic beta id for the 1M-token context window on Claude Sonnet 4
/// (and successors). Carried in the `anthropic-beta` header on the 1P
/// transport and in `additional_model_request_fields.anthropic_beta` on
/// Bedrock.
pub const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";

/// Anthropic beta id for interleaved/extended thinking — lets the model
/// reason between tool calls within a single turn. Body-eligible on
/// Bedrock; otherwise header-only.
pub const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Beta opt-ins that must be carried in the request body (not the HTTP
/// header) when the request goes through Bedrock. Other betas have no
/// safe Bedrock transport today and are dropped.
pub(crate) const BEDROCK_EXTRA_PARAMS_BETAS: &[&str] = &[
    INTERLEAVED_THINKING_BETA,
    CONTEXT_1M_BETA,
    "tool-search-tool-2025-10-19",
];

/// Deduplicate while preserving the first occurrence of each beta. Used by
/// every transport so a caller-supplied list that overlaps with a future
/// capability-derived list never produces a duplicate on the wire.
pub(crate) fn dedup_preserving_order(betas: &[Arc<str>]) -> Vec<Arc<str>> {
    let mut out: Vec<Arc<str>> = Vec::with_capacity(betas.len());
    for beta in betas {
        if out
            .iter()
            .any(|existing| existing.as_ref() == beta.as_ref())
        {
            continue;
        }
        out.push(beta.clone());
    }
    out
}

/// Comma-joined `anthropic-beta` header value for the 1P Anthropic
/// Messages transport. Returns `None` when there are no betas to emit so
/// the caller can skip the header entirely.
pub(crate) fn anthropic_header_value(betas: &[Arc<str>]) -> Option<String> {
    if betas.is_empty() {
        return None;
    }
    let deduped = dedup_preserving_order(betas);
    if deduped.is_empty() {
        return None;
    }
    let mut header = String::with_capacity(
        deduped
            .iter()
            .map(|beta| beta.as_ref().len())
            .sum::<usize>()
            + deduped.len().saturating_sub(1),
    );
    for (index, beta) in deduped.iter().enumerate() {
        if index > 0 {
            header.push(',');
        }
        header.push_str(beta.as_ref());
    }
    Some(header)
}

/// Subset of `betas` that Bedrock accepts via
/// `additional_model_request_fields.anthropic_beta`. Header-only betas
/// are dropped because the AWS gateway strips non-standard HTTP headers
/// before they reach the Anthropic backend.
pub(crate) fn bedrock_extra_body_betas(betas: &[Arc<str>]) -> Vec<Arc<str>> {
    if betas.is_empty() {
        return Vec::new();
    }
    let deduped = dedup_preserving_order(betas);
    deduped
        .into_iter()
        .filter(|beta| BEDROCK_EXTRA_PARAMS_BETAS.contains(&beta.as_ref()))
        .collect()
}

#[cfg(test)]
#[path = "anthropic_betas_tests.rs"]
mod tests;
