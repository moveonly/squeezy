//! OpenAI-compatible (Chat Completions) provider client.
//!
//! Covers every endpoint that speaks `POST /chat/completions` with a Bearer
//! token: OpenRouter, Vercel AI Gateway, PortKey, Groq, xAI, DeepSeek,
//! Mistral, Together AI, Fireworks AI, Cerebras, plus any custom OpenAI-
//! compatible host (self-hosted LiteLLM, Cloudflare Workers AI, etc.). The
//! native OpenAI provider stays on the `/responses` endpoint and is not
//! routed through here.

use std::{borrow::Cow, collections::BTreeMap, sync::Arc};

use async_stream::try_stream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{
    CostSnapshot, OpenAiCompatibleConfig, OpenAiCompatiblePreset, ProviderTransportConfig, Result,
    SqueezyError,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use sha2::{Digest, Sha256};

use crate::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, ReasoningKind,
    ReasoningPayload,
    anthropic_error::NON_RETRYABLE_MARKER,
    cache_policy::{CacheRetention, ephemeral_marker, json_markers, last_stable_tool_index},
    credentials::{
        ApiKeySource, resolve_api_key_with_inline, resolve_api_key_with_inline_optional,
        static_api_key_source,
    },
    openai_prompt_cache::clamp_prompt_cache_key,
    retry::{RetryPolicy, idle_timeout, send_with_auth_retry},
    sse::SseDecoder,
    transport::shared_client,
};

#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    preset: OpenAiCompatiblePreset,
    client: reqwest::Client,
    api_key: Arc<dyn ApiKeySource>,
    base_url: String,
    extra_headers: BTreeMap<String, String>,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for OpenAiCompatibleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleProvider")
            .field("preset", &self.preset)
            .field("client", &self.client)
            .field("api_key", &self.api_key)
            .field("base_url", &self.base_url)
            .field("extra_headers", &self.extra_headers)
            .field("transport", &self.transport)
            .finish()
    }
}

