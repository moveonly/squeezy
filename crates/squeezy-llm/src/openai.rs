use std::sync::Arc;

use async_stream::try_stream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{
    AzureOpenAiConfig, CostSnapshot, OpenAiCompatibleConfig, OpenAiCompatiblePreset, OpenAiConfig,
    ProviderTransportConfig, ResponseVerbosity, Result, SqueezyError,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmOutputSchema, LlmProvider, LlmRequest, LlmStream, LlmToolCall,
    ReasoningKind, ReasoningPayload,
    credentials::{ApiKeySource, resolve_api_key_with_inline, static_api_key_source},
    openai_prompt_cache::clamp_prompt_cache_key,
    retry::{RetryPolicy, idle_timeout, send_with_auth_retry},
    sse::SseDecoder,
    transport::shared_client,
};

#[derive(Clone)]
pub struct OpenAiProvider {
    name: &'static str,
    client: reqwest::Client,
    api_key: Arc<dyn ApiKeySource>,
    base_url: String,
    api_version: Option<String>,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("name", &self.name)
            .field("client", &self.client)
            .field("api_key", &self.api_key)
            .field("base_url", &self.base_url)
            .field("api_version", &self.api_version)
            .field("transport", &self.transport)
            .finish()
    }
}

impl OpenAiProvider {
    pub fn from_config(config: &OpenAiConfig) -> Result<Self> {
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        Ok(Self::with_api_key_source(
            "openai",
            static_api_key_source(api_key, "openai"),
            config.base_url.trim_end_matches('/').to_string(),
            None,
            config.transport,
        ))
    }

    pub fn from_azure_config(config: &AzureOpenAiConfig) -> Result<Self> {
        if config.base_url.trim().is_empty() {
            return Err(SqueezyError::ProviderNotConfigured(
                "missing AZURE_OPENAI_BASE_URL or providers.azure_openai.base_url".to_string(),
            ));
        }
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        Ok(Self::with_api_key_source(
            "azure_openai",
            static_api_key_source(api_key, "azure_openai"),
            config.base_url.trim_end_matches('/').to_string(),
            Some(config.api_version.clone()),
            config.transport,
        ))
    }

    /// Build an OpenAI Responses-API client targeting xAI's `/responses`
    /// endpoint. Reuses the OpenAI request body and SSE parser because xAI
    /// implements the Responses wire as a near-drop-in for Grok 3 and Grok 4
    /// (see `https://docs.x.ai/docs/api-reference/responses`). The
    /// `OpenAiCompatibleConfig::extra_headers` map is intentionally ignored
    /// here — those headers (HTTP-Referer, X-Title, x-portkey-*) are
    /// chat-completions aggregator concerns and have no analogue on a
    /// dedicated vendor Responses endpoint.
    pub fn from_xai_config(config: &OpenAiCompatibleConfig) -> Result<Self> {
        debug_assert_eq!(config.preset, OpenAiCompatiblePreset::XAi);
        if config.base_url.trim().is_empty() {
            return Err(SqueezyError::ProviderNotConfigured(
                "providers.xai.base_url is required for the xAI Responses route".to_string(),
            ));
        }
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        Ok(Self::with_api_key_source(
            "xai",
            static_api_key_source(api_key, "xai"),
            config.base_url.trim_end_matches('/').to_string(),
            None,
            config.transport,
        ))
    }

    /// Construct the provider against an already-built credential
    /// source. Used by the OpenAI Codex (ChatGPT Plus/Pro) OAuth
    /// provider so a rotating access token can flow through the same
    /// `/responses` HTTP path without rebuilding the client.
    pub fn with_api_key_source(
        name: &'static str,
        api_key: Arc<dyn ApiKeySource>,
        base_url: String,
        api_version: Option<String>,
        transport: ProviderTransportConfig,
    ) -> Self {
        Self {
            name,
            client: shared_client(&transport),
            api_key,
            base_url,
            api_version,
            transport,
        }
    }

