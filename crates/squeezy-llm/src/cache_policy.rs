//! Reusable cache-hint placement helpers.
//!
//! Anthropic-family wire formats (native Anthropic Messages, OpenAI-compatible
//! aggregators that proxy Anthropic, and Bedrock Converse) all accept inline
//! breakpoint markers that tell the server where to cache the prefix. The
//! exact serialization differs per protocol — Anthropic and aggregator routes
//! attach a JSON `cache_control: { type: "ephemeral" }` object; Bedrock's
//! Converse API uses typed `CachePoint` content blocks built through the AWS
//! SDK. The *decision* of where to place those breakpoints (tools tail,
//! system tail, last user message) is identical across all three.
//!
//! This module centralizes that decision so each protocol adapter only has to
//! emit the protocol-specific marker — not re-derive the strategy. The
//! Anthropic Messages and OpenAI-compatible adapters operate on
//! `serde_json::Value` and can use [`anthropic_messages::mark_last_user_block`]
//! and the matching `system` / `tool` helpers verbatim. The Bedrock adapter
//! builds typed AWS SDK structures and relies on
//! [`should_apply_caching`] for the cross-protocol gating decision while
//! keeping the typed insertion local.
//!
//! [`CacheSpec`] and [`CacheRetention`] are the public surface every caller
//! routes through. The legacy `LlmRequest::cache_key` field is lifted into a
//! `CacheSpec` at the provider boundary via `From<Option<String>>` so old
//! callers keep their `Short` (5m / in-memory) behavior unchanged while new
//! callers opt into `Long` (1h on Anthropic, 24h on OpenAI) by setting
//! `cache.retention` directly.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{LlmRequest, capabilities_for};

/// Cache retention window for prompt caching.
///
/// Maps the agent's coarse "how long should the provider keep the cached
/// prefix alive" intent to provider-specific knobs:
/// - Anthropic: `Long` → `cache_control: { type: "ephemeral", ttl: "1h" }`;
///   `Short` → marker without `ttl` (5m default); `None` → no marker at all.
/// - OpenAI Responses: `Long` → top-level `prompt_cache_retention: "24h"`;
///   `Short` / `None` → field omitted (provider's short-lived default).
/// - Compatible (Anthropic-via-aggregator): mirrors Anthropic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    /// Disable prompt caching for this request. Providers must not emit a
    /// `cache_control` marker, a `cachePoint` block, or a retention
    /// directive. Equivalent to the historical "no `cache_key`" path.
    #[default]
    None,
    /// Provider default cache window (Anthropic 5m, OpenAI short-lived
    /// in-memory). This is the implicit retention assigned when a caller
    /// only supplies the legacy `cache_key`.
    Short,
    /// Extended retention: Anthropic emits `ttl: "1h"`; OpenAI emits
    /// `prompt_cache_retention: "24h"`.
    Long,
}

/// Universal cache hint carried on [`LlmRequest`].
///
/// `key` groups a series of turns for provider-side cache affinity (currently
/// only OpenAI's `prompt_cache_key` actually consumes the value; Anthropic's
/// caching is prefix-hash based and ignores the key). `retention` selects the
/// TTL band — see [`CacheRetention`] for the per-provider mapping.
///
/// Construct via `CacheSpec::default()` for "no caching", set
/// `retention: CacheRetention::Long` for extended TTL, or use
/// `CacheSpec::from(Some(key))` to lift a legacy cache-key string into the
/// new shape (yields `Short` retention).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub retention: CacheRetention,
}

impl From<Option<String>> for CacheSpec {
    /// Backwards-compatibility bridge from the legacy
    /// `LlmRequest::cache_key` field. `Some(key)` maps to
    /// `{ key: Some(key), retention: Short }` so old callers preserve the
    /// 5m / in-memory provider-default behavior they had before the
    /// retention enum existed. `None` returns the disabled default.
    fn from(key: Option<String>) -> Self {
        match key {
            Some(k) => Self {
                key: Some(k),
                retention: CacheRetention::Short,
            },
            None => Self::default(),
        }
    }
}

impl CacheSpec {
    /// Build a capability-gated cache hint for a control-plane / helper
    /// request family that re-sends a large, stable prefix (system + tools +
    /// growing history) across calls sharing the same `key`.
    ///
    /// Returns the requested `{ key, retention }` only when the destination
    /// `(provider, model)` reports `prompt_caching` in the registry;
    /// otherwise returns [`CacheSpec::default`] (retention `None`) so
    /// non-supporting providers are byte-for-byte unchanged and never see a
    /// cache directive they would reject. Centralizing the decision here keeps
    /// every agent call site that opts into caching consistent — they pass the
    /// *intent* (a stable key + desired retention) and the registry gate is
    /// applied in one place.
    ///
    /// Callers must only supply a `key` that is stable across the requests
    /// meant to share a cache prefix and unique to that family — never one that
    /// mixes unrelated content, which would poison the provider-side affinity.
    pub fn for_prefix_reuse(
        provider: &str,
        model: &str,
        key: Option<String>,
        retention: CacheRetention,
    ) -> Self {
        let supported = capabilities_for(provider, model)
            .is_some_and(|capabilities| capabilities.prompt_caching);
        if supported && retention != CacheRetention::None {
            Self { key, retention }
        } else {
            Self::default()
        }
    }
}