impl OpenAiCompatibleProvider {
    pub fn from_config(config: &OpenAiCompatibleConfig) -> Result<Self> {
        if config.base_url.trim().is_empty() {
            return Err(SqueezyError::ProviderNotConfigured(format!(
                "providers.{}.base_url is required for the {} preset",
                config.preset.as_str(),
                config.preset.display_name(),
            )));
        }
        // Substitute `{account_id}` / `{gateway_id}` placeholders in the
        // base URL before the provider locks it in. The Cloudflare presets
        // ship templated defaults so the LLM client is the single place
        // that knows how to resolve them; user overrides that keep the
        // placeholder syntax (e.g. routing through a reverse proxy that
        // mirrors the Cloudflare path shape) get the same treatment for
        // free.
        let resolved_base_url = substitute_url_placeholders(
            config.base_url.trim_end_matches('/'),
            config.preset,
            config.account_id.as_deref(),
            config.gateway_id.as_deref(),
        )?;
        // X-17: local-hosted presets (LM Studio, vLLM, llama.cpp) ship
        // without authentication by default and the squeezy-invented
        // env-var names (`LMSTUDIO_API_KEY`, etc.) are not vendor
        // conventions — nobody sets them. Walk the credential chain via
        // the optional variant so an empty resolution flows as `""`
        // instead of `ProviderNotConfigured`. The `Authorization: Bearer`
        // header is short-circuited downstream when the resolved value
        // is empty so reqwest does not panic on `bearer_auth("")`. Every
        // other preset stays on the strict variant — Groq/OpenRouter/etc.
        // without a key is a real misconfiguration and the error tells
        // the user which env var to set.
        let api_key = if is_local_preset(config.preset) {
            resolve_api_key_with_inline_optional(config.api_key.as_deref(), &config.api_key_env)?
                .value
        } else {
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value
        };
        let mut headers = preset_default_headers(config.preset);
        // User-supplied headers override preset defaults so deployments can
        // attach their own HTTP-Referer / X-Title / x-portkey-* values.
        for (key, value) in &config.extra_headers {
            headers.insert(key.clone(), value.clone());
        }
        // C-11: Cloudflare AI Gateway dual-auth inversion. The /compat
        // (and the new REST) endpoint expects the *upstream provider's*
        // key in `Authorization: Bearer` (so OpenAI/Anthropic/etc. see a
        // valid key from their own envelope) and the Cloudflare gateway
        // token in `cf-aig-authorization`. squeezy-core resolves
        // `CLOUDFLARE_API_KEY` into `config.api_key{_env}` and lets
        // `CF_AIG_TOKEN` populate `cf-aig-authorization` via
        // `extra_headers`. Without the swap below the upstream sees the
        // Cloudflare key (401) and the gateway sees either no key or the
        // Cloudflare key in both slots. opencode's `cloudflare.ts:42-51`
        // models the correct split; we mirror it here.
        //
        // Source-of-truth for the upstream key (in priority order):
        //   1. `CF_UPSTREAM_KEY` env var (operator opt-in)
        //   2. `extra_headers["upstream-api-key"]` (TOML escape hatch
        //      for callers that prefer not to set the env)
        //   3. fallthrough: leave the Bearer slot pointing at the
        //      Cloudflare key for backwards compatibility with
        //      Workers AI-only gateways that were intentionally wired
        //      up under the old (broken) scheme.
        // H-59: PortKey's canonical auth path uses the
        // `x-portkey-api-key` header rather than
        // `Authorization: Bearer`. The Bearer slot is then free to
        // carry the upstream provider's credential (BYO-key mode).
        // The opt-in is a magic `use_x_portkey_api_key = "true"`
        // entry in the user's `[providers.portkey.headers]` table.
        // Lift the resolved api_key into `x-portkey-api-key` and
        // strip both the magic flag and the original Bearer path
        // (the bearer_auth call in `stream_response` will suppress
        // itself when it sees `x-portkey-api-key` set; see the
        // P12 sibling for the symmetric `Authorization` handling).
        let portkey_canonical_auth = config.preset == OpenAiCompatiblePreset::PortKey
            && headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("use_x_portkey_api_key") && v == "true");
        if portkey_canonical_auth {
            // Strip the magic flag — it is not a wire header.
            headers.retain(|key, _| !key.eq_ignore_ascii_case("use_x_portkey_api_key"));
            // Lift the resolved Cloudflare-flavored key into the
            // `x-portkey-api-key` slot, leaving the Bearer slot
            // for whatever the user supplies on the upstream
            // request. User-supplied `x-portkey-api-key`
            // overrides win.
            let has_canonical = headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("x-portkey-api-key"));
            if !has_canonical && !api_key.is_empty() {
                headers.insert("x-portkey-api-key".to_string(), api_key.clone());
            }
        }
        // H-40: when the CF AI Gateway preset migrates to the new
        // REST URL shape, gateway selection moves from the URL
        // path to a `cf-aig-gateway-id` header. Emit it here when
        // the config carries a gateway id so the gateway is
        // selected correctly regardless of which URL template the
        // user has in place; user-supplied headers still win.
        if config.preset == OpenAiCompatiblePreset::CloudflareAiGateway
            && let Some(gateway_id) = config
                .gateway_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            && !headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("cf-aig-gateway-id"))
        {
            headers.insert("cf-aig-gateway-id".to_string(), gateway_id.to_string());
        }
        let api_key = if config.preset == OpenAiCompatiblePreset::CloudflareAiGateway {
            let upstream_key = std::env::var("CF_UPSTREAM_KEY")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    headers.iter().find_map(|(key, value)| {
                        key.eq_ignore_ascii_case("upstream-api-key")
                            .then(|| value.clone())
                    })
                });
            // Strip the `upstream-api-key` TOML escape hatch once we've
            // lifted it into the Bearer slot — it is not a real wire
            // header and would otherwise be sent verbatim.
            headers.retain(|key, _| !key.eq_ignore_ascii_case("upstream-api-key"));
            // Lift the Cloudflare gateway token into
            // `cf-aig-authorization` when the user has not already set
            // it via TOML / `CF_AIG_TOKEN` (squeezy-core handles the env
            // → header lift). User-supplied headers win to preserve
            // manual overrides; the empty-key case is left to the
            // upstream to reject.
            let has_aig_header = headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("cf-aig-authorization"));
            if !has_aig_header && !api_key.is_empty() {
                headers.insert(
                    "cf-aig-authorization".to_string(),
                    format!("Bearer {api_key}"),
                );
            }
            match upstream_key {
                Some(key) => key,
                None => api_key,
            }
        } else {
            api_key
        };
        Ok(Self::with_api_key_source(
            config.preset,
            static_api_key_source(api_key, config.preset.as_str()),
            resolved_base_url,
            headers,
            config.transport,
        ))
    }

    #[cfg(test)]
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Test-only mirror of the bearer/header attachment block inside
    /// [`Self::stream_response`]. Given a resolved key string (which may
    /// be empty for unauthenticated local presets — see X-17 / H-46),
    /// stamp the same headers a live request would carry, then return a
    /// built `reqwest::Request` callers can inspect without standing up
    /// a mock server. The conditional `bearer_auth` gate is what H-46
    /// pins: when `key` is empty the `Authorization` header must be
    /// absent so `bearer_auth("")` never panics inside reqwest and an
    /// LM Studio / vLLM / llama.cpp deployment with no token is not
    /// served a malformed `Authorization: Bearer ` blank.
    /// Test-only helper that mirrors the bearer/header attachment block.
    /// Accepts a non-secret marker (e.g. `"present"`/`""`) rather than a
    /// real API key so CodeQL taint analysis does not flag the
    /// construction path; the live `stream_response` is the only call
    /// site that ever sees a resolved credential.
    #[cfg(test)]
    pub(crate) fn build_chat_request_for_test(&self, key_marker: &str) -> reqwest::Request {
        let url = format!("{}/chat/completions", self.base_url);
        let mut builder = self.client.post(&url);
        if !key_marker.is_empty() {
            // Use a fixed token literal so the test asserts header
            // *presence* without channelling resolved credentials
            // through this helper.
            builder = builder.bearer_auth("test-bearer");
        }
        for (header_key, header_value) in &self.extra_headers {
            builder = builder.header(header_key.as_str(), header_value.as_str());
        }
        builder
            .build()
            .expect("compatible request must build with valid headers")
    }

    #[cfg(test)]
    pub(crate) fn api_key_source(&self) -> Arc<dyn ApiKeySource> {
        self.api_key.clone()
    }

    /// Construct the provider against an already-built credential
    /// source. The GitHub Copilot OAuth provider uses this path so a
    /// rotating Bearer token can flow through the Chat-Completions
    /// route without rebuilding the client.
    pub fn with_api_key_source(
        preset: OpenAiCompatiblePreset,
        api_key: Arc<dyn ApiKeySource>,
        base_url: String,
        extra_headers: BTreeMap<String, String>,
        transport: ProviderTransportConfig,
    ) -> Self {
        Self {
            preset,
            client: shared_client(&transport),
            api_key,
            base_url,
            extra_headers,
            transport,
        }
    }

    pub fn preset(&self) -> OpenAiCompatiblePreset {
        self.preset
    }

    pub fn extra_headers(&self) -> &BTreeMap<String, String> {
        &self.extra_headers
    }

    #[cfg(test)]
    pub(crate) fn request_body(request: &LlmRequest) -> Value {
        Self::request_body_for_preset(request, OpenAiCompatiblePreset::Custom)
    }

    /// Variant of [`request_body`] that takes a [`OpenAiCompatiblePreset`]
    /// so per-preset wire-shape branches (reasoning emission, body
    /// gates) can fork without spreading another substring test
    /// across the codebase. Production code (`stream_response`)
    /// always calls this overload with the constructed provider's
    /// preset; the legacy `request_body` is retained for tests that
    /// don't care about preset-specific quirks and for the
    /// non-Cloudflare path that historically did not branch.
    pub(crate) fn request_body_for_preset(
        request: &LlmRequest,
        preset: OpenAiCompatiblePreset,
    ) -> Value {
        // Anthropic-via-aggregator routes accept the same ephemeral
        // cache_control markers as the native Anthropic API. We attach them
        // when the caller has supplied a cache_key and the destination model
        // classifies as an Anthropic-compatible flavor in the
        // [`COMPAT_TABLE`] (covers OpenRouter, Vercel AI Gateway, and any
        // other aggregator that exposes the `anthropic/` namespace).
        // Without this the aggregator route reports zero cached tokens,
        // which silently inflates cost vs. a direct vendor call.
        //
        // Marker placement (system tail / last user block / last stable
        // tool) is decided centrally in `crate::cache_policy`; this
        // adapter only emits the protocol-specific shape (`cache_control`
        // objects on JSON content blocks and tool entries) at the spots
        // that policy module identifies.
        let cache_spec = request.effective_cache_spec();
        let cache_retention = cache_spec.retention;
        let anthropic_caching =
            cache_retention != CacheRetention::None && supports_anthropic_caching(&request.model);
        let cache_control = anthropic_caching.then(|| ephemeral_marker(cache_retention));
        // Canonicalize cross-provider tool-call ids and synthesize
        // placeholders for orphan tool results BEFORE the
        // chat-completions message rewrite. Aggregator routes (PortKey
        // + OpenRouter especially) frequently see mixed-provider ids
        // when the user swaps models mid-session, and Anthropic-via-
        // aggregator routes reject `tool_call_id`s that don't match
        // the Anthropic regex + length cap. Indices computed below
        // (cache-control breakpoint) are over the *normalized* slice
        // so the synthetic placeholder shifts later positions
        // accordingly.
        let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
        // Find the last user-text turn so we can mark it as the cache
        // breakpoint. Anthropic caches everything *before* a marker, so the
        // last user message is the natural place.
        let last_user_text_index = cache_control.as_ref().and_then(|_| {
            normalized_input
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, item)| {
                    matches!(item, LlmInputItem::UserText(_)).then_some(index)
                })
        });

        let mut messages = Vec::with_capacity(normalized_input.len() + 1);
        let trimmed_instructions = request.instructions.trim();
        if !trimmed_instructions.is_empty() {
            if anthropic_caching {
                messages.push(json!({
                    "role": "system",
                    "content": json_markers::system_array_with_marker(
                        &request.instructions,
                        cache_retention,
                    ),
                }));
            } else {
                messages.push(json!({
                    "role": "system",
                    "content": &*request.instructions,
                }));
            }
        }
        for (index, item) in normalized_input.iter().enumerate() {
            let attach_cache_control = if Some(index) == last_user_text_index {
                cache_control.as_ref()
            } else {
                None
            };
            if let Some(msg) = chat_message(item, attach_cache_control) {
                messages.push(msg);
            }
        }
        let mut body = json!({
            "model": &*request.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
            // H-24: pin `n: 1` so an upstream default of `n: 2` (rare
            // but legal under Chat Completions) cannot silently double-
            // bill. The streamed parser only honours `choices[0]`; any
            // additional choices would be dropped, including their
            // tool calls. Emitting the field explicitly guarantees the
            // server-side count matches what we read.
            "n": 1,
        });
        if let Some(max_tokens) = request.max_output_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        // X-05: forward `output_schema` as the chat-completions
        // `response_format: { type: "json_schema", json_schema: { ... } }`
        // shape so providers that honour structured outputs
        // (OpenAI through any aggregator, Together, Mistral, Groq)
        // see the JSON schema. The OpenAI Responses provider has
        // already emitted this via `text.format` on its native
        // path; the chat-completions path was a silent gap.
        if let Some(schema) = request.output_schema.as_ref() {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {
                    "name": schema.name,
                    "schema": schema.schema,
                    "strict": schema.strict,
                }
            });
        }
        if let Some(effort) = request.reasoning_effort {
            emit_reasoning_hints(&mut body, preset, &request.model, effort);
        }
        if let Some(key) = cache_spec.key.as_deref() {
            // OpenAI's Chat Completions / Responses APIs honor a top-level
            // `prompt_cache_key` that groups requests for prompt-cache
            // affinity. OpenRouter forwards the field verbatim to OpenAI-
            // hosted models (`openai/*`), and other aggregator routes ignore
            // unknown body fields, so emitting it unconditionally costs
            // nothing and recovers cached-input billing for OpenAI-via-
            // OpenRouter traffic that the Anthropic-only `cache_control`
            // path above does not cover.
            //
            // H-33: callers that derive the affinity key from a full
            // path hash (or similar) hit the 64-codepoint limit
            // OpenAI silently enforces. Truncation collides those
            // keys (`abc/path/very/long/one` and
            // `abc/path/very/long/two` clamp to the same prefix),
            // mixing the cache across distinct sessions. Hash long
            // keys via SHA-256 → first 32 hex chars instead;
            // collision risk is negligible and the cache stays
            // partitioned by the caller's intent. Short keys round-
            // trip unchanged so existing callers keep their human-
            // readable identifiers.
            body["prompt_cache_key"] = json!(stable_prompt_cache_key(key));
        }
        if cache_retention == CacheRetention::Long && !preset_rejects_prompt_cache_retention(preset)
        {
            // Mirror the OpenAI native provider's extended-retention opt-in
            // so OpenAI-hosted models proxied via an aggregator (OpenRouter
            // `openai/*`, Vercel AI Gateway, etc.) still get the 24h
            // window. Anthropic-hosted aggregator routes have already
            // emitted the `ttl: "1h"` marker above; OpenAI ignores
            // `prompt_cache_retention` from non-OpenAI flavors.
            //
            // H-56: Mistral 422s on any unknown body field, so
            // `prompt_cache_retention` would break every request that
            // flips the Long-retention knob on a Mistral preset.
            body["prompt_cache_retention"] = json!("24h");
        }
        if !request.tools.is_empty() {
            let mut tool_values: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect();
            if anthropic_caching {
                // Anthropic-via-aggregator caches the tool prefix the same
                // way the native Anthropic API does. Without this marker
                // the aggregator route reports zero cached tokens on
                // every turn that re-sends the same tool list — the
                // common multi-turn coding case. The shared cache-policy
                // helper picks the breakpoint index (skipping any
                // mcp__-prefixed dynamic tools the registry pushed to
                // the tail) so this adapter and the native Anthropic
                // adapter cannot drift on which entry gets the marker.
                if let Some(idx) =
                    last_stable_tool_index(request.tools.iter().map(|tool| tool.name.as_str()))
                    && let Some(obj) = tool_values.get_mut(idx).and_then(Value::as_object_mut)
                {
                    obj.insert(
                        "cache_control".to_string(),
                        ephemeral_marker(cache_retention),
                    );
                }
            }
            body["tools"] = Value::Array(tool_values);
            // Forward `tool_choice` when the caller set one. Omitting the
            // field leaves the provider's default in place (typically
            // `auto`), which preserves historical behavior for working
            // models. Tool-shy models routed through aggregators (Qwen
            // via OpenRouter, smaller MoEs) ignore `auto` and emit a
            // chatty preamble with zero tool calls; setting
            // `tool_choice = "required"` in `[model]` flips them into
            // calling at least one tool per turn by passing the field
            // through verbatim.
            if let Some(choice) = request.tool_choice.as_deref() {
                body["tool_choice"] = normalize_tool_choice(preset, choice);
            }
            // H-32: forward `parallel_tool_calls` when the caller
            // explicitly set it. OpenAI's Responses provider already
            // honors this; aggregator routes that proxy to OpenAI
            // (OpenRouter, Vercel, PortKey) accept it and translate;
            // routes that don't recognize the field ignore it. The
            // field is `None` by default so we don't pin a per-route
            // policy.
            if let Some(parallel) = request.parallel_tool_calls {
                body["parallel_tool_calls"] = json!(parallel);
            }
        }
        body
    }
}

/// Coarse classification of an OpenAI-compatible model namespace.
///
/// The Chat-Completions transport speaks one wire shape, but each upstream
/// vendor has small quirks (Anthropic-style `cache_control` markers,
/// `reasoning_effort` shapes, etc.). Branching on a typed flavor instead of
/// re-running `model.starts_with("anthropic/")` everywhere keeps the matrix
/// reviewable and makes adding a new aggregator namespace a one-line edit
/// to [`COMPAT_TABLE`].
///
/// `OpenAi`, `GoogleCompat`, `XaiCompat`, and `Generic` are descriptive
/// today — production code only branches on `AnthropicCompat` via the
/// `supports_cache_control` flag — but exposed so the next per-vendor
/// branch in `request_body` has a typed slot to attach to instead of
/// growing a fresh `starts_with` test.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompatFlavor {
    /// Vanilla OpenAI completions/responses shape (`openai/*` via an
    /// aggregator). Honors `prompt_cache_key`, `reasoning_effort`.
    OpenAi,
    /// Anthropic `/v1/messages`-style shape proxied over chat-completions
    /// (`anthropic/*`). Honors ephemeral `cache_control` markers on text
    /// blocks and the system prompt.
    AnthropicCompat,
    /// Google Gemini routed via an OpenAI-compatible aggregator
    /// (`google/*`).
    GoogleCompat,
    /// xAI Grok routed via an OpenAI-compatible aggregator (`xai/*`).
    XaiCompat,
    /// Unknown namespace. Treated as best-effort: no cache markers, no
    /// vendor-specific flags. Matches the historical default for any model
    /// id that did not start with a recognized prefix.
    Generic,
}

