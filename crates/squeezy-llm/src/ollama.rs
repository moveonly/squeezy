use async_stream::try_stream;
use futures_util::StreamExt;
use serde_json::{Value, json};
use squeezy_core::{CostSnapshot, OllamaConfig, Result, SqueezyError};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall};

#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
}

impl OllamaProvider {
    pub fn from_config(config: &OllamaConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
        }
    }

    pub(crate) fn request_body(request: &LlmRequest) -> Value {
        let mut body = json!({
            "model": request.model,
            "messages": ollama_messages(&request.instructions, &request.input),
            "stream": true,
        });
        if let Some(max_output_tokens) = request.max_output_tokens {
            body["options"] = json!({ "num_predict": max_output_tokens });
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(
                request
                    .tools
                    .iter()
                    .map(|tool| json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    }))
                    .collect::<Vec<_>>()
            );
        }
        body
    }
}

pub async fn fetch_ollama_context_window(base_url: &str, model: &str) -> Option<u64> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(250))
        .build()
        .ok()?;
    let url = format!("{}/show", base_url.trim_end_matches('/'));
    let value: Value = client
        .post(url)
        .json(&json!({ "model": model }))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    ollama_context_window_from_show(&value)
}

pub async fn fetch_ollama_model_names(base_url: &str) -> Vec<String> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(250))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let url = format!("{}/tags", base_url.trim_end_matches('/'));
    let value: Value = match client.get(url).send().await {
        Ok(response) => match response.json().await {
            Ok(value) => value,
            Err(_) => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    ollama_model_names_from_tags(&value)
}

pub(crate) fn ollama_model_names_from_tags(value: &Value) -> Vec<String> {
    value
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn ollama_context_window_from_show(value: &Value) -> Option<u64> {
    value
        .get("model_info")
        .and_then(Value::as_object)
        .and_then(|info| {
            info.iter().find_map(|(key, value)| {
                if key.ends_with(".context_length") {
                    value.as_u64()
                } else {
                    None
                }
            })
        })
        .or_else(|| {
            value
                .get("parameters")
                .and_then(Value::as_str)
                .and_then(parse_num_ctx)
        })
}

fn parse_num_ctx(parameters: &str) -> Option<u64> {
    parameters.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("num_ctx"), Some(value)) => value.parse().ok(),
            _ => None,
        }
    })
}

impl LlmProvider for OllamaProvider {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        let client = self.client.clone();
        let url = format!("{}/chat", self.base_url);
        let body = Self::request_body(&request);

        Box::pin(try_stream! {
            let response_result = tokio::select! {
                _ = cancel.cancelled() => {
                    yield LlmEvent::Cancelled;
                    return;
                }
                response = client.post(&url).json(&body).send() => response,
            };
            let response = response_result
                .map_err(|err| SqueezyError::ProviderRequest(err.to_string()))?;
            let status = response.status();
            let response = if status.is_success() {
                response
            } else {
                let message = response.text().await.unwrap_or_else(|_| "failed to read error response".to_string());
                Err(SqueezyError::ProviderRequest(format!("{status}: {message}")))?;
                unreachable!("provider error returned above");
            };

            yield LlmEvent::Started;
            let mut decoder = JsonLineDecoder::default();
            let mut bytes = response.bytes_stream();
            while let Some(chunk) = tokio::select! {
                _ = cancel.cancelled() => {
                    yield LlmEvent::Cancelled;
                    return;
                }
                chunk = bytes.next() => chunk
            } {
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for line in decoder.push(&chunk) {
                    for event in parse_ollama_line(&line)? {
                        yield event;
                    }
                }
            }
            for line in decoder.finish() {
                for event in parse_ollama_line(&line)? {
                    yield event;
                }
            }
        })
    }
}

fn ollama_messages(instructions: &str, input: &[LlmInputItem]) -> Value {
    let mut messages = Vec::new();
    if !instructions.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": instructions }));
    }
    for item in input {
        match item {
            LlmInputItem::UserText(text) => {
                messages.push(json!({ "role": "user", "content": text }));
            }
            LlmInputItem::AssistantText(text) => {
                messages.push(json!({ "role": "assistant", "content": text }));
            }
            LlmInputItem::FunctionCall {
                call_id: _,
                name,
                arguments,
            } => {
                messages.push(json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }]
                }));
            }
            LlmInputItem::FunctionCallOutput { call_id: _, output } => {
                messages.push(json!({ "role": "tool", "content": output }));
            }
        }
    }
    Value::Array(messages)
}

#[derive(Debug, Default)]
struct JsonLineDecoder {
    buffer: Vec<u8>,
}

impl JsonLineDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line = self.buffer.drain(..=index).collect::<Vec<_>>();
            if let Ok(text) = String::from_utf8(line) {
                let text = text.trim();
                if !text.is_empty() {
                    lines.push(text.to_string());
                }
            }
        }
        lines
    }

    fn finish(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let line = std::mem::take(&mut self.buffer);
        String::from_utf8(line)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .into_iter()
            .collect()
    }
}

fn parse_ollama_line(line: &str) -> Result<Vec<LlmEvent>> {
    let value: Value = serde_json::from_str(line)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid Ollama JSON: {err}")))?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(SqueezyError::ProviderStream(error.to_string()));
    }

    let mut events = Vec::new();
    if let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        && !content.is_empty()
    {
        events.push(LlmEvent::TextDelta(content.to_string()));
    }
    if let Some(tool_calls) = value
        .get("message")
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for (index, tool_call) in tool_calls.iter().enumerate() {
            let Some(function) = tool_call.get("function") else {
                continue;
            };
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SqueezyError::ProviderStream("Ollama tool call missing name".to_string())
                })?
                .to_string();
            let arguments = function
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            events.push(LlmEvent::ToolCall(LlmToolCall {
                call_id: format!("ollama_call_{index}"),
                name,
                arguments,
            }));
        }
    }
    if value.get("done").and_then(Value::as_bool) == Some(true) {
        events.push(LlmEvent::Completed {
            response_id: None,
            cost: CostSnapshot {
                input_tokens: value.get("prompt_eval_count").and_then(Value::as_u64),
                output_tokens: value.get("eval_count").and_then(Value::as_u64),
                reasoning_output_tokens: None,
                cached_input_tokens: None,
                cache_write_input_tokens: None,
                estimated_usd_micros: Some(0),
            },
        });
    }
    Ok(events)
}

#[cfg(test)]
#[path = "ollama_tests.rs"]
mod tests;
