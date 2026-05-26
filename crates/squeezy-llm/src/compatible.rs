//! OpenAI-compatible (Chat Completions) provider client.
//!
//! Covers every endpoint that speaks `POST /chat/completions` with a Bearer
//! token: OpenRouter, Vercel AI Gateway, PortKey, Groq, xAI, DeepSeek,
//! Mistral, Together AI, Fireworks AI, Cerebras, plus any custom OpenAI-
//! compatible host (self-hosted LiteLLM, Cloudflare Workers AI, etc.). The
//! native OpenAI provider stays on the `/responses` endpoint and is not
//! routed through here.

use std::collections::BTreeMap;

use async_stream::try_stream;
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
    credentials::resolve_api_key_with_inline,
    retry::{RetryPolicy, idle_timeout, send_with_retry},
    sse::SseDecoder,
};

#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    preset: OpenAiCompatiblePreset,
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    extra_headers: BTreeMap<String, String>,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for OpenAiCompatibleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleProvider")
            .field("preset", &self.preset)
            .field("client", &self.client)
            .field("api_key", &"<redacted>")
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
        Ok(Self {
            preset: config.preset,
            client: reqwest::Client::new(),
            api_key,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            extra_headers: headers,
            transport: config.transport,
        })
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
        // is namespaced under `anthropic/` (covers OpenRouter, Vercel AI
        // Gateway, and any other aggregator that uses that namespace
        // convention). Without this the aggregator route reports zero cached
        // tokens, which silently inflates cost vs. a direct vendor call.
        let cache_control =
            if request.cache_key.is_some() && supports_anthropic_caching(&request.model) {
                Some(json!({ "type": "ephemeral" }))
            } else {
                None
            };
        // Find the last user-text turn so we can mark it as the cache
        // breakpoint. Anthropic caches everything *before* a marker, so the
        // last user message is the natural place.
        let last_user_text_index = cache_control.as_ref().and_then(|_| {
            request
                .input
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, item)| {
                    matches!(item, LlmInputItem::UserText(_)).then_some(index)
                })
        });

        let mut messages = Vec::with_capacity(request.input.len() + 1);
        let trimmed_instructions = request.instructions.trim();
        if !trimmed_instructions.is_empty() {
            if let Some(cc) = &cache_control {
                messages.push(json!({
                    "role": "system",
                    "content": [
                        {
                            "type": "text",
                            "text": &*request.instructions,
                            "cache_control": cc,
                        }
                    ],
                }));
            } else {
                messages.push(json!({
                    "role": "system",
                    "content": &*request.instructions,
                }));
            }
        }
        for (index, item) in request.input.iter().enumerate() {
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
        if !request.tools.is_empty() {
            body["tools"] = json!(
                request
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
                    .collect::<Vec<_>>()
            );
        }
        body
    }
}

fn supports_anthropic_caching(model: &str) -> bool {
    // Aggregator routes that proxy Anthropic models use a `vendor/model`
    // namespace; OpenRouter's docs treat the `anthropic/` prefix as the
    // signal to enable cache_control. We mirror that. Direct Anthropic calls
    // do not go through this client (the native Anthropic provider handles
    // them with its own cache markers).
    model.to_ascii_lowercase().starts_with("anthropic/")
}

impl LlmProvider for OpenAiCompatibleProvider {
    fn name(&self) -> &'static str {
        self.preset.as_str()
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
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
            let response = send_with_retry(
                RetryPolicy::provider_requests(transport),
                &cancel,
                || {
                    let mut builder = client.post(&url).bearer_auth(api_key.clone());
                    for (key, value) in &extra_headers {
                        builder = builder.header(key.as_str(), value.as_str());
                    }
                    builder.json(&body)
                },
            )
            .await?;

            let status = response.status();
            let response = if status == StatusCode::OK {
                response
            } else {
                let message = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "failed to read error response".to_string());
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
                    "_[squeezy] stream ended without producing any content or tool call. The provider may have cut the connection mid-response; retry the turn._".to_string(),
                );
            }
            if !state.completed_emitted {
                yield LlmEvent::Completed {
                    response_id: state.response_id.take(),
                    cost: state.cost.clone(),
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
                eprintln!(
                    "squeezy: skipping incomplete chat-completions tool call at index {index} (call_id={call_id}, no function name in stream)"
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

fn parse_chat_event(data: &str, state: &mut StreamState) -> Result<Vec<LlmEvent>> {
    if data == "[DONE]" {
        let mut events = state.drain_tool_calls()?;
        if let Some(reasoning_done) = drain_reasoning(state) {
            events.push(reasoning_done);
        }
        if !state.completed_emitted {
            events.push(LlmEvent::Completed {
                response_id: state.response_id.take(),
                cost: state.cost.clone(),
            });
            state.completed_emitted = true;
        }
        return Ok(events);
    }

    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid SSE JSON: {err}")))?;

    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .unwrap_or("chat completions stream error")
            .to_string();
        return Err(SqueezyError::ProviderStream(message));
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
                            if let Some(reasoning_done) = drain_reasoning(state) {
                                events.push(reasoning_done);
                            }
                            events.push(LlmEvent::TextDelta(
                                "_[squeezy] model finished without emitting any content or tool call (finish_reason=stop). Reasoning-mode models can burn their output budget on thinking; try a more concrete prompt or lower `reasoning_effort`._".to_string(),
                            ));
                        }
                    }
                    "length" => {
                        events.extend(state.drain_tool_calls()?);
                        if let Some(reasoning_done) = drain_reasoning(state) {
                            events.push(reasoning_done);
                        }
                        events.push(LlmEvent::TextDelta(
                            "_[squeezy] response truncated by max_output_tokens (finish_reason=length). Raise `providers.<name>.max_output_tokens` or lower `reasoning_effort` and retry._".to_string(),
                        ));
                    }
                    "content_filter" => {
                        events.extend(state.drain_tool_calls()?);
                        if let Some(reasoning_done) = drain_reasoning(state) {
                            events.push(reasoning_done);
                        }
                        events.push(LlmEvent::TextDelta(
                            "_[squeezy] response blocked by content filter (finish_reason=content_filter)._".to_string(),
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
