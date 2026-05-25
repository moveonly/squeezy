use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{
    AzureOpenAiConfig, CostSnapshot, OpenAiConfig, ProviderTransportConfig, ResponseVerbosity,
    Result, SqueezyError,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    INVALID_TOOL_ARGUMENTS_ERROR_KEY, INVALID_TOOL_ARGUMENTS_KEY, INVALID_TOOL_ARGUMENTS_RAW_KEY,
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall,
    credentials::resolve_api_key,
    retry::{RetryPolicy, idle_timeout, send_with_retry},
};

#[derive(Clone)]
pub struct OpenAiProvider {
    name: &'static str,
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    api_version: Option<String>,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("name", &self.name)
            .field("client", &self.client)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("api_version", &self.api_version)
            .field("transport", &self.transport)
            .finish()
    }
}

impl OpenAiProvider {
    pub fn from_config(config: &OpenAiConfig) -> Result<Self> {
        let api_key = resolve_api_key(&config.api_key_env)?;
        Ok(Self {
            name: "openai",
            client: reqwest::Client::new(),
            api_key,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_version: None,
            transport: config.transport,
        })
    }

    pub fn from_azure_config(config: &AzureOpenAiConfig) -> Result<Self> {
        if config.base_url.trim().is_empty() {
            return Err(SqueezyError::ProviderNotConfigured(
                "missing AZURE_OPENAI_BASE_URL or providers.azure_openai.base_url".to_string(),
            ));
        }
        let api_key = resolve_api_key(&config.api_key_env)?;
        Ok(Self {
            name: "azure_openai",
            client: reqwest::Client::new(),
            api_key,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_version: Some(config.api_version.clone()),
            transport: config.transport,
        })
    }

    fn request_body(request: &LlmRequest) -> Value {
        let mut body = json!({
            "model": request.model,
            "instructions": request.instructions,
            "input": openai_input(&request.input),
            "stream": true,
            "store": request.store,
        });
        if let Some(previous_response_id) = &request.previous_response_id {
            body["previous_response_id"] = json!(previous_response_id);
        }
        if let Some(cache_key) = &request.cache_key {
            body["prompt_cache_key"] = json!(cache_key);
        }
        if let Some(max_output_tokens) = request.max_output_tokens {
            body["max_output_tokens"] = json!(max_output_tokens);
        }
        if let Some(response_verbosity) = request.response_verbosity {
            body["text"] = json!({ "verbosity": openai_text_verbosity(response_verbosity) });
        }
        if let Some(reasoning_effort) = request.reasoning_effort {
            body["reasoning"] = json!({ "effort": reasoning_effort.as_str() });
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
        }
        body
    }
}

fn openai_text_verbosity(verbosity: ResponseVerbosity) -> &'static str {
    match verbosity {
        ResponseVerbosity::Concise => "low",
        ResponseVerbosity::Normal => "medium",
        ResponseVerbosity::Verbose => "high",
    }
}

impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let provider_name = self.name;
        let transport = self.transport;
        let mut url = format!("{}/responses", self.base_url);
        if let Some(api_version) = &self.api_version {
            url.push_str("?api-version=");
            url.push_str(api_version);
        }
        let body = Self::request_body(&request);

        Box::pin(try_stream! {
            let response = send_with_retry(RetryPolicy::provider_requests(transport), &cancel, || {
                    let builder = client.post(&url);
                    let builder = if provider_name == "azure_openai" {
                        builder.header("api-key", api_key.clone())
                    } else {
                        builder.bearer_auth(api_key.clone())
                    };
                    builder.json(&body)
                }).await?;

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
                    if let Some(llm_event) = parse_openai_event(&event)? {
                        if matches!(llm_event, LlmEvent::Completed { .. }) {
                            saw_completed = true;
                        }
                        yield llm_event;
                    }
                }
            }

            for event in decoder.finish() {
                if let Some(llm_event) = parse_openai_event(&event)? {
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

#[derive(Debug, Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();

        while let Some((index, len)) = find_event_boundary(&self.buffer) {
            let event = self.buffer.drain(..index + len).collect::<Vec<_>>();
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

fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));

    match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(if lf.0 < crlf.0 { lf } else { crlf }),
        (Some(lf), None) => Some(lf),
        (None, Some(crlf)) => Some(crlf),
        (None, None) => None,
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

fn parse_openai_event(data: &str) -> Result<Option<LlmEvent>> {
    if data == "[DONE]" {
        return Ok(None);
    }

    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid SSE JSON: {err}")))?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match event_type {
        "response.output_text.delta" => {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Some(LlmEvent::TextDelta(delta)))
        }
        "response.output_item.done" => {
            if let Some(tool_call) = parse_tool_call(value.get("item"))? {
                Ok(Some(LlmEvent::ToolCall(tool_call)))
            } else {
                Ok(None)
            }
        }
        "response.completed" => {
            let response_id = value
                .get("response")
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(Some(LlmEvent::Completed {
                response_id,
                cost: parse_cost(value.get("response")),
            }))
        }
        "response.incomplete" => {
            let message = value
                .get("response")
                .and_then(|response| response.get("incomplete_details"))
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .map(|reason| format!("OpenAI response incomplete: {reason}"))
                .unwrap_or_else(|| "OpenAI response incomplete".to_string());
            Err(SqueezyError::ProviderStream(message))
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
        _ => Ok(None),
    }
}

fn openai_input(input: &[LlmInputItem]) -> Value {
    if let [LlmInputItem::UserText(text)] = input {
        return json!(text);
    }

    Value::Array(input.iter().map(openai_input_item).collect())
}

fn openai_input_item(item: &LlmInputItem) -> Value {
    match item {
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
    }
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