    pub(crate) fn request_body(request: &LlmRequest, provider_name: &str) -> Value {
        // Canonicalize cross-provider tool-call ids and synthesize
        // placeholders for orphan tool results BEFORE projecting to
        // the Responses-API `input` array. The Responses backend
        // matches `function_call_output.call_id` against the prior
        // `function_call.call_id` in the input slice; if the user
        // switched from Anthropic/Google/Bedrock mid-session those
        // ids carry the upstream's shape (`toolu_…`,
        // `google_call_…`, `tooluse_…`) and the pairing breaks even
        // though OpenAI itself accepts the id shape.
        let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
        let mut body = json!({
            "model": request.model,
            "instructions": request.instructions,
            "input": openai_input(&normalized_input),
            "stream": true,
            "store": request.store,
        });
        if let Some(previous_response_id) = &request.previous_response_id {
            body["previous_response_id"] = json!(previous_response_id);
        }
        let cache_spec = request.effective_cache_spec();
        if let Some(key) = cache_spec.key.as_deref() {
            // OpenAI's Responses API silently drops `prompt_cache_key`
            // values longer than 64 codepoints — the request succeeds
            // but the field is ignored server-side, so every turn pays
            // full uncached input cost while telemetry shows zero
            // cache hits. Clamp client-side. See [`clamp_prompt_cache_key`].
            body["prompt_cache_key"] = json!(clamp_prompt_cache_key(key));
        }
        if cache_spec.retention == crate::CacheRetention::Long {
            body["prompt_cache_retention"] = json!("24h");
        }
        if let Some(max_output_tokens) = request.max_output_tokens {
            body["max_output_tokens"] = json!(max_output_tokens);
        }
        let mut text = serde_json::Map::new();
        if let Some(response_verbosity) = request.response_verbosity {
            text.insert(
                "verbosity".to_string(),
                json!(openai_text_verbosity(response_verbosity)),
            );
        }
        if let Some(schema) = request.output_schema.as_ref() {
            text.insert("format".to_string(), openai_text_format(schema));
        }
        if !text.is_empty() {
            body["text"] = Value::Object(text);
        }
        // The o-series and gpt-5.x models reason internally regardless of
        // whether the caller picks an effort level; the API just won't stream
        // a summary or expose `encrypted_content` for replay unless we ask
        // for it. Request both whenever the model registry says this model
        // supports reasoning, and only forward `effort` when the caller
        // explicitly set one (else the provider uses its own per-model
        // default).
        let reasoning_capable = crate::capabilities_for(provider_name, &request.model)
            .is_some_and(|caps| caps.reasoning_effort);
        if reasoning_capable || request.reasoning_effort.is_some() {
            let mut reasoning = serde_json::Map::new();
            reasoning.insert("summary".to_string(), json!("auto"));
            if let Some(effort) = request.reasoning_effort {
                reasoning.insert("effort".to_string(), json!(effort.as_str()));
            }
            body["reasoning"] = Value::Object(reasoning);
            if !request.store {
                body["include"] = json!(["reasoning.encrypted_content"]);
            }
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                            "strict": tool.strict,
                        })
                    })
                    .collect::<Vec<_>>()
            );
            // Forward `tool_choice` when the caller set one. See LlmRequest
            // docs — `None` omits the field and falls back to the
            // provider's `auto` default.
            if let Some(choice) = request.tool_choice.as_deref() {
                body["tool_choice"] = json!(choice);
            }
        }
        if let Some(parallel) = request.parallel_tool_calls {
            // OpenAI's Responses API defaults to parallel tool calls; only
            // forward an explicit override when the caller opts out
            // (`Some(false)`) so the request body stays compact for the
            // common case.
            if !parallel {
                body["parallel_tool_calls"] = json!(false);
            }
        }
        body
    }

    /// Prompt-cache affinity headers attached to every Responses request
    /// that carries a cache key. OpenAI's load balancer uses these to
    /// route the session to the same backend that warmed the cached
    /// prefix; without them, repeat turns can land on a cold node and
    /// silently miss cache even when `prompt_cache_key` matches. Mirrors
    /// the official Codex CLI's behavior and pi's
    /// `openai-responses.ts` (`session_id` + `x-client-request-id`).
    ///
    /// The header values carry the full (unclamped) cache key — the
    /// 64-codepoint limit is specific to the body field; routing headers
    /// have OpenAI's general (much larger) header length cap.
    pub(crate) fn affinity_headers(request: &LlmRequest) -> Vec<(&'static str, String)> {
        let Some(key) = request.effective_cache_spec().key else {
            return Vec::new();
        };
        vec![("session_id", key.clone()), ("x-client-request-id", key)]
    }
}

