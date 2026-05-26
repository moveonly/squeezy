use std::collections::BTreeMap;

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{AnthropicConfig, CostSnapshot, ProviderTransportConfig, Result, SqueezyError};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    AnthropicThinkingBlock, AnthropicThinkingKind, LlmEvent, LlmInputItem, LlmProvider, LlmRequest,
    LlmStream, LlmToolCall, ReasoningKind, ReasoningPayload,
    credentials::resolve_api_key,
    retry::{RetryPolicy, idle_timeout, send_with_retry},
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_ANTHROPIC_MAX_OUTPUT_TOKENS: u64 = 64_000;

#[derive(Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("client", &self.client)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("transport", &self.transport)
            .finish()
    }
}

impl AnthropicProvider {
    pub fn from_config(config: &AnthropicConfig) -> Result<Self> {
        let api_key = resolve_api_key(&config.api_key_env)?;
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            transport: config.transport,
        })
    }

    pub(crate) fn request_body(request: &LlmRequest) -> Value {
        let prompt_caching = request.cache_key.is_some()
            && crate::capabilities_for("anthropic", &request.model)
                .is_some_and(|capabilities| capabilities.prompt_caching);
        let max_tokens = request
            .max_output_tokens
            .map(u64::from)
            .or_else(|| {
                crate::model_info_for("anthropic", &request.model)
                    .and_then(|info| info.limits)
                    .map(|limits| limits.max_output_tokens)
            })
            .unwrap_or(DEFAULT_ANTHROPIC_MAX_OUTPUT_TOKENS);
        let mut body = json!({
            "model": request.model,
            "system": anthropic_system(&request.instructions, prompt_caching),
            "messages": anthropic_messages(&request.input, prompt_caching),
            "max_tokens": max_tokens,
            "stream": true,
        });
        if let Some(effort) = request.reasoning_effort
            && crate::capabilities_for("anthropic", &request.model)
                .is_some_and(|caps| caps.reasoning_effort)
        {
            let budget =
                u64::from(effort.thinking_budget_tokens()).min(max_tokens.saturating_sub(1));
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
        }
        if !request.tools.is_empty() {
            let mut tool_values: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                })
                .collect();
            if prompt_caching
                && let Some(obj) = tool_values.last_mut().and_then(Value::as_object_mut)
            {
                obj.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
            }
            body["tools"] = Value::Array(tool_values);
        }
        body
    }
}

fn anthropic_system(instructions: &str, prompt_caching: bool) -> Value {
    if !prompt_caching {
        return json!(instructions);
    }
    json!([{
        "type": "text",
        "text": instructions,
        "cache_control": { "type": "ephemeral" },
    }])
}

fn anthropic_messages(input: &[LlmInputItem], prompt_caching: bool) -> Value {
    let mut messages = Vec::new();
    for item in input {
        match item {
            LlmInputItem::UserText(text) => push_anthropic_message(
                &mut messages,
                "user",
                vec![json!({
                    "type": "text",
                    "text": text,
                })],
            ),
            LlmInputItem::AssistantText(text) => push_anthropic_message(
                &mut messages,
                "assistant",
                vec![json!({
                    "type": "text",
                    "text": text,
                })],
            ),
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => push_anthropic_message(
                &mut messages,
                "assistant",
                vec![json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": arguments,
                })],
            ),
            LlmInputItem::FunctionCallOutput { call_id, output } => push_anthropic_message(
                &mut messages,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": output,
                })],
            ),
            LlmInputItem::Reasoning(ReasoningPayload::Anthropic { blocks }) => {
                let blocks_json: Vec<Value> = blocks
                    .iter()
                    .map(|block| match block.kind {
                        AnthropicThinkingKind::Thinking => {
                            let mut obj = json!({
                                "type": "thinking",
                                "thinking": block.text,
                            });
                            if let Some(signature) = &block.signature {
                                obj["signature"] = json!(signature);
                            }
                            obj
                        }
                        AnthropicThinkingKind::Redacted => {
                            json!({
                                "type": "redacted_thinking",
                                "data": block.data.clone().unwrap_or_default(),
                            })
                        }
                    })
                    .collect();
                if !blocks_json.is_empty() {
                    push_anthropic_message(&mut messages, "assistant", blocks_json);
                }
            }
            // Reasoning items from other providers are dropped when replaying to Anthropic.
            LlmInputItem::Reasoning(_) => {}
        }
    }
    if prompt_caching {
        mark_last_user_block_for_cache(&mut messages);
    }
    Value::Array(messages)
}

