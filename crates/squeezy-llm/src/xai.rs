//! xAI Grok provider with split routing between the Responses API and Chat
//! Completions.
//!
//! xAI publishes both an OpenAI-Responses-compatible endpoint
//! (`POST /v1/responses`) and a Chat-Completions endpoint
//! (`POST /v1/chat/completions`) on `https://api.x.ai`. Grok 3 and Grok 4
//! expose their richer feature surface (reasoning summaries, encrypted
//! reasoning replay, hosted tools) only through Responses; earlier Grok
//! models (grok-2, grok-beta) predate the Responses launch and answer only
//! the Chat route. Selecting per request keeps both generations working
//! through one provider entry by picking the route based on the
//! requested model id.
//!
//! The provider holds one client per route and dispatches per-request based
//! on [`classify_route`]; per-startup dispatch would lock a session to a
//! single wire even when the user switches Grok generations mid-run.
//!
//! H-22 (extra_headers asymmetry): the Responses route currently drops
//! `OpenAiCompatibleConfig::extra_headers` in `openai.rs::from_xai_config`
//! while the Chat route honours them. Fixing the asymmetry lives in
//! `openai.rs` (Phase 4B owns that file) and is intentionally not patched
//! here. Coordinate with that phase before declaring the audit closed.

use squeezy_core::{OpenAiCompatibleConfig, OpenAiCompatiblePreset, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

use crate::{LlmProvider, LlmRequest, LlmStream, OpenAiCompatibleProvider, OpenAiProvider};

#[derive(Debug, Clone)]
pub struct XaiProvider {
    responses: OpenAiProvider,
    chat: OpenAiCompatibleProvider,
}

impl XaiProvider {
    pub fn from_config(config: &OpenAiCompatibleConfig) -> Result<Self> {
        debug_assert_eq!(config.preset, OpenAiCompatiblePreset::XAi);
        Ok(Self {
            responses: OpenAiProvider::from_xai_config(config)?,
            chat: OpenAiCompatibleProvider::from_config(config)?,
        })
    }
}

impl LlmProvider for XaiProvider {
    fn name(&self) -> &'static str {
        "xai"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        // H-23: xAI Live Search.
        //
        // Once [`LlmRequest::hosted_tools`] (Phase 1) lands in this
        // branch, the dispatcher forwards the `LlmHostedTool::WebSearch`
        // entries through to whichever sub-provider handles the
        // selected route:
        //   * Responses path appends `{ "type": "web_search", "filters": … }`
        //     as a hosted tool entry alongside any caller-supplied
        //     function tools.
        //   * Chat path merges the same intent into a top-level
        //     `search_parameters: { mode: "auto", … }` field on the
        //     request body.
        //
        // The actual body lowering must land in `openai.rs`
        // (Responses) and `compatible.rs` (Chat); xAI's own dispatcher
        // is responsible only for forwarding the field unchanged when
        // it is present on `request`. Coordination: this TODO is
        // intentional so Phase 4B (openai.rs) and the cross-cutting
        // compatible.rs change land in lock-step with the field
        // appearing on `LlmRequest`. Citation parsing on the chat
        // path is tracked separately in M-31.
        //
        // Until Phase 1's `hosted_tools` slot ships, requests cannot
        // carry the Live Search intent at all. The forwarding still
        // happens because we hand the *full* `LlmRequest` to the
        // sub-provider; no per-field copy is needed here.
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
    // Strip an optional `xai/` aggregator namespace prefix so models
    // served through an aggregator and routed back into the xAI
    // dedicated provider (rare but possible via base_url override)
    // still resolve correctly.
    let id = lower.split_once('/').map(|(_, id)| id).unwrap_or(&lower);
    if id.starts_with("grok-imagine") {
        return XaiRoute::ImageNotRouted;
    }
    if id.starts_with("grok-4") || id.starts_with("grok-build") || id.starts_with("grok-code") {
        return XaiRoute::Responses;
    }
    if id.starts_with("grok-2") || id.starts_with("grok-1") || id.starts_with("grok-beta") {
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
