//! xAI Grok provider with split routing between the Responses API and Chat
//! Completions.
//!
//! xAI publishes both an OpenAI-Responses-compatible endpoint
//! (`POST /v1/responses`) and a Chat-Completions endpoint
//! (`POST /v1/chat/completions`) on `https://api.x.ai`. As of the May 2026
//! catalog refresh xAI treats the Responses route as the canonical surface
//! and rolls every new Grok generation (Grok 4.3, Grok 4.20, Grok Build,
//! Grok Code) on that wire — reasoning summaries, encrypted reasoning
//! replay, hosted tools, and Live Search all live on Responses. The Chat
//! Completions wire is kept as a defensive fallback: every shipping xAI
//! model still accepts it, and any unknown id (legacy `grok-2`, `grok-1`,
//! `grok-beta`, or a non-grok string a caller routed through a `base_url`
//! override) reaches it without a 404.
//!
//! The provider holds one client per route and dispatches per-request based
//! on [`classify_route`]; per-startup dispatch would lock a session to a
//! single wire even when the user switches Grok generations mid-run.
//!
//! M-31 (xAI `usage.cached_tokens` fallback): the chat-completions cost
//! parser in `compatible.rs` only consults
//! `prompt_tokens_details.cached_tokens` and `prompt_cache_hit_tokens`,
//! but xAI documents a top-level `usage.cached_tokens` shape in some
//! responses. The fix lives in `parse_chat_usage` (compatible.rs, NOT
//! owned by this phase). The regression marker `xai_chat_top_level_cached_tokens_gap_marker_m31`
//! locks in the *current* `None` so a Phase 4-cross fix flips one
//! assertion when it lands.

use std::collections::BTreeMap;

use squeezy_core::{OpenAiCompatibleConfig, OpenAiCompatiblePreset, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

use crate::compatible::substitute_url_placeholders;
use crate::credentials::{resolve_api_key_with_inline, static_api_key_source};
use crate::{LlmProvider, LlmRequest, LlmStream, OpenAiCompatibleProvider, OpenAiProvider};

#[derive(Debug, Clone)]
pub struct XaiProvider {
    responses: OpenAiProvider,
    chat: OpenAiCompatibleProvider,
}

impl XaiProvider {
    pub fn from_config(config: &OpenAiCompatibleConfig) -> Result<Self> {
        // M-32: resolve the xAI credential exactly once and share a
        // single `Arc<dyn ApiKeySource>` between the Responses and
        // Chat sub-providers. The previous shape called
        // `OpenAiProvider::from_xai_config` and
        // `OpenAiCompatibleProvider::from_config` back-to-back, each of
        // which re-read credentials.json and the env var. The duplicate
        // I/O is harmless for static keys today but races for any
        // future OAuth credential (cf. opencode's `XaiAuthPlugin` for
        // SuperGrok), so funnel both sub-providers through the same
        // source up front.
        if config.preset != OpenAiCompatiblePreset::XAi {
            return Err(SqueezyError::ProviderNotConfigured(format!(
                "XaiProvider::from_config requires preset=XAi, got {:?}",
                config.preset,
            )));
        }
        if config.base_url.trim().is_empty() {
            return Err(SqueezyError::ProviderNotConfigured(
                "providers.xai.base_url is required for the xAI preset".to_string(),
            ));
        }
        let resolved_base_url = substitute_url_placeholders(
            config.base_url.trim_end_matches('/'),
            config.preset,
            config.account_id.as_deref(),
            config.gateway_id.as_deref(),
        )?;
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        let api_key_source = static_api_key_source(api_key, "xai");

        let responses = OpenAiProvider::with_api_key_source(
            "xai",
            api_key_source.clone(),
            resolved_base_url.clone(),
            // No api_version applies to xAI's `/v1/responses` endpoint.
            None,
            config.transport,
        )
        .with_extra_headers(config.extra_headers.clone());

        // The chat route still merges any user-supplied headers on top
        // of the (empty) preset defaults so xAI requests routed through
        // PortKey/Helicone-style proxies keep their attribution tags.
        // xAI itself does not stamp preset_default_headers; mirror that
        // by starting from an empty map.
        let mut chat_headers: BTreeMap<String, String> = BTreeMap::new();
        for (key, value) in &config.extra_headers {
            chat_headers.insert(key.clone(), value.clone());
        }
        let chat = OpenAiCompatibleProvider::with_api_key_source(
            config.preset,
            api_key_source,
            resolved_base_url,
            chat_headers,
            config.transport,
        );

        Ok(Self { responses, chat })
    }
}

impl LlmProvider for XaiProvider {
    fn name(&self) -> &'static str {
        "xai"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        // H-23: xAI Live Search. The dispatcher keeps the hosted-tool
        // intent on the full request and lets the selected wire adapter
        // lower it: Responses appends a `web_search` hosted tool, while
        // Chat Completions maps it to top-level `search_parameters`.
        // Citation parsing on the chat path is tracked separately in M-31.
        match classify_route(&request.model) {
            XaiRoute::Responses => self.responses.stream_response(request, cancel),
            XaiRoute::Chat => self.chat.stream_response(request, cancel),
            XaiRoute::ImageNotRouted => {
                // `grok-imagine-*` lives on `/v1/images/generations` which
                // neither sub-provider knows about. Surface a structured
                // error so callers see a useful message instead of a 404
                // returned by the chat parser. M-33 tracks wiring the
                // actual image endpoint.
                let model = request.model.clone();
                let err = SqueezyError::ProviderNotConfigured(format!(
                    "xAI image generation model `{model}` requires the `/v1/images/generations` endpoint, which squeezy does not yet route. See `.audit/providers/xai.md` (M-33)."
                ));
                Box::pin(async_stream::stream! { yield Err(err); })
            }
        }
    }
}