fn mark_last_user_block_for_cache(messages: &mut [Value]) {
    for message in messages.iter_mut().rev() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(block) = message
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|content| content.last_mut())
            .and_then(Value::as_object_mut)
        {
            block.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
        }
        return;
    }
}

fn push_anthropic_message(messages: &mut Vec<Value>, role: &str, mut blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        let same_role = last.get("role").and_then(Value::as_str) == Some(role);
        if same_role && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
            content.append(&mut blocks);
            return;
        }
    }

    messages.push(json!({
        "role": role,
        "content": blocks,
    }));
}

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let url = format!("{}/messages", self.base_url);
        let body = Self::request_body(&request);
        let transport = self.transport;

        Box::pin(try_stream! {
            let response = send_with_retry(RetryPolicy::provider_requests(transport), &cancel, || {
                client
                    .post(&url)
                    .header("x-api-key", api_key.clone())
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .json(&body)
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
            let mut state = AnthropicStreamState::default();
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
                    SqueezyError::ProviderStream("Anthropic stream idle timeout".to_string())
                })?;
                let Some(chunk) = next else { break; };
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for event in decoder.push(&chunk) {
                    for llm_event in parse_anthropic_event(&event, &mut state)? {
                        if matches!(llm_event, LlmEvent::Completed { .. }) {
                            saw_completed = true;
                        }
                        yield llm_event;
                    }
                }
            }

            for event in decoder.finish() {
                for llm_event in parse_anthropic_event(&event, &mut state)? {
                    if matches!(llm_event, LlmEvent::Completed { .. }) {
                        saw_completed = true;
                    }
                    yield llm_event;
                }
            }

            if !saw_completed {
                Err(SqueezyError::ProviderStream(
                    "Anthropic stream ended without message_stop".to_string(),
                ))?;
            }
        })
    }
}

#[derive(Debug, Default)]
struct AnthropicStreamState {
    response_id: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    stop_reason: Option<String>,
    tool_blocks: BTreeMap<u64, PartialToolCall>,
    thinking_blocks: BTreeMap<u64, AnthropicThinkingBlock>,
    finished_thinking: Vec<AnthropicThinkingBlock>,
    emitted_reasoning_done: bool,
}

impl AnthropicStreamState {
    fn cost(&self) -> CostSnapshot {
        CostSnapshot {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            reasoning_output_tokens: None,
            cached_input_tokens: self.cache_read_input_tokens,
            cache_write_input_tokens: self.cache_creation_input_tokens,
            estimated_usd_micros: None,
        }
    }
}

#[derive(Debug, Default)]
struct PartialToolCall {
    call_id: String,
    name: String,
    arguments_json: String,
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

