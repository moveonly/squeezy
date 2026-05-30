//! OpenAI prompt-cache key validation helpers.
//!
//! OpenAI's Responses and Chat-Completions APIs reject `prompt_cache_key`
//! values longer than 64 Unicode codepoints, *silently* — the request
//! succeeds, but the field is dropped server-side and no cached prefix is
//! reused. Sessions whose derived cache key exceeds the limit (UUID
//! prefixes, nested skill scopes, etc.) therefore burn full uncached
//! input tokens on every turn while telemetry reports a cache hit-rate
//! of zero with no error in sight.
//!
//! This module centralizes the client-side clamp so the OpenAI Responses
//! adapter and the OpenAI-compatible chat-completions adapter agree on
//! the single source of truth.
//!
//! The clamp counts Unicode scalar values (codepoints), not bytes — a key
//! of 64 multibyte characters (up to 4 bytes each) is well within the
//! API's documented limit even when its byte length exceeds 64. Counting
//! by bytes would needlessly truncate non-ASCII session ids.

/// Maximum length the OpenAI Responses / Chat-Completions APIs accept on
/// the `prompt_cache_key` body field, counted in Unicode codepoints.
pub(crate) const OPENAI_PROMPT_CACHE_KEY_MAX_CODEPOINTS: usize = 64;

/// Clamp a prompt-cache key to OpenAI's documented 64-codepoint limit,
/// returning the input slice unchanged when it already fits. Counts
/// codepoints (`char_indices`), not bytes, so multibyte session ids survive
/// up to the codepoint cap regardless of UTF-8 byte length.
pub(crate) fn clamp_prompt_cache_key(key: &str) -> &str {
    match key
        .char_indices()
        .nth(OPENAI_PROMPT_CACHE_KEY_MAX_CODEPOINTS)
    {
        Some((byte_idx, _)) => &key[..byte_idx],
        None => key,
    }
}

#[cfg(test)]
#[path = "openai_prompt_cache_tests.rs"]
mod tests;
