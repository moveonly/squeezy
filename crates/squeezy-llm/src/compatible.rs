//! OpenAI-compatible (Chat Completions) provider client.
//!
//! Covers every endpoint that speaks `POST /chat/completions` with a Bearer
//! token: OpenRouter, Vercel AI Gateway, PortKey, Groq, xAI, DeepSeek,
//! Mistral, Together AI, Fireworks AI, Cerebras, plus any custom OpenAI-
//! compatible host (self-hosted LiteLLM, Cloudflare Workers AI, etc.). The
//! native OpenAI provider stays on the `/responses` endpoint and is not
//! routed through here.

use std::collections::BTreeMap;
use std::sync::Arc;

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

use crate::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, ReasoningKind,
    ReasoningPayload,
    cache_policy::{CacheRetention, ephemeral_marker, json_markers, last_stable_tool_index},
    credentials::{ApiKeySource, resolve_api_key_with_inline, static_api_key_source},
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
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        let mut headers = preset_default_headers(config.preset);
        // User-supplied headers override preset defaults so deployments can
        // attach their own HTTP-Referer / X-Title / x-portkey-* values.
        for (key, value) in &config.extra_headers {
            headers.insert(key.clone(), value.clone());
        }
        Ok(Self::with_api_key_source(
            config.preset,
            static_api_key_source(api_key, config.preset.as_str()),
            config.base_url.trim_end_matches('/').to_string(),
            headers,
            config.transport,
        ))
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

    pub(crate) fn request_body(request: &LlmRequest) -> Value {
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
        });
        if let Some(max_tokens) = request.max_output_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(effort) = request.reasoning_effort {
            // OpenRouter, xAI, and most OpenAI-compatible endpoints accept the
            // top-level legacy form. OpenRouter's docs now recommend the
            // nested `reasoning: { effort: ... }` form; send both so we
            // cover both shapes without per-preset branching. Aggregators
            // ignore unknown fields; non-reasoning models ignore the hint.
            let effort_str = effort.as_str();
            body["reasoning_effort"] = json!(effort_str);
            body["reasoning"] = json!({ "effort": effort_str });
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
            body["prompt_cache_key"] = json!(key);
        }
        if cache_retention == CacheRetention::Long {
            // Mirror the OpenAI native provider's extended-retention opt-in
            // so OpenAI-hosted models proxied via an aggregator (OpenRouter
            // `openai/*`, Vercel AI Gateway, etc.) still get the 24h
            // window. Anthropic-hosted aggregator routes have already
            // emitted the `ttl: "1h"` marker above; OpenAI ignores
            // `prompt_cache_retention` from non-OpenAI flavors.
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
            // calling at least one tool per turn — see opencode's
            // pass-through pattern in `openai-chat.ts:267`.
            if let Some(choice) = request.tool_choice.as_deref() {
                body["tool_choice"] = json!(choice);
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

/// Per-namespace compatibility row. The struct mirrors pi's
/// `OpenAICompletionsCompat` interface (`others/pi/packages/ai/src/types.ts`)
/// in spirit: a typed set of capability flags that drive wire-shape choices
/// without scattering substring tests across the request builder.
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
        let url = format!("{}/chat/completions", self.base_url);
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
        let body = Self::request_body(&request);
        let provider_label = self.preset.display_name();

        Box::pin(try_stream! {
            let response = send_with_auth_retry(
                &api_key,
                RetryPolicy::provider_requests(transport),
                &cancel,
                |key| {
                    let mut builder = client.post(&url).bearer_auth(key);
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
                } else {
                    ""
                };
                Err(SqueezyError::ProviderRequest(format!(
                    "{provider_label} {status}: {message}{hint}"
                )))?;
                unreachable!("provider error returned above");
            };

            yield LlmEvent::Started;

            let mut decoder = SseDecoder::default();
            let mut state = StreamState::default();
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
                    for emitted in parse_chat_event(&event, &mut state)? {
                        yield emitted;
                    }
                    if state.completed_emitted {
                        return;
                    }
                }
            }

            for event in decoder.finish() {
                for emitted in parse_chat_event(&event, &mut state)? {
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
        LlmInputItem::FunctionCallOutput { call_id, output } => json!({
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
    })
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
    tool_calls: BTreeMap<usize, PartialToolCall>,
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
}

#[derive(Debug, Default)]
struct PartialToolCall {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl StreamState {
    fn accumulate_tool_call(&mut self, index: usize, delta: &Value) {
        let entry = self.tool_calls.entry(index).or_default();
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
                entry.arguments.push_str(arguments);
            }
        }
    }

    fn drain_tool_calls(&mut self) -> Result<Vec<LlmEvent>> {
        let mut events = Vec::new();
        let drained = std::mem::take(&mut self.tool_calls);
        for (index, partial) in drained {
            let call_id = partial.call_id.unwrap_or_else(|| format!("call_{index}"));
            // Skip incomplete tool calls (no function.name accumulated)
            // instead of erroring the whole stream. PortKey / OpenRouter /
            // Qwen sometimes ship a tool-call delta whose name chunk goes
            // missing or whose stream cuts mid-call. Erroring here would
            // discard any assistant text the model already produced and
            // halt the turn. Match opencode's `finishAll`
            // (utils/tool-stream.ts:200): drop the partial entry, complete
            // the turn with whatever did surface, let the model retry next
            // turn. A short stderr warning makes the drop traceable.
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
            let arguments_text = if partial.arguments.is_empty() {
                "{}".to_string()
            } else {
                partial.arguments
            };
            let arguments = serde_json::from_str::<Value>(&arguments_text).unwrap_or_else(|err| {
                json!({
                    INVALID_TOOL_ARGUMENTS_KEY: true,
                    INVALID_TOOL_ARGUMENTS_ERROR_KEY: err.to_string(),
                    INVALID_TOOL_ARGUMENTS_RAW_KEY: arguments_text,
                })
            });
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
fn collect_delta_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                } else if let Some(delta) = part.get("delta").and_then(Value::as_str) {
                    out.push_str(delta);
                }
            }
            out
        }
        _ => String::new(),
    }
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
        return Err(SqueezyError::ProviderStream(format_chat_error(
            &value,
            "chat completions stream error",
        )));
    }

    if let Some(id) = value.get("id").and_then(Value::as_str) {
        state.response_id.get_or_insert_with(|| id.to_string());
    }

    if let Some(usage) = value.get("usage") {
        state.cost = parse_chat_usage(usage);
    }

    let mut events = Vec::new();
    let choices = value.get("choices").and_then(Value::as_array);
    if let Some(choices) = choices {
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                let reasoning = collect_delta_text(delta.get("reasoning_content"))
                    + &collect_delta_text(delta.get("reasoning"));
                if !reasoning.is_empty() {
                    state.reasoning_buf.push_str(&reasoning);
                    events.push(LlmEvent::ReasoningDelta {
                        text: reasoning,
                        kind: ReasoningKind::Summary,
                    });
                }
                let content = collect_delta_text(delta.get("content"));
                if !content.is_empty() {
                    state.saw_visible_output = true;
                    events.push(LlmEvent::TextDelta(content));
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tool_call in tool_calls {
                        let index =
                            tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        state.accumulate_tool_call(index, tool_call);
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
                            if !state.reasoning_buf.trim().is_empty() {
                                state.reasoning_only_stop = true;
                            }
                            if let Some(reasoning_done) = drain_reasoning(state) {
                                events.push(reasoning_done);
                            }
                            events.push(LlmEvent::TextDelta(
                                "\n[squeezy] model finished without emitting any content or tool call (finish_reason=stop). Reasoning-mode models can burn their output budget on thinking; try a more concrete prompt, lower reasoning_effort, or set [model].tool_choice = \"required\" to force a tool call.\n".to_string(),
                            ));
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