fn openai_text_verbosity(verbosity: ResponseVerbosity) -> &'static str {
    match verbosity {
        ResponseVerbosity::Concise => "low",
        ResponseVerbosity::Normal => "medium",
        ResponseVerbosity::Verbose => "high",
    }
}

fn openai_text_format(schema: &LlmOutputSchema) -> Value {
    json!({
        "type": "json_schema",
        "name": schema.name,
        "strict": schema.strict,
        "schema": schema.schema,
    })
}

impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        if let Err(err) = request.ensure_vision_support(self.name) {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let provider_name = self.name;
        let transport = self.transport;
        let mut url = format!("{}/responses", self.base_url);
        if let Some(api_version) = &self.api_version {
            url.push_str("?api-version=");
            url.push_str(api_version);
        }
        let body = Self::request_body(&request, provider_name);
        let affinity_headers = Self::affinity_headers(&request);

        Box::pin(try_stream! {
            let response = send_with_auth_retry(
                &api_key,
                RetryPolicy::provider_requests(transport),
                &cancel,
                |key| {
                    let builder = client.post(&url);
                    let builder = if provider_name == "azure_openai" {
                        builder.header("api-key", key)
                    } else {
                        builder.bearer_auth(key)
                    };
                    // Cache-affinity headers (only emitted when the
                    // request carries a cache key) keep multi-turn
                    // sessions pinned to the backend that warmed the
                    // cached prefix.
                    let builder = affinity_headers
                        .iter()
                        .fold(builder, |b, (name, value)| b.header(*name, value.as_str()));
                    builder.json(&body)
                },
            ).await?;

            let status = response.status();
            let response = if status == StatusCode::OK {
                response
            } else {
                let message = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "failed to read error response".to_string());
                Err(SqueezyError::ProviderRequest(format!("{status}: {message}")))?;
                unreachable!("provider error returned above");
            };

            yield LlmEvent::Started;

            let mut decoder = SseDecoder::default();
            let mut saw_completed = false;
            let mut reasoning_acc = ReasoningAccumulator::default();
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
                    SqueezyError::ProviderStream("OpenAI stream idle timeout".to_string())
                })?;
                let Some(chunk) = next else { break; };
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for event in decoder.push(&chunk) {
                    let parsed = parse_openai_event(&event, &mut reasoning_acc)?;
                    if let Some(server) = reasoning_acc.take_server_model()
                        && let Some(echo) = server_model_echo.observe(&request.model, &server)
                    {
                        yield echo;
                    }
                    if let Some(llm_event) = parsed {
                        if matches!(llm_event, LlmEvent::Completed { .. }) {
                            saw_completed = true;
                        }
                        yield llm_event;
                    }
                }
            }

            for event in decoder.finish() {
                let parsed = parse_openai_event(&event, &mut reasoning_acc)?;
                if let Some(server) = reasoning_acc.take_server_model()
                    && let Some(echo) = server_model_echo.observe(&request.model, &server)
                {
                    yield echo;
                }
                if let Some(llm_event) = parsed {
                    if matches!(llm_event, LlmEvent::Completed { .. }) {
                        saw_completed = true;
                    }
                    yield llm_event;
                }
            }

            if !saw_completed {
                Err(SqueezyError::ProviderStream(
                    "OpenAI stream ended without response.completed".to_string(),
                ))?;
            }
        })
    }
}

/// Buffers reasoning deltas across an OpenAI Responses stream so
/// `response.output_item.done` can backfill an empty `summary` from
/// the streamed text. The Responses API sometimes ships the item-close
/// event with `summary: []` even though `response.reasoning_summary_text.delta`
/// (or `response.reasoning_text.delta`) already streamed the real text;
/// without the buffer the persisted `ReasoningPayload::OpenAi.summary`
/// would be empty and the next turn's replay would drop the segment.
#[derive(Debug, Default)]
pub(crate) struct ReasoningAccumulator {
    summary: String,
    text: String,
    /// Stashes the server-echoed `response.model` field the first time
    /// any event carries it (typically `response.created`). The outer
    /// stream loop drains the value via [`Self::take_server_model`] and
    /// feeds it to [`crate::ServerModelEcho`] so [`LlmEvent::ServerModel`]
    /// lands additively right after [`LlmEvent::Started`]. Stays `None`
    /// for every event that doesn't include a `response.model` field —
    /// the loop only acts on the first observation per stream.
    server_model: Option<String>,
}

