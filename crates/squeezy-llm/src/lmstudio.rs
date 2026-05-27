//! LM Studio provider client.
//!
//! LM Studio (<https://lmstudio.ai>) is a desktop app that hosts local OSS
//! models behind an OpenAI-compatible HTTP server (`/v1/chat/completions`,
//! `/v1/models`, `/v1/responses`) at `http://localhost:1234` by default.
//! This client speaks the Chat Completions wire (the most universally
//! supported endpoint on LM Studio releases) and mirrors the structure of
//! [`crate::compatible::OpenAiCompatibleProvider`] without the aggregator-
//! specific bits (no API key required, no preset header injection, no
//! Anthropic cache_control markers).

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{CostSnapshot, ProviderTransportConfig, Result, SqueezyError};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall,
    retry::{RetryPolicy, idle_timeout, send_with_retry},
    sse::SseDecoder,
};

/// Default base URL for a freshly installed LM Studio server. The desktop
/// app binds `127.0.0.1:1234` by default and exposes the OpenAI-compatible
/// endpoints under `/v1`.
pub const DEFAULT_LMSTUDIO_BASE_URL: &str = "http://localhost:1234/v1";

/// Configuration for the LM Studio provider. Mirrors the shape of
/// [`squeezy_core::OllamaConfig`] — base URL plus shared transport — and
/// adds an optional bearer token for users who put LM Studio behind a
/// reverse proxy that enforces auth.
#[derive(Debug, Clone)]
pub struct LMStudioConfig {
    /// Server root including `/v1` (e.g. `http://localhost:1234/v1`).
    pub base_url: String,
    /// Optional bearer token. LM Studio ignores the header out of the box,
    /// but any reverse proxy in front of it can require one.
    pub api_key: Option<String>,
    pub transport: ProviderTransportConfig,
}

impl Default for LMStudioConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_LMSTUDIO_BASE_URL.to_string(),
            api_key: None,
            transport: ProviderTransportConfig::default(),
        }
    }
}

#[derive(Clone)]
pub struct LMStudioProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for LMStudioProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LMStudioProvider")
            .field("client", &self.client)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("transport", &self.transport)
            .finish()
    }
}

impl LMStudioProvider {
    pub fn from_config(config: &LMStudioConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key.clone(),
            transport: config.transport,
        }
    }

    pub(crate) fn request_body(request: &LlmRequest) -> Value {
        let mut messages = Vec::with_capacity(request.input.len() + 1);
        let trimmed_instructions = request.instructions.trim();
        if !trimmed_instructions.is_empty() {
            messages.push(json!({
                "role": "system",
                "content": &*request.instructions,
            }));
        }
        for item in request.input.iter() {
            if let Some(msg) = lmstudio_message(item) {
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

impl LlmProvider for LMStudioProvider {
    fn name(&self) -> &'static str {
        "lmstudio"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let transport = self.transport;
        let url = format!("{}/chat/completions", self.base_url);
        let body = Self::request_body(&request);

        Box::pin(try_stream! {
            let response = send_with_retry(
                RetryPolicy::provider_requests(transport),
                &cancel,
                || {
                    let mut builder = client.post(&url);
                    if let Some(key) = api_key.as_ref() {
                        builder = builder.bearer_auth(key);
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
                Err(SqueezyError::ProviderRequest(format!(
                    "LM Studio {status}: {message}"
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
                    SqueezyError::ProviderStream("LM Studio stream idle timeout".to_string())
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

            // LM Studio closed the stream without `[DONE]`. Emit any pending
            // tool calls and a Completed event so the agent loop terminates.
            for emitted in state.drain_tool_calls()? {
                yield emitted;
            }
            if !state.completed_emitted {
                let stop_reason = state.finish_reason.as_deref().map(lmstudio_stop_reason);
                yield LlmEvent::Completed {
                    response_id: state.response_id.take(),
                    cost: state.cost.clone(),
                    stop_reason,
                    reasoning_only_stop: false,
                };
            }
        })
    }
}

/// Map OpenAI chat-completions `finish_reason` strings observed via
/// LM Studio into the normalized [`crate::StopReason`]. Mirrors
/// `compatible.rs::chat_stop_reason`; kept inline so the two providers
/// stay independent.
fn lmstudio_stop_reason(value: &str) -> crate::StopReason {
    match value {
        "stop" => crate::StopReason::EndTurn,
        "tool_calls" | "function_call" => crate::StopReason::ToolUse,
        "length" => crate::StopReason::MaxTokens,
        "content_filter" => crate::StopReason::Refusal,
        other => crate::StopReason::Other(other.to_string()),
    }
}

/// Probe a running LM Studio server for the catalog of loaded models.
///
/// Returns an empty vector when the server is unreachable so callers can
/// degrade gracefully (the model picker treats it as "no live discovery").
pub async fn fetch_lmstudio_model_names(base_url: &str) -> Vec<String> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let value: Value = match client.get(url).send().await {
        Ok(response) => match response.json().await {
            Ok(value) => value,
            Err(_) => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    lmstudio_model_names_from_models(&value)
}

pub(crate) fn lmstudio_model_names_from_models(value: &Value) -> Vec<String> {
    value
        .get("data")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn lmstudio_message(item: &LlmInputItem) -> Option<Value> {
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
        // Local OSS models have no signed reasoning replay format; skip.
        LlmInputItem::Reasoning(_) => return None,
    })
}

#[derive(Debug, Default)]
pub(crate) struct StreamState {
    response_id: Option<String>,
    cost: CostSnapshot,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    completed_emitted: bool,
    /// Captured OpenAI chat-completions `finish_reason` from the last
    /// streamed choice; mapped to [`crate::StopReason`] at completion.
    finish_reason: Option<String>,
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
            let name = partial.name.ok_or_else(|| {
                SqueezyError::ProviderStream(
                    "LM Studio tool call missing function name".to_string(),
                )
            })?;
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

pub(crate) fn parse_chat_event(data: &str, state: &mut StreamState) -> Result<Vec<LlmEvent>> {
    if data == "[DONE]" {
        let mut events = state.drain_tool_calls()?;
        if !state.completed_emitted {
            let stop_reason = state.finish_reason.as_deref().map(lmstudio_stop_reason);
            events.push(LlmEvent::Completed {
                response_id: state.response_id.take(),
                cost: state.cost.clone(),
                stop_reason,
                reasoning_only_stop: false,
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
            .unwrap_or("LM Studio stream error")
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
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(Value::as_str)
                    && !content.is_empty()
                {
                    events.push(LlmEvent::TextDelta(content.to_string()));
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
                if matches!(
                    finish_reason,
                    "tool_calls" | "function_call" | "stop" | "length" | "content_filter"
                ) {
                    events.extend(state.drain_tool_calls()?);
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
    CostSnapshot {
        input_tokens: prompt_tokens,
        output_tokens: completion_tokens,
        reasoning_output_tokens: None,
        cached_input_tokens: None,
        cache_write_input_tokens: None,
        estimated_usd_micros: None,
    }
}

#[cfg(test)]
#[path = "lmstudio_tests.rs"]
mod tests;