#[cfg(test)]
#[path = "cache_policy_tests.rs"]
mod tests;

/// Where in the message list to anchor the trailing breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageStrategy {
    /// Mark the most recent user-role message (Anthropic recommended).
    LatestUserMessage,
}

/// How many user-role messages behind the moving (latest-user) breakpoint
/// the stationary "stable-tail anchor" sits.
///
/// Anthropic gives every request four `cache_control` breakpoints. Squeezy's
/// auto policy spends three (tools / system / latest-user) and historically
/// left the fourth idle. With only the latest-user breakpoint, an agent loop
/// re-bills the just-settled tail at the 1.25x cache-*write* rate every round:
/// when the model appends a large multi-file tool-result batch, the single
/// moving breakpoint hops onto that fresh batch, so the prefix it vacated is
/// no longer a cache prefix boundary and gets re-written instead of read.
/// Real Haiku traces showed `corr(cost, cache_write) = +0.95` and the corpus
/// written to cache ~2x (e.g. 132k writes for ~67k unique tokens).
///
/// The fourth breakpoint anchors a *settled* user block a fixed distance back
/// from the tail. The bulk prefix up to the anchor is then guaranteed a cheap
/// cache *read* (0.1x) on the next round while the moving breakpoint absorbs
/// only the new delta.
///
/// The backoff must be `>= 1` so the anchor is strictly behind the moving
/// breakpoint (two `cache_control` markers on the same block is wasteful and
/// the anchor would move in lockstep, defeating the purpose). A *small* value
/// keeps the anchor close to the tail so it still covers nearly the whole
/// prefix. `2` places the anchor two user turns back: with the typical
/// assistant-tool-call → user-tool-result cadence that leaves exactly one full
/// round (the in-flight assistant turn plus its fresh tool-result batch)
/// between the anchor and the moving breakpoint, so the anchor never lands on
/// a block that is still being appended to, yet trails the tail by the minimum
/// needed to settle.
pub(crate) const STABLE_ANCHOR_BACKOFF: usize = 2;

/// Auto-placement policy: mark tools, system, and the latest user message.
///
/// Squeezy currently exposes only this default; per-skill or per-session
/// overrides plug into the same struct when needed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CachePolicy {
    pub tools: bool,
    pub system: bool,
    pub messages: MessageStrategy,
}

impl CachePolicy {
    /// The single default policy used by every Anthropic-family adapter.
    pub(crate) const AUTO: Self = Self {
        tools: true,
        system: true,
        messages: MessageStrategy::LatestUserMessage,
    };
}

/// Decide whether the destination model accepts inline cache breakpoints.
///
/// Returns `true` only when the caller asked for caching (effective
/// `CacheRetention != None`, i.e. either an explicit `cache.retention` of
/// `Short`/`Long` or the legacy `cache_key` is set) *and* the model registry
/// reports `prompt_caching` for `(provider, model)`. The retention gate lets
/// agent code disable caching on short conversations (where cache writes cost
/// more than reads); the registry gate keeps us from sending markers to
/// models that would 400.
pub(crate) fn should_apply_caching(provider: &str, request: &LlmRequest) -> bool {
    request.effective_cache_retention() != CacheRetention::None
        && capabilities_for(provider, &request.model)
            .is_some_and(|capabilities| capabilities.prompt_caching)
}

/// Ephemeral `cache_control` literal shared across Anthropic Messages and
/// OpenAI-compatible aggregator wire formats. The optional `ttl: "1h"` field
/// is emitted only for [`CacheRetention::Long`]; `Short` keeps the marker
/// shape Anthropic has historically accepted (no `ttl` = 5m default). Callers
/// must not invoke this with [`CacheRetention::None`] — the caching decision
/// is gated upstream by [`should_apply_caching`].
pub(crate) fn ephemeral_marker(retention: CacheRetention) -> Value {
    if retention == CacheRetention::Long {
        json!({ "type": "ephemeral", "ttl": "1h" })
    } else {
        json!({ "type": "ephemeral" })
    }
}

/// Tool-name prefix the agent reserves for dynamically advertised MCP
/// tools. The tool registry pushes any tool whose name starts with this
/// to the *end* of the advertised list, so the cache breakpoint must
/// land before them — otherwise an MCP `tools/list` refresh that
/// reorders or replaces dynamic tools would invalidate the cached
/// prompt prefix on every turn.
pub(crate) const DYNAMIC_TOOL_NAME_PREFIX: &str = "mcp__";

