use std::collections::BTreeMap;
use std::sync::Arc;

use async_stream::try_stream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{CostSnapshot, GoogleConfig, ProviderTransportConfig, Result, SqueezyError};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, ReasoningKind,
    ReasoningPayload,
    credentials::{ApiKeySource, resolve_api_key_with_inline, static_api_key_source},
    retry::{RetryPolicy, idle_timeout, send_with_auth_retry},
    sse::SseDecoder,
    transport::shared_client,
};

#[derive(Clone)]
pub struct GoogleProvider {
    client: reqwest::Client,
    api_key: Arc<dyn ApiKeySource>,
    base_url: String,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleProvider")
            .field("client", &self.client)
            .field("api_key", &self.api_key)
            .field("base_url", &self.base_url)
            .field("transport", &self.transport)
            .finish()
    }
}

impl GoogleProvider {
    pub fn from_config(config: &GoogleConfig) -> Result<Self> {
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        Ok(Self {
            client: shared_client(&config.transport),
            api_key: static_api_key_source(api_key, "google"),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            transport: config.transport,
        })
    }

    pub(crate) fn request_body(request: &LlmRequest) -> Value {
        // Canonicalize tool-call ids and synthesize placeholders for
        // orphan tool results BEFORE projecting to Google's
        // `contents` array. Google identifies tool calls by `name`
        // (no explicit id) and pairs `functionResponse` to the
        // preceding `functionCall` by name; cross-provider replay
        // can leave `FunctionCallOutput` items whose `call_id`
        // doesn't appear in any prior `FunctionCall`, in which case
        // the response gets dropped to a generic `"tool"` name and
        // the model can't follow the conversation. Synthesizing a
        // placeholder call keeps the pairing intact.
        let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
        let mut body = json!({
            "systemInstruction": {
                "parts": [{"text": request.instructions}]
            },
            "contents": google_contents(&normalized_input),
            "generationConfig": {},
        });
        if let Some(max_output_tokens) = request.max_output_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(max_output_tokens);
        }
        // Gemini 2.5 thinks by default; the API just won't return thought
        // summaries unless `includeThoughts` is on. Mirror OpenAI: request
        // summaries whenever the model is reasoning-capable, and only set
        // an explicit `thinkingBudget` when the caller picked an effort.
        let reasoning_capable = crate::capabilities_for("google", &request.model)
            .is_some_and(|caps| caps.reasoning_effort);
        if reasoning_capable || request.reasoning_effort.is_some() {
            let mut thinking = json!({ "includeThoughts": true });
            if let Some(effort) = request.reasoning_effort {
                thinking["thinkingBudget"] = json!(effort.thinking_budget_tokens());
            }
            body["generationConfig"]["thinkingConfig"] = thinking;
        }
        if !request.tools.is_empty() {
            body["tools"] = json!([{
                "functionDeclarations": request
                    .tools
                    .iter()
                    .map(|tool| json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }))
                    .collect::<Vec<_>>()
            }]);
        }
        body
    }
}