/// Per-namespace compatibility row. A typed set of capability flags
/// drives wire-shape choices without scattering substring tests across
/// the request builder.
///
/// `flavor`, `supports_tool_calls`, and `supports_reasoning` are read by
/// the unit tests in `compatible_tests.rs` and exposed for the next
/// per-vendor branch to consume; production code today only reads
/// `supports_cache_control`. The `#[allow(dead_code)]` keeps the typed
/// surface intact without lying about which fields are wired up.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct CompatEntry {
    /// Lowercase model-id prefix. Matched against `model.to_ascii_lowercase()`
    /// so user-supplied casing does not bypass the table.
    pub model_prefix: &'static str,
    pub flavor: CompatFlavor,
    /// Function-calling support. Today every recognized flavor supports
    /// tools; the flag is here so future entries (e.g. a tool-less namespace)
    /// can be declared without breaking the table shape.
    pub supports_tool_calls: bool,
    /// Whether the aggregator forwards Anthropic-style ephemeral
    /// `cache_control` markers on text content parts. Drives the
    /// `supports_anthropic_caching` decision in [`OpenAiCompatibleProvider::request_body`].
    pub supports_cache_control: bool,
    /// Whether models in this namespace honor `reasoning_effort` /
    /// `reasoning.effort`. Currently descriptive only — the request builder
    /// emits both shapes unconditionally because aggregators ignore unknown
    /// fields — but exposed here so future per-flavor branching has a
    /// single place to consult.
    pub supports_reasoning: bool,
}

/// Single source of truth for OpenAI-compatible namespace quirks. Order
/// matters: the table is walked top-to-bottom and the first prefix match
/// wins, so list more-specific aggregator prefixes (e.g. a future
/// `openrouter/anthropic/`) before broader vendor prefixes.
///
/// Adding a new aggregator namespace is a one-line edit: append a row and
/// no further changes are needed in `request_body` or the stream path.
pub(crate) static COMPAT_TABLE: &[CompatEntry] = &[
    CompatEntry {
        model_prefix: "anthropic/",
        flavor: CompatFlavor::AnthropicCompat,
        supports_tool_calls: true,
        supports_cache_control: true,
        supports_reasoning: true,
    },
    CompatEntry {
        model_prefix: "openai/",
        flavor: CompatFlavor::OpenAi,
        supports_tool_calls: true,
        supports_cache_control: false,
        supports_reasoning: true,
    },
    CompatEntry {
        model_prefix: "google/",
        flavor: CompatFlavor::GoogleCompat,
        supports_tool_calls: true,
        supports_cache_control: false,
        supports_reasoning: true,
    },
    CompatEntry {
        model_prefix: "xai/",
        flavor: CompatFlavor::XaiCompat,
        supports_tool_calls: true,
        supports_cache_control: false,
        supports_reasoning: true,
    },
];

/// Classify a model id into a [`CompatFlavor`]. Returns
/// [`CompatFlavor::Generic`] for any namespace not represented in
/// [`COMPAT_TABLE`], preserving the historical "fall through with best-effort
/// defaults" behavior. Today this is exercised by the unit tests and is the
/// recommended entry point for adding the next per-vendor branch — production
/// code currently reads the more specific `supports_cache_control` flag
/// directly via [`compat_entry`].
#[allow(dead_code)]
pub(crate) fn classify(model: &str) -> CompatFlavor {
    compat_entry(model)
        .map(|entry| entry.flavor)
        .unwrap_or(CompatFlavor::Generic)
}

/// Look up the full [`CompatEntry`] for a model id, or `None` when no
/// prefix in [`COMPAT_TABLE`] matches.
pub(crate) fn compat_entry(model: &str) -> Option<&'static CompatEntry> {
    let lowered = model.to_ascii_lowercase();
    COMPAT_TABLE
        .iter()
        .find(|entry| lowered.starts_with(entry.model_prefix))
}

/// Whether the destination model honors Anthropic-style ephemeral
/// `cache_control` markers on text content parts. The decision is read
/// directly from [`COMPAT_TABLE`] so this file no longer carries an
/// ad-hoc `starts_with("anthropic/")` substring test — see F11.
///
/// Direct Anthropic calls do not go through this client (the native
/// Anthropic provider handles them with its own cache markers).
fn supports_anthropic_caching(model: &str) -> bool {
    compat_entry(model).is_some_and(|entry| entry.supports_cache_control)
}

impl LlmProvider for OpenAiCompatibleProvider {
    fn name(&self) -> &'static str {
        self.preset.as_str()
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        if let Err(err) = request.ensure_vision_support(self.preset.as_str()) {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let transport = self.transport;
        // C-12 follow-up: resolve any `{provider}` placeholder that
        // survived construction (left for the AI Gateway preset's
        // per-request upstream routing) before appending the
        // chat-completions suffix. `resolve_provider_segment` is a
        // no-op when the URL contains no placeholder, so non-CF
        // routes pay nothing for this hop.
        let resolved_base = resolve_provider_segment(&self.base_url, &request.model);
        let url = format!("{resolved_base}/chat/completions");
        let extra_headers = self.extra_headers.clone();
        let preset = self.preset;
        // We previously auto-injected `x-portkey-provider` from a
        // `vendor/model` prefix. That guessed at an OpenRouter-style
        // routing semantic that PortKey does not actually use — PortKey
        // accounts with attached integrations route by `@<integration>/<model>`
        // ids, not by a `x-portkey-provider` header. Sending the header
        // bypassed those integrations and produced misleading errors.
        // The model id now passes through verbatim; user-supplied
        // `providers.portkey.headers.*` still wins for the (rare) case
        // where a deployment really does want a config/virtual-key
        // header.
        let portkey_routing_configured = portkey_routing_header_present(&extra_headers);
        // H-54: llamacpp's chat-completions endpoint requires the
        // server to have been started with `--jinja` to honour the
        // tool-call schema. Without the flag the server 500s on
        // any request that carries `tools: [...]` with no
        // actionable error text. We attach a hint downstream.
        let llamacpp_tools_attempt =
            matches!(preset, OpenAiCompatiblePreset::LlamaCpp) && !request.tools.is_empty();
        let body = Self::request_body_for_preset(&request, preset);
        let provider_label = self.preset.display_name();
        // H-59: when the user opted into the PortKey canonical
        // `x-portkey-api-key` auth path (via the magic flag in
        // extra_headers, lifted in `from_config`), suppress the
        // Bearer header so the wire carries the PortKey key in
        // the header slot only. The Bearer slot is freed for
        // BYO-upstream-key flows.
        let suppress_bearer = extra_headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("x-portkey-api-key"));