impl ReasoningAccumulator {
    fn take(&mut self) -> Vec<String> {
        let mut out = Vec::with_capacity(2);
        let summary = std::mem::take(&mut self.summary);
        if !summary.is_empty() {
            out.push(summary);
        }
        let text = std::mem::take(&mut self.text);
        if !text.is_empty() {
            out.push(text);
        }
        out
    }

    pub(crate) fn take_server_model(&mut self) -> Option<String> {
        self.server_model.take()
    }
}

pub(crate) fn parse_openai_event(
    data: &str,
    reasoning_acc: &mut ReasoningAccumulator,
) -> Result<Option<LlmEvent>> {
    if data == "[DONE]" {
        return Ok(None);
    }

    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid SSE JSON: {err}")))?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    tracing::trace!(target: "squeezy_llm::openai", event_type, "sse event");

    // OpenAI Responses events embed the server-chosen model on the
    // `response` object that ships with `response.created`,
    // `response.in_progress`, `response.completed`, etc. Pluck it the
    // first time we see it so the outer stream loop can compare
    // against `request.model` and emit an additive `ServerModel`
    // event. Repeated observations on the same stream are coalesced
    // into a single drain in the caller via
    // [`crate::ServerModelEcho`].
    if reasoning_acc.server_model.is_none()
        && let Some(server_model) = value
            .get("response")
            .and_then(|response| response.get("model"))
            .and_then(Value::as_str)
        && !server_model.is_empty()
    {
        reasoning_acc.server_model = Some(server_model.to_string());
    }

    match event_type {
        "response.output_text.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Some(LlmEvent::TextDelta(delta)))
        }
        "response.reasoning_summary_text.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            reasoning_acc.summary.push_str(&delta);
            Ok(Some(LlmEvent::ReasoningDelta {
                text: delta,
                kind: ReasoningKind::Summary,
            }))
        }
        "response.reasoning_text.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            reasoning_acc.text.push_str(&delta);
            Ok(Some(LlmEvent::ReasoningDelta {
                text: delta,
                kind: ReasoningKind::Text,
            }))
        }
        "response.output_item.done" => {
            let item = value.get("item");
            if let Some(payload) = parse_reasoning_item(item, reasoning_acc) {
                Ok(Some(LlmEvent::ReasoningDone(payload)))
            } else if let Some(tool_call) = parse_tool_call(item)? {
                Ok(Some(LlmEvent::ToolCall(tool_call)))
            } else {
                Ok(None)
            }
        }
        "response.completed" => {
            let response = value.get("response");
            let response_id = response
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // Successful completions don't carry `incomplete_details`;
            // treat their absence as `EndTurn` so the agent's turn loop
            // sees a normalized stop reason on every provider event.
            let stop_reason = response
                .and_then(|response| response.get("incomplete_details"))
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .map(crate::StopReason::from_openai_incomplete)
                .or(Some(crate::StopReason::EndTurn));
            Ok(Some(LlmEvent::Completed {
                response_id,
                cost: parse_cost(response),
                stop_reason,
                reasoning_only_stop: false,
            }))
        }
        "response.incomplete" => {
            // Surface incompletion to the agent instead of erroring here
            // so max-output-tokens / content-filter cases reach the same
            // recovery path as the other providers.
            let response = value.get("response");
            let response_id = response
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let stop_reason = response
                .and_then(|response| response.get("incomplete_details"))
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .map(crate::StopReason::from_openai_incomplete)
                .or(Some(crate::StopReason::Other("incomplete".to_string())));
            Ok(Some(LlmEvent::Completed {
                response_id,
                cost: parse_cost(response),
                stop_reason,
                reasoning_only_stop: false,
            }))
        }
        "error" | "response.failed" => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .unwrap_or("OpenAI stream error");
            Err(SqueezyError::ProviderStream(message.to_string()))
        }
        _ => {
            tracing::debug!(
                target: "squeezy_llm::openai",
                event_type,
                "unhandled OpenAI SSE event"
            );
            Ok(None)
        }
    }
}