impl LlmProvider for GoogleProvider {
    fn name(&self) -> &'static str {
        "google"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        if let Err(err) = request.ensure_vision_support("google") {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        let client = self.client.clone();
        // Keep the API key off the URL: `reqwest::Error::Display` appends
        // `" for url ({url})"` to every transport/stream error message, so a
        // key-in-query URL would leak the key into `SqueezyError::ProviderRequest`
        // / `ProviderStream`, the CLI/TUI status line, logs, tracing, and bug
        // reports on any DNS/TLS/timeout/connection or chunk error. Send it via
        // Google's documented `x-goog-api-key` header instead.
        let url = google_stream_url(&self.base_url, &request.model);
        let api_key = self.api_key.clone();
        let body = Self::request_body(&request);
        let transport = self.transport;

        Box::pin(try_stream! {
            let response = send_with_auth_retry(
                &api_key,
                RetryPolicy::provider_requests(transport),
                &cancel,
                |key| {
                    client
                        .post(&url)
                        .header("x-goog-api-key", key)
                        .json(&body)
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
            let mut last_cost = CostSnapshot::default();
            let mut last_finish_reason: Option<String> = None;
            let mut server_model_slot: Option<String> = None;
            let mut server_model_echo = crate::ServerModelEcho::default();
            let mut saw_any = false;
            let mut reasoning_buf = GoogleReasoningBuffer::default();
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
                    SqueezyError::ProviderStream("Google stream idle timeout".to_string())
                })?;
                let Some(chunk) = next else { break; };
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for event in decoder.push(&chunk) {
                    saw_any = true;
                    let parsed = parse_google_event(
                        &event,
                        &mut last_cost,
                        &mut last_finish_reason,
                        &mut reasoning_buf,
                        &mut server_model_slot,
                    )?;
                    if let Some(server) = server_model_slot.take()
                        && let Some(echo) = server_model_echo.observe(&request.model, &server)
                    {
                        yield echo;
                    }
                    for llm_event in parsed {
                        yield llm_event;
                    }
                }
            }
            for event in decoder.finish() {
                saw_any = true;
                let parsed = parse_google_event(
                    &event,
                    &mut last_cost,
                    &mut last_finish_reason,
                    &mut reasoning_buf,
                    &mut server_model_slot,
                )?;
                if let Some(server) = server_model_slot.take()
                    && let Some(echo) = server_model_echo.observe(&request.model, &server)
                {
                    yield echo;
                }
                for llm_event in parsed {
                    yield llm_event;
                }
            }
            if !saw_any {
                Err(SqueezyError::ProviderStream("Google stream ended without events".to_string()))?;
            }
            if let Some(payload) = reasoning_buf.flush() {
                yield LlmEvent::ReasoningDone(payload);
            }
            yield LlmEvent::Completed {
                response_id: None,
                cost: last_cost,
                stop_reason: last_finish_reason
                    .as_deref()
                    .map(crate::StopReason::from_google),
                reasoning_only_stop: false,
            };
        })
    }
}

pub(crate) fn google_stream_url(base_url: &str, model: &str) -> String {
    format!("{base_url}/models/{model}:streamGenerateContent?alt=sse")
}

fn google_contents(input: &[LlmInputItem]) -> Value {
    let mut contents = Vec::new();
    let mut tool_names_by_call_id = BTreeMap::new();
    for item in input {
        match item {
            LlmInputItem::UserText(text) => contents.push(json!({
                "role": "user",
                "parts": [{"text": text}],
            })),
            LlmInputItem::AssistantText(text) => contents.push(json!({
                "role": "model",
                "parts": [{"text": text}],
            })),
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                tool_names_by_call_id.insert(call_id.as_str(), name.as_str());
                contents.push(json!({
                    "role": "model",
                    "parts": [{"functionCall": {"name": name, "args": arguments}}],
                }));
            }
            LlmInputItem::FunctionCallOutput { call_id, output } => {
                let name = tool_names_by_call_id
                    .get(call_id.as_str())
                    .copied()
                    .unwrap_or("tool");
                contents.push(json!({
                "role": "function",
                "parts": [{"functionResponse": {
                    "name": name,
                    "response": {"output": output},
                }}],
                }));
            }
            LlmInputItem::Image { media_type, bytes } => contents.push(json!({
                "role": "user",
                "parts": [{
                    "inlineData": {
                        "mimeType": media_type,
                        "data": BASE64_STANDARD.encode(bytes.as_ref()),
                    },
                }],
            })),
            LlmInputItem::Reasoning(ReasoningPayload::Google {
                summary,
                thought_signature,
            }) => {
                let parts: Vec<Value> = summary
                    .iter()
                    .map(|text| {
                        let mut part = json!({
                            "text": text,
                            "thought": true,
                        });
                        if let Some(sig) = thought_signature {
                            part["thoughtSignature"] = json!(sig);
                        }
                        part
                    })
                    .collect();
                if !parts.is_empty() {
                    contents.push(json!({
                        "role": "model",
                        "parts": parts,
                    }));
                }
            }
            // Reasoning items from other providers are dropped when replaying to Google.
            LlmInputItem::Reasoning(_) => {}
        }
    }
    Value::Array(contents)
}