        Box::pin(try_stream! {
            let response = send_with_auth_retry(
                &api_key,
                RetryPolicy::provider_requests(transport),
                &cancel,
                |key| {
                    let mut builder = client.post(&url);
                    // X-17: skip `bearer_auth(key)` when the resolved
                    // key is empty (local LM Studio / vLLM / llama.cpp
                    // unauthenticated presets — reqwest panics on
                    // `bearer_auth("")`).
                    // H-59: also skip when the PortKey canonical-auth
                    // path opted into `x-portkey-api-key`, so the
                    // Bearer slot stays free for a BYO upstream key.
                    if !key.is_empty() && !suppress_bearer {
                        builder = builder.bearer_auth(key);
                    }
                    for (header_key, header_value) in &extra_headers {
                        builder = builder.header(header_key.as_str(), header_value.as_str());
                    }
                    builder.json(&body)
                },
            )
            .await?;

            let status = response.status();
            let response = if status == StatusCode::OK {
                response
            } else {
                let raw_body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "failed to read error response".to_string());
                let message = match serde_json::from_str::<Value>(&raw_body) {
                    Ok(value) if value.get("error").is_some() => {
                        format_chat_error(&value, &raw_body)
                    }
                    _ => raw_body.clone(),
                };
                // PortKey returns 400 about `x-portkey-*` whenever it
                // can't figure out which upstream to dial. The most
                // common cause on integration-style PortKey accounts is
                // that the model id is missing the `@<integration>/` prefix
                // (e.g. `gpt-4o` instead of `@open-ai/gpt-4o`). The other
                // case is a deployment that wants a routing header.
                // `portkey_routing_configured` lets the second case
                // suppress the "use a routing header" half of the hint.
                let hint = if matches!(preset, OpenAiCompatiblePreset::PortKey)
                    && status == StatusCode::BAD_REQUEST
                    && message.to_ascii_lowercase().contains("x-portkey")
                {
                    if portkey_routing_configured {
                        " — hint: a routing header is set in providers.portkey.headers \
                         but PortKey still rejected. Check that the header value \
                         (config id / virtual key / provider) actually exists in your \
                         PortKey workspace."
                    } else {
                        " — hint: PortKey routes by either an `@<integration>/<model>` \
                         prefix on the model id (call `GET https://api.portkey.ai/v1/models` \
                         with your key to see what's available — e.g. `@open-ai/gpt-4o-mini`, \
                         `@openrouter/<vendor>/<model>`) or by a header in \
                         providers.portkey.headers (x-portkey-config / x-portkey-virtual-key / \
                         x-portkey-provider). Set one of those and retry."
                    }
                } else if llamacpp_tools_attempt
                    && status.is_server_error()
                {
                    // H-54: llamacpp's chat-completions surface 500s on
                    // any tool-call request when the server is not
                    // started with `--jinja`. Surface the actionable
                    // hint so users don't have to grep upstream logs
                    // to discover the flag.
                    " — hint: llama.cpp requires `--jinja` on the server command line \
                     to honour OpenAI-style `tools` payloads. Restart `llama-server` \
                     with `--jinja --chat-template-file /path/to/template.jinja` (or \
                     the bundled `chatml` template) and retry."
                } else {
                    local_jit_load_hint(preset, status, &message)
                };
                Err(SqueezyError::ProviderRequest(format!(
                    "{provider_label} {status}: {message}{hint}"
                )))?;
                unreachable!("provider error returned above");
            };

            yield LlmEvent::Started;

            let mut decoder = SseDecoder::default();
            let mut state = StreamState {
                // H-43: opt into inline `<think>` extraction for CF
                // Workers AI (DeepSeek-R1-distill / Kimi K2.6 /
                // Gemma 4 ship reasoning that way on Cloudflare's
                // OpenAI-compat path because no `reasoning_content`
                // field is exposed).
                extract_inline_think: matches!(preset, OpenAiCompatiblePreset::CloudflareWorkersAi),
                ..StreamState::default()
            };
            let mut server_model_echo = crate::ServerModelEcho::default();
            let mut bytes = response.bytes_stream();

            loop {
                let polled = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield LlmEvent::Cancelled;
                        return;
                    }
                    next = timeout(idle_timeout(transport), bytes.next()) => next,
                };
                let next = polled.map_err(|_| {
                    SqueezyError::ProviderStream(format!(
                        "{provider_label} stream idle timeout",
                    ))
                })?;
                let Some(chunk) = next else { break; };
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for event in decoder.push(&chunk) {
                    let parsed = parse_chat_event(&event, &mut state)?;
                    if let Some(server) = state.server_model.take()
                        && let Some(echo) = server_model_echo.observe(&request.model, &server)
                    {
                        yield echo;
                    }
                    for emitted in parsed {
                        yield emitted;
                    }
                    if state.completed_emitted {
                        return;
                    }
                }
            }

            for event in decoder.finish() {
                let parsed = parse_chat_event(&event, &mut state)?;
                if let Some(server) = state.server_model.take()
                    && let Some(echo) = server_model_echo.observe(&request.model, &server)
                {
                    yield echo;
                }
                for emitted in parsed {
                    yield emitted;
                }
                if state.completed_emitted {
                    return;
                }
            }

            // The aggregator closed the stream without `[DONE]`. Drain
            // pending tool calls + reasoning so neither is lost, inject the
            // empty-output notice if nothing visible reached the user, then
            // emit Completed so the agent loop finishes cleanly.
            for emitted in state.drain_tool_calls()? {
                yield emitted;
            }
            if let Some(reasoning_done) = drain_reasoning(&mut state) {
                yield reasoning_done;
            }
            if !state.saw_visible_output && !state.completed_emitted {
                yield LlmEvent::TextDelta(
                    "\n[squeezy] stream ended without producing any content or tool call. The provider may have cut the connection mid-response; retry the turn.\n".to_string(),
                );
            }
            if !state.completed_emitted {
                // Truncated stream — no terminal finish_reason from the
                // upstream. Surface `None` so the agent loop / eval can
                // distinguish "stream cut" from a real provider-reported
                // stop. `reasoning_only_stop` stays false: we don't know
                // what the model intended in this case.
                let stop_reason = state.finish_reason.as_deref().map(chat_stop_reason);
                yield LlmEvent::Completed {
                    response_id: state.response_id.take(),
                    cost: state.cost.clone(),
                    stop_reason,
                    reasoning_only_stop: state.reasoning_only_stop,
                };
            }
        })
    }
}

fn chat_message(item: &LlmInputItem, cache_control: Option<&Value>) -> Option<Value> {
    Some(match item {
        LlmInputItem::UserText(text) => {
            if let Some(cc) = cache_control {
                json!({
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": text,
                            "cache_control": cc,
                        }
                    ],
                })
            } else {
                json!({
                    "role": "user",
                    "content": text,
                })
            }
        }
        LlmInputItem::AssistantText(text) => json!({
            "role": "assistant",
            "content": text,
        }),
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [{
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(arguments)
                        .unwrap_or_else(|_| "{}".to_string()),
                }
            }],
        }),
        LlmInputItem::FunctionCallOutput {
            call_id, output, ..
        } => json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": output,
        }),
        LlmInputItem::Image { media_type, bytes } => json!({
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": {
                    "url": format!(
                        "data:{media_type};base64,{}",
                        BASE64_STANDARD.encode(bytes.as_ref())
                    ),
                },
            }],
        }),
        // Chat Completions has no signed reasoning replay format. Reasoning
        // items are rendered in the UI but skipped when replaying.
        LlmInputItem::Reasoning(_) => return None,
        // Chat Completions has no first-class document content block;
        // Phase 4 may inline as a `file` part where the route supports
        // it. For now we skip with a debug log.
        LlmInputItem::Document { name, .. } => {
            tracing::debug!(
                target: "squeezy_llm::compatible",
                name = name.as_str(),
                "chat-completions document content block not yet implemented; skipping",
            );
            return None;
        }
    })
}

/// Resolve `{account_id}` / `{gateway_id}` placeholders in an
/// OpenAI-compatible base URL.
///
/// The Cloudflare Workers AI and AI Gateway presets ship base URL
/// templates with these placeholders so the configuration layer can
/// flow the per-account / per-gateway values through verbatim and
/// defer substitution until the LLM client is constructed (see
/// [`OpenAiCompatibleProvider::from_config`]). Keeping substitution
/// here — instead of building the URL eagerly in config land — lets a
/// user override `base_url` to point at a reverse proxy that mirrors
/// the Cloudflare path shape without re-implementing the substitution.
///
/// Returns the resolved URL when every placeholder present in
/// `template` has a corresponding non-empty value, or a
/// [`SqueezyError::ProviderNotConfigured`] error naming the missing
/// placeholder and the config field/env-var the user needs to set.
/// Templates that carry no placeholders are returned unchanged, which
/// keeps every non-Cloudflare preset on its existing zero-overhead
/// path.
pub(crate) fn substitute_url_placeholders(
    template: &str,
    preset: OpenAiCompatiblePreset,
    account_id: Option<&str>,
    gateway_id: Option<&str>,
) -> Result<String> {
    // Required-vs-optional is structural: a placeholder is "required"
    // exactly when it appears in the template, regardless of preset.
    // This keeps the substitution table preset-agnostic so a custom
    // base URL with `{account_id}` on a non-Cloudflare preset still
    // gets a helpful error instead of a literal `{account_id}` reaching
    // the wire.
    let section = preset.as_str();
    let preset_label = preset.display_name();
    let substitutions: [(&str, Option<&str>, &str, &str); 2] = [
        (
            "{account_id}",
            account_id.map(str::trim).filter(|value| !value.is_empty()),
            "cloudflare_account_id",
            "CLOUDFLARE_ACCOUNT_ID",
        ),
        (
            "{gateway_id}",
            gateway_id.map(str::trim).filter(|value| !value.is_empty()),
            "cloudflare_gateway_id",
            "CLOUDFLARE_AI_GATEWAY_ID",
        ),
    ];
    let mut resolved = template.to_string();
    for (placeholder, value, field, env_var) in substitutions {
        if !resolved.contains(placeholder) {
            continue;
        }
        let Some(value) = value else {
            return Err(SqueezyError::ProviderNotConfigured(format!(
                "providers.{section}.base_url contains the {placeholder} \
                 placeholder but providers.{section}.{field} (or {env_var}) \
                 is unset; the {preset_label} preset cannot resolve a request \
                 URL without it"
            )));
        };
        resolved = resolved.replace(placeholder, value);
    }
    // C-12 follow-up: the new CF AI Gateway REST URL shape
    // (`api.cloudflare.com/.../ai/v1/{provider}/v1/chat/completions`)
    // routes by the upstream provider in the URL path, but the
    // provider is a *per-request* property (derived from the model's
    // namespace prefix). Leave any `{provider}` placeholder in the
    // string untouched at construction time so the request-time
    // resolver in `stream_response` can substitute the right value
    // per turn (e.g. `openai/gpt-5.5` -> `openai`). Other presets
    // don't accept the placeholder — surface an explicit error so a
    // typo in `[providers.custom.base_url]` doesn't escape as a
    // literal `{provider}` segment.
    if resolved.contains("{provider}") && preset != OpenAiCompatiblePreset::CloudflareAiGateway {
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "providers.{section}.base_url contains the {{provider}} \
             placeholder but the {preset_label} preset does not support \
             per-request upstream routing; remove the placeholder or \
             switch to the cloudflare_ai_gateway preset"
        )));
    }
    Ok(resolved)
}

/// Resolve any `{provider}` placeholder that survived construction
/// (left in place by [`substitute_url_placeholders`] for the AI
/// Gateway preset) by deriving the upstream provider from the
/// model id's namespace prefix. Used at request build time so each
/// turn can route through the correct upstream without rebuilding
/// the provider.
///
/// Falls back to `workers-ai` for unprefixed models because that is
/// the only Cloudflare-direct upstream that the AI Gateway exposes
/// for Workers AI checkpoints (model ids like `@cf/meta/...`).
pub(crate) fn resolve_provider_segment(base_url: &str, model: &str) -> String {
    if !base_url.contains("{provider}") {
        return base_url.to_string();
    }
    let provider = if let Some(entry) = compat_entry(model) {
        // Strip the trailing slash to land on the segment name.
        entry.model_prefix.trim_end_matches('/').to_string()
    } else if model.starts_with("@cf/") {
        "workers-ai".to_string()
    } else if let Some((prefix, _)) = model.split_once('/') {
        prefix.to_ascii_lowercase()
    } else {
        // Default upstream for legacy callers that pass an
        // unprefixed model id. Cloudflare auto-discovers the
        // upstream when the segment is `compat`.
        "compat".to_string()
    };
    base_url.replace("{provider}", &provider)
}

fn portkey_routing_header_present(headers: &BTreeMap<String, String>) -> bool {
    // PortKey accepts any of these as the upstream-routing signal; if the
    // user already configured one, we don't override.
    const ROUTING_HEADERS: &[&str] = &[
        "x-portkey-provider",
        "x-portkey-virtual-key",
        "x-portkey-config",
    ];
    headers.keys().any(|key| {
        ROUTING_HEADERS
            .iter()
            .any(|needle| key.eq_ignore_ascii_case(needle))
    })
}