/// Routing outcome for the xAI dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum XaiRoute {
    /// Forward to the OpenAI-Responses sub-provider (`/v1/responses`).
    Responses,
    /// Forward to the OpenAI-compatible Chat Completions sub-provider
    /// (`/v1/chat/completions`).
    Chat,
    /// Image-only family (`grok-imagine-*`). The dispatcher rejects the
    /// request with a structured error because the dedicated image
    /// endpoint is not wired through either sub-provider.
    ImageNotRouted,
}

/// Pick the wire route for a given xAI model id.
///
/// The matcher walks an explicit allow-list of Grok families that xAI
/// ships on Responses as of the May 2026 catalog refresh:
///
///   * `grok-4` — flagship Grok 4 and dated SKUs.
///   * `grok-4.3` — Grok 4.3 (target of the May 15 retirement redirect
///     from `grok-4`).
///   * `grok-4.20` — Grok 4.20 family (multi-agent and
///     reasoning/non-reasoning splits).
///   * `grok-build` — Grok Build long-context (256k) coder.
///   * `grok-code` — Grok Code (code-tuned, Grok-4-era).
///
/// xAI now treats Responses as the canonical surface, so any
/// *unrecognised* Grok generation defaults to Responses too —
/// future `grok-5-*`, `grok-omega-*`, etc. SKUs route correctly
/// without a code change. Legacy `grok-2`, `grok-1`, and `grok-beta`
/// ids stay on Chat Completions where they have always lived; any
/// non-grok id falls through to Chat as a defensive default because
/// the chat endpoint accepts arbitrary model strings the user might
/// have routed through a base_url override.
///
/// `grok-imagine-*` is image-only and lives on
/// `/v1/images/generations`. Neither sub-provider knows that
/// endpoint, so the dispatcher returns [`XaiRoute::ImageNotRouted`]
/// and the caller surfaces a structured error.
pub(crate) fn classify_route(model: &str) -> XaiRoute {
    let lower = model.to_ascii_lowercase();
    // Walk to the last `/`-delimited segment so multi-layer aggregator
    // prefixes (`vercel/xai/grok-4`, `@openrouter/xai/grok-4.3`,
    // `portkey/integration/xai/grok-build-0.1`) all resolve to the
    // underlying Grok slug. `split_once('/')` would only chew the
    // first segment and misclassify the tail, so use `rsplit_once`
    // instead — the trailing segment is always the actual model id.
    let id = lower.rsplit_once('/').map(|(_, id)| id).unwrap_or(&lower);
    if id.starts_with("grok-imagine") {
        return XaiRoute::ImageNotRouted;
    }
    if id.starts_with("grok-4") || id.starts_with("grok-build") || id.starts_with("grok-code") {
        return XaiRoute::Responses;
    }
    if matches_grok_family(id, "grok-2")
        || matches_grok_family(id, "grok-1")
        || id.starts_with("grok-beta")
    {
        return XaiRoute::Chat;
    }
    if id.starts_with("grok-") {
        // Unknown Grok generation: default to Responses because xAI's
        // docs treat Responses as the canonical surface as of May
        // 2026. Falling back to Chat would 404 every future grok-5
        // reasoning request.
        return XaiRoute::Responses;
    }
    XaiRoute::Chat
}

fn matches_grok_family(id: &str, family: &str) -> bool {
    id == family
        || id
            .strip_prefix(family)
            .is_some_and(|suffix| suffix.starts_with(['-', '.']))
}

/// `true` when the model id should be dispatched against xAI's Responses
/// endpoint. Thin shim over [`classify_route`] retained for tests that
/// only care about the binary chat-vs-responses outcome.
#[cfg(test)]
pub(crate) fn is_responses_capable(model: &str) -> bool {
    matches!(classify_route(model), XaiRoute::Responses)
}

#[cfg(test)]
#[path = "xai_tests.rs"]
mod tests;