#[derive(Debug, Default)]
struct GoogleReasoningBuffer {
    summary: Vec<String>,
    signature: Option<String>,
}

impl GoogleReasoningBuffer {
    fn push(&mut self, text: &str, signature: Option<&str>) {
        if !text.is_empty() {
            self.summary.push(text.to_string());
        }
        if let Some(sig) = signature {
            self.signature = Some(sig.to_string());
        }
    }

    fn flush(&mut self) -> Option<ReasoningPayload> {
        if self.summary.is_empty() && self.signature.is_none() {
            return None;
        }
        let summary = std::mem::take(&mut self.summary);
        let thought_signature = self.signature.take();
        Some(ReasoningPayload::Google {
            summary,
            thought_signature,
        })
    }
}

fn parse_google_event(
    data: &str,
    cost: &mut CostSnapshot,
    last_finish_reason: &mut Option<String>,
    reasoning_buf: &mut GoogleReasoningBuffer,
    server_model_slot: &mut Option<String>,
) -> Result<Vec<LlmEvent>> {
    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid Google SSE JSON: {err}")))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Google stream error");
        return Err(SqueezyError::ProviderStream(message.to_string()));
    }
    if server_model_slot.is_none()
        && let Some(server_model) = value.get("modelVersion").and_then(Value::as_str)
        && !server_model.is_empty()
    {
        // Google's `streamGenerateContent` echoes `modelVersion` on
        // every chunk (the pinned snapshot id, e.g. `gemini-2.5-pro` →
        // `gemini-2.5-pro-002`). Capture the first occurrence; the
        // outer stream loop drains the slot and emits `ServerModel`
        // once when the snapshot id differs from `request.model`.
        *server_model_slot = Some(server_model.to_string());
    }
    if let Some(usage) = value.get("usageMetadata") {
        cost.input_tokens = usage.get("promptTokenCount").and_then(Value::as_u64);
        cost.output_tokens = usage.get("candidatesTokenCount").and_then(Value::as_u64);
        cost.cached_input_tokens = usage.get("cachedContentTokenCount").and_then(Value::as_u64);
        cost.reasoning_output_tokens = usage.get("thoughtsTokenCount").and_then(Value::as_u64);
    }
    if let Some(reason) = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("finishReason"))
        .and_then(Value::as_str)
    {
        *last_finish_reason = Some(reason.to_string());
    }
    let mut events = Vec::new();
    let parts = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array);
    let Some(parts) = parts else {
        return Ok(events);
    };
    for (index, part) in parts.iter().enumerate() {
        let is_thought = part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            if is_thought {
                let signature = part.get("thoughtSignature").and_then(Value::as_str);
                reasoning_buf.push(text, signature);
                events.push(LlmEvent::ReasoningDelta {
                    text: text.to_string(),
                    kind: ReasoningKind::Summary,
                });
                continue;
            }
            if let Some(payload) = reasoning_buf.flush() {
                events.push(LlmEvent::ReasoningDone(payload));
            }
            events.push(LlmEvent::TextDelta(text.to_string()));
        }
        if let Some(function_call) = part.get("functionCall") {
            if let Some(payload) = reasoning_buf.flush() {
                events.push(LlmEvent::ReasoningDone(payload));
            }
            let name = function_call
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SqueezyError::ProviderStream("Google functionCall missing name".to_string())
                })?
                .to_string();
            let arguments = function_call
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            events.push(LlmEvent::ToolCall(LlmToolCall {
                call_id: format!("google_call_{index}"),
                name,
                arguments,
            }));
        }
    }
    Ok(events)
}

#[cfg(test)]
#[path = "google_tests.rs"]
mod tests;