fn preset_default_headers(preset: OpenAiCompatiblePreset) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    if matches!(preset, OpenAiCompatiblePreset::OpenRouter) {
        // OpenRouter uses HTTP-Referer + X-Title to attribute traffic in its
        // ranking dashboard. Sending them lets the OpenRouter "Squeezy" entry
        // accumulate stats. Users can override via providers.openrouter.headers.
        headers.insert(
            "HTTP-Referer".to_string(),
            "https://github.com/esqueezy/squeezy".to_string(),
        );
        headers.insert("X-Title".to_string(), "Squeezy".to_string());
    }
    headers
}

#[derive(Debug, Default)]
struct StreamState {
    response_id: Option<String>,
    cost: CostSnapshot,
    /// H-43: when `true`, the parser scans content deltas for
    /// inline `<think>...</think>` blocks (CF Workers AI's
    /// DeepSeek-R1-distill / Kimi K2.6 / Gemma 4 ship reasoning
    /// inline on the OpenAI-compat path because no
    /// `reasoning_content` field is available). Tags that arrive
    /// split across chunks are stitched together via
    /// `think_tag_buf` and routed to `ReasoningDelta`.
    extract_inline_think: bool,
    /// Whether we are currently inside a `<think>...</think>`
    /// block. Driven by [`split_inline_think`].
    inside_think: bool,
    /// Buffers a partial `<think>` or `</think>` opening/closing
    /// tag that arrived split across delta chunks.
    think_tag_buf: String,
    /// Tool-call accumulator partitioned by `(choice_index,
    /// tool_index)`. H-24: aggregators that relay multi-choice
    /// streams (rare today since we pin `n: 1` but legal under
    /// Chat Completions) and providers that occasionally omit the
    /// `index` field on continuation deltas (some Anthropic-via-
    /// aggregator relays of `content_block_delta`) would otherwise
    /// collapse the second call's args into the first. The
    /// `(choice_index, tool_index)` key separates choices and the
    /// `latest_tool_index_per_choice` map covers the missing-index
    /// case by treating it as a continuation of the most recent
    /// active index on that choice.
    tool_calls: BTreeMap<(usize, usize), PartialToolCall>,
    /// Tracks the highest-seen `tool_calls[].index` value per
    /// choice so a continuation delta whose `index` field is
    /// missing routes to the matching accumulator instead of
    /// silently appending to `index=0`.
    latest_tool_index_per_choice: BTreeMap<usize, usize>,
    completed_emitted: bool,
    /// Captured OpenAI chat-completions `finish_reason` from the last
    /// streamed choice, so the agent's turn loop sees a normalized
    /// stop reason for compatibility providers too.
    finish_reason: Option<String>,
    /// Accumulates `reasoning_content` / `reasoning` text streamed across
    /// chat-completions deltas. Drained into a `ReasoningDone` event when
    /// the stream finishes so the agent loop persists the segment to the
    /// conversation history and the TUI promotes the live "thinking"
    /// buffer into a permanent transcript entry. Without this, providers
    /// routed through chat-completions (PortKey, OpenRouter, DeepSeek,
    /// Qwen, etc.) emitted reasoning deltas but never a Done event, so
    /// the TUI cleared the live buffer on turn completion and the text
    /// vanished.
    reasoning_buf: String,
    /// Whether any user-visible signal has surfaced this stream. Set
    /// `true` on the first non-empty content delta OR on the first
    /// tool-call delta carrying a function name. Reasoning deltas do
    /// *not* count: a reasoning-only response (Qwen3-style: model
    /// thinks, finishes with `stop`, no content or tool calls) is
    /// exactly the case we want to detect so we can inject a visible
    /// notice instead of completing with an empty assistant message.
    saw_visible_output: bool,
    /// `true` iff a `finish_reason="stop"` was observed while
    /// `saw_visible_output` was false AND the reasoning buffer had any
    /// content. Latched once because subsequent choices on a multi-choice
    /// stream don't clear the marker. Distinct from `finish_reason`
    /// because the normalized `StopReason::EndTurn` alone can't
    /// disambiguate "clean stop" from "reasoning-only stop".
    reasoning_only_stop: bool,
    /// Buffers the server-echoed top-level `model` field of a
    /// chat-completions chunk the first time the stream surfaces it.
    /// The outer `stream_response` loop drains the slot after each
    /// `parse_chat_event` call and feeds the value into
    /// [`crate::ServerModelEcho`] so [`LlmEvent::ServerModel`] lands
    /// additively right after [`LlmEvent::Started`]. Stays `None` for
    /// every later chunk because the helper short-circuits once the
    /// echo has been latched.
    server_model: Option<String>,
}

/// Upper bound on the accumulated tool-call arguments string for a
/// single call. H-25: a misbehaving upstream that keeps streaming
/// `function.arguments` deltas without ever sending a
/// `finish_reason` could grow the buffer to gigabytes before the
/// idle timeout fires. 1 MiB is well above any real tool's
/// argument payload (OpenAI documents 8 KiB as the practical limit)
/// while still leaving ample headroom for adversarial inputs to
/// fall harmlessly into the invalid-arguments path.
const MAX_TOOL_ARGUMENTS_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
struct PartialToolCall {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    /// Latches `true` the first time an arguments delta is dropped
    /// because the accumulator would have exceeded
    /// [`MAX_TOOL_ARGUMENTS_BYTES`]. `drain_tool_calls` consults
    /// this to surface an `INVALID_TOOL_ARGUMENTS_*` envelope
    /// instead of pretending the call succeeded with truncated
    /// args.
    arguments_overflow: bool,
}

impl StreamState {
    /// Accumulate a streamed `tool_calls[]` delta from `choice_index`
    /// into the per-`(choice, tool)` partition.
    ///
    /// `tool_index` is `None` when the upstream omitted the `index`
    /// field on a continuation delta — Anthropic-via-aggregator
    /// relays of `content_block_delta` and some PortKey upstreams
    /// drop the field after the first chunk. H-24: route those to
    /// the most-recently-seen active index on the same choice
    /// instead of silently defaulting to `0`, which would
    /// concatenate a second parallel call's arguments into the
    /// first call's accumulator.
    fn accumulate_tool_call(
        &mut self,
        choice_index: usize,
        tool_index: Option<usize>,
        delta: &Value,
    ) {
        let resolved_tool_index = match tool_index {
            Some(idx) => {
                self.latest_tool_index_per_choice
                    .entry(choice_index)
                    .and_modify(|current| *current = (*current).max(idx))
                    .or_insert(idx);
                idx
            }
            None => *self
                .latest_tool_index_per_choice
                .get(&choice_index)
                .unwrap_or(&0),
        };
        let key = (choice_index, resolved_tool_index);
        let entry = self.tool_calls.entry(key).or_default();
        if let Some(id) = delta.get("id").and_then(Value::as_str) {
            entry.call_id = Some(id.to_string());
        }
        if let Some(function) = delta.get("function") {
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                let acc = entry.name.get_or_insert_with(String::new);
                acc.push_str(name);
                // A tool-call delta carrying a function name is the model
                // committing to actionable output. Latch the visibility
                // signal so we suppress the no-output notice even if the
                // stream cuts before arguments fully arrive (incomplete
                // tool calls are handled defensively in drain_tool_calls).
                self.saw_visible_output = true;
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                // H-25: cap the accumulator so a pathological
                // stream that keeps shipping arguments deltas
                // without ever sending finish_reason cannot grow
                // the buffer to gigabytes. Once we cross the
                // ceiling we keep parsing the rest of the stream
                // (so finish_reason / [DONE] still land) but stop
                // appending — `drain_tool_calls` will surface the
                // INVALID_TOOL_ARGUMENTS envelope so the agent
                // loop can decide to retry or abort.
                if entry.arguments.len().saturating_add(arguments.len()) > MAX_TOOL_ARGUMENTS_BYTES
                {
                    if !entry.arguments_overflow {
                        tracing::warn!(
                            target: "squeezy_llm::compatible",
                            choice_index,
                            tool_index = resolved_tool_index,
                            cap_bytes = MAX_TOOL_ARGUMENTS_BYTES,
                            "tool-call arguments accumulator hit cap; further deltas dropped"
                        );
                    }
                    entry.arguments_overflow = true;
                } else {
                    entry.arguments.push_str(arguments);
                }
            }
        }
    }

    fn drain_tool_calls(&mut self) -> Result<Vec<LlmEvent>> {
        let mut events = Vec::new();
        let drained = std::mem::take(&mut self.tool_calls);
        self.latest_tool_index_per_choice.clear();
        for ((_choice_index, index), partial) in drained {
            let call_id = partial.call_id.unwrap_or_else(|| format!("call_{index}"));
            // Skip incomplete tool calls (no function.name accumulated)
            // instead of erroring the whole stream. PortKey / OpenRouter /
            // Qwen sometimes ship a tool-call delta whose name chunk goes
            // missing or whose stream cuts mid-call. Erroring here would
            // discard any assistant text the model already produced and
            // halt the turn. Drop the partial entry, complete the turn
            // with whatever did surface, let the model retry next turn.
            // A short stderr warning makes the drop traceable.
            let Some(name) = partial.name else {
                // Surface the drop both to stderr (for `tail -f
                // ~/.cache/squeezy-tui-debug.log`) AND via the
                // structured `tracing` channel so eval harnesses can
                // count it with `RUST_LOG=squeezy_llm=warn`. The drop is
                // silent to the user otherwise and is a likely
                // contributor to the "model said it'd call X then the
                // turn ended" pattern on Qwen-class models.
                eprintln!(
                    "squeezy: skipping incomplete chat-completions tool call at index {index} (call_id={call_id}, no function name in stream)"
                );
                tracing::warn!(
                    target: "squeezy_llm::compatible",
                    index,
                    call_id = %call_id,
                    "dropped incomplete chat-completions tool call (no function name)"
                );
                continue;
            };
            // M-29: empty `function.arguments` means the model intends a
            // zero-arg call; emit `Value::Null` instead of fabricating an
            // empty object. The tool dispatch layer can disambiguate
            // "model sent no arguments" from "model sent `{}`" and a few
            // tool implementations (notably ones that gate on
            // `value.is_null()`) need that signal. The parse-failure
            // branch still applies to genuinely non-empty malformed JSON
            // so the existing `INVALID_TOOL_ARGUMENTS_*` markers continue
            // to flow through.
            let arguments = if partial.arguments.is_empty() {
                // M-29 (Phase 4FH-AB): an empty arguments string from
                // a zero-arg tool call surfaces as `Value::Null` so the
                // dispatcher can distinguish "model sent no arguments"
                // from "model sent `{}`". H-25 cap does not apply here
                // because the cap only latches when *deltas* arrive.
                Value::Null
            } else if partial.arguments_overflow {
                // H-25: if the accumulator hit the byte cap mid-stream
                // the args we have are necessarily truncated. Surface
                // the synthetic invalid-arguments envelope so the
                // agent loop can decide what to do (retry / abort)
                // instead of feeding the model a half-JSON blob that
                // would parse-fail later in the tool dispatcher.
                let arguments_text = partial.arguments;
                json!({
                    INVALID_TOOL_ARGUMENTS_KEY: true,
                    INVALID_TOOL_ARGUMENTS_ERROR_KEY: format!(
                        "tool-call arguments exceeded {MAX_TOOL_ARGUMENTS_BYTES} bytes; truncated"
                    ),
                    INVALID_TOOL_ARGUMENTS_RAW_KEY: arguments_text,
                })
            } else {
                let arguments_text = partial.arguments;
                serde_json::from_str::<Value>(&arguments_text).unwrap_or_else(|err| {
                    json!({
                        INVALID_TOOL_ARGUMENTS_KEY: true,
                        INVALID_TOOL_ARGUMENTS_ERROR_KEY: err.to_string(),
                        INVALID_TOOL_ARGUMENTS_RAW_KEY: arguments_text,
                    })
                })
            };
            events.push(LlmEvent::ToolCall(LlmToolCall {
                call_id,
                name,
                arguments,
            }));
        }
        Ok(events)
    }
}