    [lf, crlf].into_iter().flatten().min_by_key(|b| b.0)
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

fn parse_anthropic_event(data: &str, state: &mut AnthropicStreamState) -> Result<Vec<LlmEvent>> {
    let value: Value = serde_json::from_str(data)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid SSE JSON: {err}")))?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let single = |evt: LlmEvent| Ok(vec![evt]);
    let none = || Ok(Vec::new());

    match event_type {
        "message_start" => {
            if let Some(message) = value.get("message") {
                state.response_id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                merge_usage(state, message.get("usage"));
            }
            none()
        }
        "content_block_start" => {
            let Some(block) = value.get("content_block") else {
                return none();
            };
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let call_id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            SqueezyError::ProviderStream(
                                "Anthropic tool_use missing id".to_string(),
                            )
                        })?
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            SqueezyError::ProviderStream(
                                "Anthropic tool_use missing name".to_string(),
                            )
                        })?
                        .to_string();
                    let arguments_json = block
                        .get("input")
                        .filter(|input| !input.as_object().is_some_and(serde_json::Map::is_empty))
                        .map(serde_json::to_string)
                        .transpose()
                        .map_err(|err| {
                            SqueezyError::ProviderStream(format!(
                                "failed to serialize Anthropic tool_use input: {err}"
                            ))
                        })?
                        .unwrap_or_default();
                    state.tool_blocks.insert(
                        index,
                        PartialToolCall {
                            call_id,
                            name,
                            arguments_json,
                        },
                    );
                    none()
                }
                Some("thinking") => {
                    let initial_text = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let initial_signature = block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    state.thinking_blocks.insert(
                        index,
                        AnthropicThinkingBlock {
                            kind: AnthropicThinkingKind::Thinking,
                            text: initial_text.clone(),
                            signature: initial_signature,
                            data: None,
                        },
                    );
                    if initial_text.is_empty() {
                        none()
                    } else {
                        single(LlmEvent::ReasoningDelta {
                            text: initial_text,
                            kind: ReasoningKind::Text,
                        })
                    }
                }
                Some("redacted_thinking") => {
                    let data = block
                        .get("data")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    state.thinking_blocks.insert(
                        index,
                        AnthropicThinkingBlock {
                            kind: AnthropicThinkingKind::Redacted,
                            text: String::new(),
                            signature: None,
                            data,
                        },
                    );
                    none()
                }
                _ => none(),
            }
        }
        "content_block_delta" => {
            let Some(delta) = value.get("delta") else {
                return none();
            };
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    let mut events = Vec::new();
                    if !state.finished_thinking.is_empty() && !state.emitted_reasoning_done {
                        let blocks = std::mem::take(&mut state.finished_thinking);
                        state.emitted_reasoning_done = true;
                        events.push(LlmEvent::ReasoningDone(ReasoningPayload::Anthropic {
                            blocks,
                        }));
                    }
                    events.push(LlmEvent::TextDelta(
                        delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ));
                    Ok(events)
                }
                Some("input_json_delta") => {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                    if let Some(tool_call) = state.tool_blocks.get_mut(&index)
                        && let Some(partial_json) =
                            delta.get("partial_json").and_then(Value::as_str)
                    {
                        tool_call.arguments_json.push_str(partial_json);
                    }
                    none()
                }
                Some("thinking_delta") => {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let text = delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if let Some(block) = state.thinking_blocks.get_mut(&index) {
                        block.text.push_str(&text);
                    }
                    if text.is_empty() {
                        none()
                    } else {
                        single(LlmEvent::ReasoningDelta {
                            text,
                            kind: ReasoningKind::Text,
                        })
                    }
                }
                Some("signature_delta") => {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let signature = delta
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if let Some(block) = state.thinking_blocks.get_mut(&index) {
                        match block.signature.as_mut() {
                            Some(existing) => existing.push_str(&signature),
                            None => block.signature = Some(signature),
                        }
                    }
                    none()
                }
                _ => none(),
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(thinking) = state.thinking_blocks.remove(&index) {
                state.finished_thinking.push(thinking);
                return none();
            }
            let Some(tool_call) = state.tool_blocks.remove(&index) else {
                return none();
            };
            let arguments = if tool_call.arguments_json.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&tool_call.arguments_json).map_err(|err| {
                    SqueezyError::ProviderStream(format!(
                        "invalid Anthropic tool_use input JSON: {err}"
                    ))
                })?
            };
            let mut events = Vec::new();
            if !state.finished_thinking.is_empty() && !state.emitted_reasoning_done {
                let blocks = std::mem::take(&mut state.finished_thinking);
                state.emitted_reasoning_done = true;
                events.push(LlmEvent::ReasoningDone(ReasoningPayload::Anthropic {
                    blocks,
                }));
            }
            events.push(LlmEvent::ToolCall(LlmToolCall {
                call_id: tool_call.call_id,
                name: tool_call.name,
                arguments,
            }));
            Ok(events)
        }
        "message_delta" => {
            if let Some(delta) = value.get("delta") {
                state.stop_reason = delta
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            merge_usage(state, value.get("usage"));
            none()
        }
        "message_stop" => {
            if state.stop_reason.as_deref() == Some("max_tokens") {
                return Err(SqueezyError::ProviderStream(
                    "Anthropic response stopped after max_tokens".to_string(),
                ));
            }
            let mut events = Vec::new();
            if !state.finished_thinking.is_empty() && !state.emitted_reasoning_done {
                let blocks = std::mem::take(&mut state.finished_thinking);
                state.emitted_reasoning_done = true;
                events.push(LlmEvent::ReasoningDone(ReasoningPayload::Anthropic {
                    blocks,
                }));
            }
            events.push(LlmEvent::Completed {
                response_id: state.response_id.clone(),
                cost: state.cost(),
            });
            Ok(events)
        }
        "error" => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Anthropic stream error");
            Err(SqueezyError::ProviderStream(message.to_string()))
        }
        _ => none(),
    }
}

fn merge_usage(state: &mut AnthropicStreamState, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };

    state.input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .or(state.input_tokens);
    state.output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .or(state.output_tokens);
    state.cache_read_input_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .or(state.cache_read_input_tokens);
    state.cache_creation_input_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .or(state.cache_creation_input_tokens);
}

#[cfg(test)]
#[path = "anthropic_tests.rs"]
mod tests;