/// Pick the index of the last *stable* (non-mcp__-prefixed) tool to
/// anchor the cache breakpoint on. Falls back to the literal last
/// index when every advertised tool is dynamic so callers still place a
/// breakpoint somewhere when caching is enabled. Returns `None` only on
/// an empty iterator.
///
/// Centralizing this decision means the Anthropic JSON path, the
/// OpenAI-compatible aggregator path (Anthropic-flavoured), and any
/// future protocol adapter all agree on which tool entry receives the
/// marker. Each adapter still owns the marker insertion in its own
/// wire shape (`cache_control` on the chosen JSON object, or a typed
/// `CachePoint` block for Bedrock).
pub(crate) fn last_stable_tool_index<'a, I>(names: I) -> Option<usize>
where
    I: IntoIterator<Item = &'a str>,
    I::IntoIter: DoubleEndedIterator + ExactSizeIterator,
{
    let iter = names.into_iter();
    let len = iter.len();
    if len == 0 {
        return None;
    }
    let stable = iter
        .enumerate()
        .rev()
        .find_map(|(idx, name)| (!name.starts_with(DYNAMIC_TOOL_NAME_PREFIX)).then_some(idx));
    Some(stable.unwrap_or(len - 1))
}

/// Helpers that operate on the Anthropic Messages JSON wire format. The
/// OpenAI-compatible aggregator path (when the destination is an Anthropic
/// model) re-uses the same content shape, so it shares these helpers.
///
/// Every helper takes a [`CacheRetention`] so the emitted `cache_control`
/// marker carries `ttl: "1h"` when the caller asked for `Long` retention;
/// `Short` keeps the historical no-`ttl` (5m default) shape.
pub(crate) mod json_markers {
    use serde_json::{Value, json};

    use super::CacheRetention;

    /// Wrap a plain `system` string in the array form Anthropic Messages
    /// requires for inline cache markers.
    pub(crate) fn system_array_with_marker(instructions: &str, retention: CacheRetention) -> Value {
        json!([{
            "type": "text",
            "text": instructions,
            "cache_control": super::ephemeral_marker(retention),
        }])
    }

    /// Mark the last block of the most recent user-role message. No-op when
    /// no user message exists.
    pub(crate) fn mark_last_user_block(messages: &mut [Value], retention: CacheRetention) {
        for message in messages.iter_mut().rev() {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if let Some(block) = message
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .and_then(|content| content.last_mut())
                .and_then(Value::as_object_mut)
            {
                block.insert(
                    "cache_control".to_string(),
                    super::ephemeral_marker(retention),
                );
            }
            return;
        }
    }

    /// Mark the last block of the user-role message that sits `backoff`
    /// user-role messages behind the most recent one — the stationary
    /// "stable-tail anchor".
    ///
    /// `mark_last_user_block` places the *moving* breakpoint on the latest
    /// user block; this places a *settled* breakpoint `backoff` user turns
    /// earlier so the cached prefix up to that point survives as a cheap
    /// cache read while the moving breakpoint absorbs the freshly appended
    /// tail. The two markers therefore land on different messages by
    /// construction (`backoff >= 1`), so no block ever carries two
    /// `cache_control` markers.
    ///
    /// No-op when fewer than `backoff + 1` user-role messages exist — short
    /// conversations have no settled prefix worth anchoring, and forcing a
    /// second marker there would collide with (or land adjacent to) the
    /// moving breakpoint for no benefit. Callers must invoke this with the
    /// *same* `messages` slice and *after* `mark_last_user_block` so the two
    /// breakpoints are budgeted together.
    pub(crate) fn mark_stable_anchor_block(
        messages: &mut [Value],
        backoff: usize,
        retention: CacheRetention,
    ) {
        debug_assert!(
            backoff >= 1,
            "stable anchor must trail the moving breakpoint"
        );
        // Walk from the tail and skip past `backoff` user-role messages; the
        // next user-role message after that is the anchor. Counting only
        // user-role messages (not assistant/tool turns) keeps the anchor a
        // fixed number of *user turns* back regardless of how many assistant
        // or reasoning blocks sit between them.
        let mut user_seen = 0usize;
        for message in messages.iter_mut().rev() {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            user_seen += 1;
            if user_seen <= backoff {
                continue;
            }
            if let Some(block) = message
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .and_then(|content| content.last_mut())
                .and_then(Value::as_object_mut)
            {
                block.insert(
                    "cache_control".to_string(),
                    super::ephemeral_marker(retention),
                );
            }
            return;
        }
    }

    /// Attach the cache breakpoint to the last *stable* tool definition in
    /// the Anthropic Messages tool shape (`{"name": ..., ...}`). Reads
    /// the `name` field directly off each JSON value and delegates the
    /// breakpoint-index decision to [`super::last_stable_tool_index`].
    /// No-op on an empty slice.
    pub(crate) fn mark_last_stable_tool(tool_values: &mut [Value], retention: CacheRetention) {
        let Some(idx) = super::last_stable_tool_index(
            tool_values
                .iter()
                .map(|tool| tool.get("name").and_then(Value::as_str).unwrap_or("")),
        ) else {
            return;
        };
        if let Some(obj) = tool_values.get_mut(idx).and_then(Value::as_object_mut) {
            obj.insert(
                "cache_control".to_string(),
                super::ephemeral_marker(retention),
            );
        }
    }
}