fn openai_input(input: &[LlmInputItem]) -> Value {
    if let [LlmInputItem::UserText(text)] = input {
        return json!(text);
    }

    Value::Array(input.iter().filter_map(openai_input_item).collect())
}

fn openai_input_item(item: &LlmInputItem) -> Option<Value> {
    Some(match item {
        LlmInputItem::UserText(text) => json!({
            "role": "user",
            "content": text,
        }),
        LlmInputItem::AssistantText(text) => json!({
            "role": "assistant",
            "content": text,
        }),
        LlmInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string()),
        }),
        LlmInputItem::FunctionCallOutput { call_id, output } => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }),
        LlmInputItem::Image { media_type, bytes } => json!({
            "role": "user",
            "content": [{
                "type": "input_image",
                "detail": "auto",
                "image_url": format!(
                    "data:{media_type};base64,{}",
                    BASE64_STANDARD.encode(bytes.as_ref())
                ),
            }],
        }),
        LlmInputItem::Reasoning(ReasoningPayload::OpenAi {
            item_id,
            summary,
            encrypted_content,
        }) => {
            let summary_value = Value::Array(
                summary
                    .iter()
                    .map(|text| json!({ "type": "summary_text", "text": text }))
                    .collect(),
            );
            let mut obj = json!({
                "type": "reasoning",
                "id": item_id,
                "summary": summary_value,
            });
            if let Some(encrypted) = encrypted_content {
                obj["encrypted_content"] = json!(encrypted);
            }
            obj
        }
        // Reasoning items from other providers are dropped when replaying to OpenAI.
        LlmInputItem::Reasoning(_) => return None,
    })
}

fn parse_reasoning_item(
    item: Option<&Value>,
    reasoning_acc: &mut ReasoningAccumulator,
) -> Option<ReasoningPayload> {
    let item = item?;
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut summary: Vec<String> = item
        .get("summary")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_string))
                .filter(|text| !text.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // OpenAI Responses sometimes closes the reasoning item with `summary: []`
    // even when `response.reasoning_summary_text.delta` already streamed the
    // real text; without backfilling from the streamed deltas the persisted
    // payload would be empty and the next turn's replay would lose the
    // segment. Drain the accumulator on every `output_item.done` so a
    // subsequent reasoning item starts clean.
    let buffered = reasoning_acc.take();
    if summary.is_empty() {
        summary = buffered;
    }
    let encrypted_content = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(ReasoningPayload::OpenAi {
        item_id,
        summary,
        encrypted_content,
    })
}

fn parse_tool_call(item: Option<&Value>) -> Result<Option<LlmToolCall>> {
    let Some(item) = item else {
        return Ok(None);
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return Ok(None);
    }

    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .ok_or_else(|| SqueezyError::ProviderStream("function call missing call_id".to_string()))?
        .to_string();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| SqueezyError::ProviderStream("function call missing name".to_string()))?
        .to_string();
    let arguments = match item.get("arguments") {
        Some(Value::String(arguments)) => serde_json::from_str(arguments).unwrap_or_else(|err| {
            json!({
                INVALID_TOOL_ARGUMENTS_KEY: true,
                INVALID_TOOL_ARGUMENTS_ERROR_KEY: err.to_string(),
                INVALID_TOOL_ARGUMENTS_RAW_KEY: arguments,
            })
        }),
        Some(arguments @ Value::Object(_)) => arguments.clone(),
        None => Value::Object(Default::default()),
        Some(_) => {
            return Err(SqueezyError::ProviderStream(
                "function call arguments must be a JSON object or encoded JSON string".to_string(),
            ));
        }
    };

    Ok(Some(LlmToolCall {
        call_id,
        name,
        arguments,
    }))
}

fn parse_cost(response: Option<&Value>) -> CostSnapshot {
    let Some(usage) = response.and_then(|response| response.get("usage")) else {
        return CostSnapshot::default();
    };

    CostSnapshot {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        reasoning_output_tokens: usage
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        cached_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write_input_tokens: None,
        estimated_usd_micros: None,
    }
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