/// Flush any reasoning text accumulated across delta events into a
/// `ReasoningDone` event so the agent loop persists the segment to
/// conversation history and the TUI promotes the live "thinking" buffer
/// into a permanent transcript entry. Uses the OpenAi payload variant as
/// a generic carrier — the chat-completions replay path drops
/// `LlmInputItem::Reasoning` items entirely, so the variant choice only
/// affects display, never the wire format on the next turn.
/// Map OpenAI Chat-Completions `finish_reason` strings to the normalized
/// [`crate::StopReason`]. Shared by the Responses-style streaming path and
/// the legacy chat-completions path in this provider.
fn chat_stop_reason(value: &str) -> crate::StopReason {
    match value {
        "stop" => crate::StopReason::EndTurn,
        "tool_calls" | "function_call" => crate::StopReason::ToolUse,
        "length" => crate::StopReason::MaxTokens,
        "content_filter" => crate::StopReason::Refusal,
        other => crate::StopReason::Other(other.to_string()),
    }
}

fn drain_reasoning(state: &mut StreamState) -> Option<LlmEvent> {
    let text = std::mem::take(&mut state.reasoning_buf);
    if text.trim().is_empty() {
        return None;
    }
    Some(LlmEvent::ReasoningDone(ReasoningPayload::OpenAi {
        item_id: String::new(),
        summary: vec![text],
        encrypted_content: None,
    }))
}

/// Flatten a Chat-Completions delta field that may be a plain string or an
/// array of structured content parts into a single string. The spec says
/// `content` and `reasoning_content` are strings, but live aggregator routes
/// (notably Qwen via OpenRouter/PortKey, Anthropic-via-aggregator routes that
/// echo the Responses content-part shape) sometimes stream them as arrays
/// of `{type, text}` or `{type, delta}` objects. The old `as_str`-only path
/// silently dropped those entire deltas — billed output tokens with no text
/// ever surfacing to the agent loop.
///
/// Accepts the union of shapes we have seen on real traffic: a bare string;
/// an array whose elements expose either a `text` or `delta` string field
/// (regardless of `type`, which varies — `text`, `output_text`,
/// `output_text_delta`, `text_delta`, `reasoning`, etc).
fn collect_delta_text(value: Option<&Value>) -> Option<Cow<'_, str>> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Some(Cow::Borrowed(text)),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                } else if let Some(delta) = part.get("delta").and_then(Value::as_str) {
                    out.push_str(delta);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Cow::Owned(out))
            }
        }
        _ => None,
    }
}

fn collect_reasoning_delta(delta: &Value) -> Option<Cow<'_, str>> {
    match (
        collect_delta_text(delta.get("reasoning_content")),
        collect_delta_text(delta.get("reasoning")),
    ) {
        (Some(left), Some(right)) => {
            let mut text = String::with_capacity(left.len() + right.len());
            text.push_str(&left);
            text.push_str(&right);
            Some(Cow::Owned(text))
        }
        (Some(text), None) | (None, Some(text)) => Some(text),
        (None, None) => None,
    }
}

/// True for the OpenAI-compatible presets that target a server running
/// on the user's own machine (LM Studio, vLLM, llama.cpp). These presets
/// (1) run unauthenticated by default, so credential resolution must
/// tolerate the "no key configured" terminal (see X-17 in
/// [`OpenAiCompatibleProvider::from_config`]), and (2) JIT-load
/// checkpoints on demand, so a `400 Bad Request` with a "not loaded"
/// message wants a provider-specific load-CLI hint
/// (see [`local_jit_load_hint`]).
fn is_local_preset(preset: OpenAiCompatiblePreset) -> bool {
    matches!(
        preset,
        OpenAiCompatiblePreset::LMStudio
            | OpenAiCompatiblePreset::VLlm
            | OpenAiCompatiblePreset::LlamaCpp
    )
}

/// Append a JIT-load hint to a 400 error body from a local OpenAI-compatible
/// server (LM Studio, vLLM, llama.cpp). All three return `400 Bad Request`
/// with a message containing "not loaded" / "model not loaded" / "no models
/// loaded" when the user pointed `model = "<id>"` at a checkpoint the server
/// hasn't loaded into memory yet. Returns an inline hint string the caller
/// concatenates onto the surfaced error message; returns `""` when the
/// preset / status / message combination does not match. The hint points
/// the user at the provider-appropriate fix (`lms load`, `vllm serve`,
/// `llama-server -m`) instead of the bare upstream complaint.
fn local_jit_load_hint(
    preset: OpenAiCompatiblePreset,
    status: StatusCode,
    message: &str,
) -> &'static str {
    if status != StatusCode::BAD_REQUEST || !is_local_preset(preset) {
        return "";
    }
    let lower = message.to_ascii_lowercase();
    if !(lower.contains("not loaded")
        || lower.contains("no models loaded")
        || lower.contains("model is not loaded"))
    {
        return "";
    }
    match preset {
        OpenAiCompatiblePreset::LMStudio => {
            " — hint: LM Studio rejected the request because the requested model is not loaded. \
             Open the LM Studio app, switch to the Developer tab, and load the checkpoint, or run \
             `lms load <model>` from the LM Studio CLI before retrying."
        }
        OpenAiCompatiblePreset::VLlm => {
            " — hint: the vLLM server is running but the requested model is not loaded. \
             Restart `vllm serve` with `--model <id>` pointing at the checkpoint you want to call, \
             or pick a model id that matches what the server already advertises via `GET /v1/models`."
        }
        OpenAiCompatiblePreset::LlamaCpp => {
            " — hint: llama.cpp server has no model loaded. Start it with `llama-server -m <path>` \
             pointing at the GGUF file you want to serve, then retry."
        }
        _ => "",
    }
}

/// X-04: per-preset normalization for `tool_choice`. Most
/// aggregators accept the OpenAI shape verbatim
/// (`"auto" / "none" / "required" / {type, function}`). Mistral
/// renamed `required` to `any` and 422s on the OpenAI value; map
/// it client-side so user-facing configs stay uniform.
fn normalize_tool_choice(preset: OpenAiCompatiblePreset, choice: &str) -> Value {
    if matches!(preset, OpenAiCompatiblePreset::Mistral) && choice.eq_ignore_ascii_case("required")
    {
        return json!("any");
    }
    json!(choice)
}

