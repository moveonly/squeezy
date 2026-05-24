use std::{collections::BTreeMap, env};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{CostSnapshot, GoogleConfig, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

use crate::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall};

#[derive(Clone)]
pub struct GoogleProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl std::fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleProvider")
            .field("client", &self.client)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl GoogleProvider {
    pub fn from_config(config: &GoogleConfig) -> Result<Self> {
        let api_key = env::var(&config.api_key_env).map_err(|_| {
            SqueezyError::ProviderNotConfigured(format!("missing {}", config.api_key_env))
        })?;
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: config.base_url.trim_end_matches('/').to_string(),
        })
    }

    pub(crate) fn request_body(request: &LlmRequest) -> Value {
        let mut body = json!({
            "systemInstruction": {
                "parts": [{"text": request.instructions}]
            },
            "contents": google_contents(&request.input),
            "generationConfig": {},
        });
        if let Some(max_output_tokens) = request.max_output_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(max_output_tokens);
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

        Box::pin(try_stream! {
            let response_result = tokio::select! {
                _ = cancel.cancelled() => {
                    yield LlmEvent::Cancelled;
                    return;
                }
                response = client
                    .post(&url)
                    .header("x-goog-api-key", &api_key)
                    .json(&body)
                    .send() => response,
            };
            let response = response_result
                .map_err(|err| SqueezyError::ProviderRequest(err.to_string()))?;
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
            let mut saw_any = false;
            let mut bytes = response.bytes_stream();
            while let Some(chunk) = tokio::select! {
                _ = cancel.cancelled() => {
                    yield LlmEvent::Cancelled;
                    return;
                }
                chunk = bytes.next() => chunk
            } {
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for event in decoder.push(&chunk) {
                    saw_any = true;
                    for llm_event in parse_google_event(&event, &mut last_cost)? {
                        yield llm_event;
                    }
                }
            }
            for event in decoder.finish() {
                saw_any = true;
                for llm_event in parse_google_event(&event, &mut last_cost)? {
                    yield llm_event;
                }
            }
            if !saw_any {
                Err(SqueezyError::ProviderStream("Google stream ended without events".to_string()))?;
            }
            yield LlmEvent::Completed {
                response_id: None,
                cost: last_cost,
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
        }
    }
    Value::Array(contents)
}

#[derive(Debug, Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(index) = self.buffer.windows(2).position(|window| window == b"\n\n") {
            let event = self.buffer.drain(..index + 2).collect::<Vec<_>>();
            if let Some(data) = decode_sse_event(&event) {
                events.push(data);
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let event = std::mem::take(&mut self.buffer);
        decode_sse_event(&event).into_iter().collect()
    }
}

fn decode_sse_event(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut data_lines = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start());
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}

fn parse_google_event(data: &str, cost: &mut CostSnapshot) -> Result<Vec<LlmEvent>> {
    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid Google SSE JSON: {err}")))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Google stream error");
        return Err(SqueezyError::ProviderStream(message.to_string()));
    }
    if let Some(usage) = value.get("usageMetadata") {
        cost.input_tokens = usage.get("promptTokenCount").and_then(Value::as_u64);
        cost.output_tokens = usage.get("candidatesTokenCount").and_then(Value::as_u64);
        cost.cached_input_tokens = usage.get("cachedContentTokenCount").and_then(Value::as_u64);
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
        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            events.push(LlmEvent::TextDelta(text.to_string()));
        }
        if let Some(function_call) = part.get("functionCall") {
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