/// H-39 / H-49 / H-52 / H-55 / H-62 / H-65: per-preset reasoning-
/// hint emission. The vendors diverge on how to enable thinking
/// mode and on the field names; one centralized branch keeps the
/// matrix reviewable.
///
/// The legacy default emits both `reasoning_effort` and
/// `reasoning: { effort }` because the historical comment was that
/// aggregators ignore unknown fields. May 2026 verification shows
/// that's true for OpenRouter / xAI / generic upstreams, but
/// Mistral, Vercel, Vertex, DeepSeek-V4, Baseten/vLLM/llamacpp,
/// and Groq all want different shapes; emitting the OpenAI legacy
/// form on those breaks the request (Mistral 422) or silently
/// drops the hint (Vercel/Vertex/DeepSeek/Baseten/vLLM/llamacpp).
fn emit_reasoning_hints(
    body: &mut Value,
    preset: OpenAiCompatiblePreset,
    model: &str,
    effort: squeezy_core::ReasoningEffort,
) {
    let effort_str = effort.as_str();
    match preset {
        OpenAiCompatiblePreset::Mistral => {
            // H-55: Mistral 422s on the nested `reasoning` form and
            // only honors a top-level `reasoning_effort` clamped to
            // its enum `none | high`. Map intermediate effort values
            // to `high` so a `Low`/`Medium` setting still flips
            // thinking on (the only other option is to drop the
            // hint, which is worse — silently disables thinking).
            body["reasoning_effort"] = json!("high");
        }
        OpenAiCompatiblePreset::Vercel => {
            // H-62: Vercel rejects squeezy's top-level
            // `reasoning_effort` outright; the hint must ride under
            // `providerOptions.{anthropic|openai}` keyed off the
            // upstream the gateway is dialing. Pick the shape from
            // the model id's namespace prefix; fall back to the
            // OpenAI shape for unrecognized prefixes.
            let provider_opts = body
                .as_object_mut()
                .expect("body is a JSON object")
                .entry("providerOptions".to_string())
                .or_insert_with(|| json!({}));
            let lower = model.to_ascii_lowercase();
            if lower.starts_with("anthropic/") {
                let budget = match effort {
                    squeezy_core::ReasoningEffort::Low => 4096,
                    squeezy_core::ReasoningEffort::Medium => 8192,
                    squeezy_core::ReasoningEffort::High => 16384,
                    squeezy_core::ReasoningEffort::XHigh => 32768,
                };
                provider_opts["anthropic"] = json!({ "thinkingBudget": budget });
            } else {
                // OpenAI-on-Vercel shape — both Camel-case fields
                // match Vercel's documented Responses API surface.
                provider_opts["openai"] = json!({
                    "reasoningEffort": effort_str,
                    "reasoningSummary": "auto",
                });
            }
        }
        OpenAiCompatiblePreset::Vertex => {
            // H-65: Vertex's OpenAI-compat layer translates
            // Gemini-thinking via `extra_body.google.thinking_config.thinking_budget`.
            // The top-level OpenAI `reasoning_effort` is silently
            // dropped today. Map effort to the documented budget
            // tokens.
            let budget = match effort {
                squeezy_core::ReasoningEffort::Low => 4096,
                squeezy_core::ReasoningEffort::Medium => 8192,
                squeezy_core::ReasoningEffort::High => 16384,
                squeezy_core::ReasoningEffort::XHigh => 32768,
            };
            let extra = body
                .as_object_mut()
                .expect("body is a JSON object")
                .entry("extra_body".to_string())
                .or_insert_with(|| json!({}));
            extra["google"] = json!({
                "thinking_config": {
                    "thinking_budget": budget,
                }
            });
        }
        OpenAiCompatiblePreset::DeepSeek => {
            // H-49: DeepSeek V4 controls thinking via the
            // `thinking` body field, not via `reasoning_effort`.
            // Map effort to the documented budget modes.
            let budget = match effort {
                squeezy_core::ReasoningEffort::Low => 2048,
                squeezy_core::ReasoningEffort::Medium => 8192,
                squeezy_core::ReasoningEffort::High => 16384,
                squeezy_core::ReasoningEffort::XHigh => 32768,
            };
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
        OpenAiCompatiblePreset::Groq => {
            // H-52: Groq controls reasoning per model family. The
            // gpt-oss-* family wants `include_reasoning: true`; Qwen
            // and DeepSeek-R1 derivatives want a string
            // `reasoning_format` enum. The two are mutually
            // exclusive (Groq rejects both together).
            let lower = model.to_ascii_lowercase();
            if lower.contains("gpt-oss") {
                body["include_reasoning"] = json!(true);
            } else if lower.contains("qwen") || lower.contains("deepseek") {
                body["reasoning_format"] = json!("parsed");
            } else {
                // Fall back to the OpenAI legacy shape for unknown
                // Groq models (e.g. Llama-3.x).
                body["reasoning_effort"] = json!(effort_str);
            }
        }
        OpenAiCompatiblePreset::Baseten
        | OpenAiCompatiblePreset::VLlm
        | OpenAiCompatiblePreset::LlamaCpp => {
            // H-39: Baseten + vLLM + llamacpp enable thinking via
            // a `chat_template_args.enable_thinking` flag the
            // jinja template consumes. The OpenAI-style
            // `reasoning_effort` does nothing on these servers
            // unless the template branches on it explicitly.
            let chat_template_args = body
                .as_object_mut()
                .expect("body is a JSON object")
                .entry("chat_template_args".to_string())
                .or_insert_with(|| json!({}));
            chat_template_args["enable_thinking"] = json!(true);
        }
        _ => {
            // OpenRouter / xAI / generic: emit both shapes — the
            // legacy top-level + the newer nested form. Aggregators
            // ignore unknown fields.
            body["reasoning_effort"] = json!(effort_str);
            body["reasoning"] = json!({ "effort": effort_str });
        }
    }
}

/// H-56: Mistral 422s on any unknown body field, including
/// `prompt_cache_retention`. The retention knob has no Mistral
/// equivalent today (Mistral cache TTL is server-managed), so the
/// safest path is to suppress the field on the Mistral preset
/// entirely. Other presets keep the OpenAI-compat opt-in.
fn preset_rejects_prompt_cache_retention(preset: OpenAiCompatiblePreset) -> bool {
    matches!(preset, OpenAiCompatiblePreset::Mistral)
}

/// H-33: derive a stable, collision-safe `prompt_cache_key` that
/// respects OpenAI's silent 64-codepoint limit. Short keys (≤64
/// chars) round-trip verbatim so existing callers keep their
/// human-readable identifiers (and the test fixtures continue to
/// match exact strings). Long keys hash via SHA-256 → first 32 hex
/// chars (well under the 64 limit, collision rate is 2^-128). The
/// existing `clamp_prompt_cache_key` helper still backs the
/// final truncation so the OpenAI native provider and this
/// adapter agree on the codepoint cap.
fn stable_prompt_cache_key(key: &str) -> String {
    if key.chars().count() <= 64 {
        return clamp_prompt_cache_key(key).to_string();
    }
    let digest = Sha256::digest(key.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        std::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}")).expect("hex write");
    }
    hex
}

/// H-27: classify an inline mid-stream `{"error":...}` envelope as
/// retryable vs terminal. Retryable shapes (`rate_limit_*`,
/// `server_error`, `overloaded`, `timeout`, HTTP 408/429/5xx) are
/// returned as plain `ProviderStream` errors so the existing
/// transport retry policy can take another swing. Terminal shapes
/// (auth, invalid request, context overflow, content filter)
/// surface with the [`NON_RETRYABLE_MARKER`] prefix so the agent
/// loop drops the turn instead of looping the same broken
/// request.
fn is_inline_error_retryable(value: &Value) -> bool {
    let error = value.get("error");
    let kind = error
        .and_then(|err| err.get("type"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let code = error
        .and_then(|err| err.get("code"))
        .and_then(|c| {
            c.as_str()
                .map(str::to_ascii_lowercase)
                .or_else(|| c.as_i64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    if let Some(kind) = kind.as_deref() {
        if kind.contains("rate_limit")
            || kind.contains("overloaded")
            || kind.contains("server_error")
            || kind.contains("api_error")
            || kind.contains("timeout")
            || kind.contains("transient")
            || kind.contains("internal")
        {
            return true;
        }
        if kind.contains("auth")
            || kind.contains("permission")
            || kind.contains("invalid_request")
            || kind.contains("not_found")
            || kind.contains("content_filter")
            || kind.contains("context_length")
            || kind.contains("quota")
        {
            return false;
        }
    }
    // Code-based classification covers providers that only ship a
    // `code` field (OpenRouter, some PortKey upstreams) without a
    // `type`. Common retryable codes: `rate_limit_exceeded`,
    // `service_unavailable`, numeric HTTP statuses 408/429/5xx.
    if !code.is_empty() {
        if code.contains("rate_limit")
            || code.contains("server_error")
            || code.contains("overloaded")
            || code.contains("service_unavailable")
            || code.contains("timeout")
            || code.contains("transient")
            || code == "408"
            || code == "429"
            || code.starts_with("5")
        {
            return true;
        }
        if code.contains("invalid")
            || code.contains("auth")
            || code.contains("permission")
            || code.contains("context_length")
            || code.contains("not_found")
            || code.contains("insufficient_quota")
            || code.contains("content_filter")
        {
            return false;
        }
    }
    // Default: retryable. The stream-retry policy still bails after
    // the configured attempt count, so a falsely-retryable
    // classification can't loop forever. A falsely-terminal
    // classification, on the other hand, would drop turns on
    // brand-new error shapes the catalog doesn't yet know about —
    // worse user experience.
    true
}

/// Format a Chat-Completions `{ "error": { message, type, code, … } }` envelope
/// into a single human-readable string. Surfaces `type` and `code` (Anthropic's
/// `rate_limit_error`, OpenAI's `invalid_request_error` / `context_length_exceeded`,
/// OpenRouter / aggregator-specific codes) which the upstream caller needs to
/// distinguish retryable failures from auth failures from prompt-shape bugs.
/// Falls back to `default_message` only when `error` is missing or empty.
fn format_chat_error(value: &Value, default_message: &str) -> String {
    let error = value.get("error");
    let message = error
        .and_then(|err| err.get("message"))
        .and_then(Value::as_str)
        .or_else(|| error.and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or(default_message);
    let kind = error
        .and_then(|err| err.get("type"))
        .and_then(Value::as_str);
    let code = error.and_then(|err| err.get("code")).and_then(|c| {
        c.as_str()
            .map(str::to_string)
            .or_else(|| c.as_i64().map(|n| n.to_string()))
    });
    match (kind, code.as_deref()) {
        (Some(kind), Some(code)) => format!("{message} (type={kind}, code={code})"),
        (Some(kind), None) => format!("{message} (type={kind})"),
        (None, Some(code)) => format!("{message} (code={code})"),
        (None, None) => message.to_string(),
    }
}

/// H-43: split a content delta into reasoning + visible content
/// spans by scanning for inline `<think>` / `</think>` tags.
/// Tags split across chunks are stitched via `state.think_tag_buf`.
/// Returns (reasoning_text, visible_text); either may be empty.
fn split_inline_think(state: &mut StreamState, mut content: String) -> (String, String) {
    if !state.extract_inline_think {
        return (String::new(), content);
    }
    // Stitch a partial tag from the previous chunk back onto the
    // front of this chunk so the scan sees the full token.
    if !state.think_tag_buf.is_empty() {
        let mut combined = std::mem::take(&mut state.think_tag_buf);
        combined.push_str(&content);
        content = combined;
    }
    let mut reasoning = String::new();
    let mut visible = String::new();
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let remaining = &content[cursor..];
        if state.inside_think {
            // Look for the closing tag.
            if let Some(idx) = remaining.find("</think>") {
                reasoning.push_str(&remaining[..idx]);
                cursor += idx + "</think>".len();
                state.inside_think = false;
            } else if let Some(stub) = trailing_partial_tag(remaining, "</think>") {
                reasoning.push_str(&remaining[..remaining.len() - stub.len()]);
                state.think_tag_buf = stub.to_string();
                cursor = content.len();
            } else {
                reasoning.push_str(remaining);
                cursor = content.len();
            }
        } else {
            // Look for the opening tag.
            if let Some(idx) = remaining.find("<think>") {
                visible.push_str(&remaining[..idx]);
                cursor += idx + "<think>".len();
                state.inside_think = true;
            } else if let Some(stub) = trailing_partial_tag(remaining, "<think>") {
                visible.push_str(&remaining[..remaining.len() - stub.len()]);
                state.think_tag_buf = stub.to_string();
                cursor = content.len();
            } else {
                visible.push_str(remaining);
                cursor = content.len();
            }
        }
    }
    (reasoning, visible)
}

/// Returns the trailing partial-tag stub at the end of `slice` if
/// the tail is a prefix of `tag` (longer than zero, shorter than
/// the full tag). Used to buffer split-across-chunk tag tokens.
fn trailing_partial_tag<'a>(slice: &'a str, tag: &str) -> Option<&'a str> {
    let mut len = (tag.len() - 1).min(slice.len());
    while len > 0 {
        let tail = &slice[slice.len() - len..];
        if tag.starts_with(tail) {
            return Some(tail);
        }
        len -= 1;
    }
    None
}

fn parse_chat_event(data: &str, state: &mut StreamState) -> Result<Vec<LlmEvent>> {
    if data == "[DONE]" {
        let mut events = state.drain_tool_calls()?;
        if let Some(reasoning_done) = drain_reasoning(state) {
            events.push(reasoning_done);
        }
        if !state.completed_emitted {
            let stop_reason = state.finish_reason.as_deref().map(chat_stop_reason);
            events.push(LlmEvent::Completed {
                response_id: state.response_id.take(),
                cost: state.cost.clone(),
                stop_reason,
                reasoning_only_stop: state.reasoning_only_stop,
            });
            state.completed_emitted = true;
        }
        return Ok(events);
    }

    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid SSE JSON: {err}")))?;

    if value.get("error").is_some() {
        // H-27: inline mid-stream `error` JSON. Classify so the
        // outer retry policy can distinguish "retryable transport
        // hiccup" (rate-limited, 5xx upstream, network blip) from
        // "request-shape bug / auth fail" (invalid api key, model
        // gone, schema violation). Retryable shapes stay as
        // `ProviderStream` (the existing stream-retry policy is
        // already permissive about that); terminal shapes get the
        // `[non-retryable]` marker prepended so the agent loop +
        // TUI mirror the same suppression behavior they apply to
        // POST-time errors.
        let formatted = format_chat_error(&value, "chat completions stream error");
        let message = if is_inline_error_retryable(&value) {
            formatted
        } else {
            format!("{NON_RETRYABLE_MARKER}{formatted}")
        };
        return Err(SqueezyError::ProviderStream(message));
    }

    if let Some(id) = value.get("id").and_then(Value::as_str) {
        state.response_id.get_or_insert_with(|| id.to_string());
    }

    if state.server_model.is_none()
        && let Some(server_model) = value.get("model").and_then(Value::as_str)
        && !server_model.is_empty()
    {
        // Chat-completions chunks repeat the top-level `model` field on
        // every event. Stash the value the first time we see it; the
        // outer stream loop drains the slot and feeds it to
        // `ServerModelEcho`, which suppresses duplicate emissions.
        state.server_model = Some(server_model.to_string());
    }

    if let Some(usage) = value.get("usage") {
        state.cost = parse_chat_usage(usage);
    }

    let mut events = Vec::new();
    let choices = value.get("choices").and_then(Value::as_array);
    if let Some(choices) = choices {
        for choice in choices {
            // H-24: chat-completions choices carry an `index` field
            // when `n > 1`. We pin `n: 1` in `request_body` so this
            // is normally `0`, but multi-choice traffic surfaces
            // through aggregator routes that mirror the upstream
            // verbatim. Track the choice index so the tool-call
            // accumulator partitions correctly across choices.
            let choice_index = choice.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(delta) = choice.get("delta") {
                if let Some(reasoning) = collect_reasoning_delta(delta) {
                    state.reasoning_buf.push_str(&reasoning);
                    events.push(LlmEvent::ReasoningDelta {
                        text: reasoning.into_owned(),
                        kind: ReasoningKind::Summary,
                    });
                }
                if let Some(content) = collect_delta_text(delta.get("content")) {
                    // H-43: when the preset is CF Workers AI (or
                    // another upstream we know inlines reasoning
                    // via `<think>...</think>` tags on the
                    // OpenAI-compat path), split the content into
                    // reasoning + visible spans before emitting.
                    // The reasoning span lands as a
                    // `ReasoningDelta` so the TUI promotes it to
                    // the thinking pane and downstream cost /
                    // event consumers see the right kind of
                    // signal.
                    let (reasoning_span, visible_span) =
                        split_inline_think(state, content.into_owned());
                    if !reasoning_span.is_empty() {
                        state.reasoning_buf.push_str(&reasoning_span);
                        events.push(LlmEvent::ReasoningDelta {
                            text: reasoning_span,
                            kind: ReasoningKind::Summary,
                        });
                    }
                    if !visible_span.is_empty() {
                        // H-50: DeepSeek V4 and other interleaved-
                        // reasoning models stream
                        // `reasoning → content → reasoning → content`
                        // segments within a single turn. Flush the
                        // accumulated reasoning into a
                        // `ReasoningDone` when the first content
                        // delta arrives so the transcript renders
                        // thinking BEFORE its matching answer
                        // segment (not concatenated at end-of-turn
                        // and out of position).
                        if !state.reasoning_buf.trim().is_empty()
                            && let Some(reasoning_done) = drain_reasoning(state)
                        {
                            events.push(reasoning_done);
                        }
                        state.saw_visible_output = true;
                        events.push(LlmEvent::TextDelta(visible_span));
                    }
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tool_call in tool_calls {
                        // Missing `index` on a continuation delta
                        // routes to the most-recent active index on
                        // this choice (H-24). `None` is *not*
                        // collapsed to `0` here — accumulate_tool_call
                        // decides the right slot.
                        let tool_index = tool_call
                            .get("index")
                            .and_then(Value::as_u64)
                            .map(|v| v as usize);
                        state.accumulate_tool_call(choice_index, tool_index, tool_call);
                    }
                }
            }
            if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
                state.finish_reason = Some(finish_reason.to_string());
                match finish_reason {
                    "tool_calls" | "function_call" => {
                        events.extend(state.drain_tool_calls()?);
                    }
                    "stop" => {
                        events.extend(state.drain_tool_calls()?);
                        // INVARIANT (C-10): do NOT flip
                        // `state.completed_emitted` inside this arm.
                        // Groq + OpenRouter-via-Groq + native OpenAI
                        // ship a trailing usage chunk *after* the
                        // `finish_reason: stop` chunk and before the
                        // terminal `[DONE]`. The outer loop short-
                        // circuits as soon as `completed_emitted`
                        // flips, so latching it here would discard the
                        // usage chunk and report zero cost. The
                        // terminal `Completed` event is emitted from
                        // the `[DONE]` arm only — see
                        // `parse_chat_event` at the top of this
                        // function.
                        //
                        // Reasoning-mode models (Qwen3, DeepSeek-R1 via
                        // aggregator, etc.) sometimes emit only reasoning
                        // and finish cleanly with `stop` — no content, no
                        // tool calls. The agent loop would then push an
                        // empty assistant message and the user would see
                        // the spinner stop with nothing visible in the
                        // transcript. Drain the streamed reasoning so it
                        // lands in the transcript first, then inject a
                        // visible notice so the user understands the turn
                        // produced no actionable output. Skipped when the
                        // model did emit content or a tool call.
                        if !state.saw_visible_output {
                            // Latch reasoning-only-stop only when there is
                            // actually reasoning text in the buffer.
                            // Otherwise this is "model said nothing at
                            // all", which is a different (and rarer)
                            // failure mode.
                            let has_reasoning = !state.reasoning_buf.trim().is_empty();
                            if has_reasoning {
                                state.reasoning_only_stop = true;
                            }
                            if let Some(reasoning_done) = drain_reasoning(state) {
                                events.push(reasoning_done);
                            }
                            // H-31: DeepSeek `deepseek-reasoner` (and other
                            // reasoning-only modes that finish a thinking
                            // turn with `stop` and no content) ship a
                            // legitimate completion in this shape. The
                            // notice text references `reasoning_effort` (V4
                            // ignores it; see DS-4) and
                            // `tool_choice = "required"` (nonsensical on a
                            // text-only reasoning turn), so the user reads
                            // a confused apology mid-transcript instead of
                            // the model's normal end-of-thinking signal.
                            // Suppress when reasoning surfaced — the
                            // `reasoning_only_stop` flag latched above
                            // still lets the agent loop decide what to do
                            // (re-prompt for a visible response).
                            // Genuinely-empty stops (no reasoning, no
                            // content, no tool call) keep the notice so
                            // the user gets *some* breadcrumb.
                            if !has_reasoning {
                                events.push(LlmEvent::TextDelta(
                                    "\n[squeezy] model finished without emitting any content or tool call (finish_reason=stop). Reasoning-mode models can burn their output budget on thinking; try a more concrete prompt, lower reasoning_effort, or set [model].tool_choice = \"required\" to force a tool call.\n".to_string(),
                                ));
                            }
                        }
                    }
                    "length" => {
                        events.extend(state.drain_tool_calls()?);
                        if let Some(reasoning_done) = drain_reasoning(state) {
                            events.push(reasoning_done);
                        }
                        events.push(LlmEvent::TextDelta(
                            "\n[squeezy] response truncated by max_output_tokens (finish_reason=length). Raise providers.<name>.max_output_tokens or lower reasoning_effort and retry.\n".to_string(),
                        ));
                    }
                    "content_filter" => {
                        events.extend(state.drain_tool_calls()?);
                        if let Some(reasoning_done) = drain_reasoning(state) {
                            events.push(reasoning_done);
                        }
                        events.push(LlmEvent::TextDelta(
                            "\n[squeezy] response blocked by content filter (finish_reason=content_filter).\n".to_string(),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(events)
}

fn parse_chat_usage(usage: &Value) -> CostSnapshot {
    let prompt_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64);
    let completion_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64);
    let cached_input_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .or_else(|| usage.get("prompt_cache_hit_tokens"))
        .and_then(Value::as_u64);
    let reasoning_output_tokens = usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    CostSnapshot {
        input_tokens: prompt_tokens,
        output_tokens: completion_tokens,
        reasoning_output_tokens,
        cached_input_tokens,
        cache_write_input_tokens: None,
        estimated_usd_micros: None,
    }
}

#[cfg(test)]
#[path = "compatible_tests.rs"]
mod tests;
