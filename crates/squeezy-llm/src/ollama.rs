use async_stream::try_stream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_core::Stream;
use futures_util::StreamExt;
use serde_json::{Value, json};
use squeezy_core::{
    CostSnapshot, OllamaConfig, OllamaRoute, ProviderTransportConfig, Result, SqueezyError,
};
use std::pin::Pin;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall,
    lmstudio::{LMStudioConfig, LMStudioProvider},
    retry::{RetryPolicy, idle_timeout, send_with_retry},
    transport::shared_client,
};

/// Default `options.num_ctx` stamped on every native `/api/chat` request.
/// Ollama's server default is 4096, which silently truncates agent prompts.
/// 32k is the upper bound of opencode's recommended 16k–32k tool-calling
/// safe range. Agents that have probed the model's true window via
/// `fetch_ollama_context_window` can override at a higher layer.
pub(crate) const DEFAULT_NUM_CTX: u64 = 32_768;

// TODO(audit C-06): `DEFAULT_OLLAMA_BASE_URL` in `squeezy-core` still bakes in
// the `/api` suffix and the config layer reads `OLLAMA_BASE_URL` without
// falling back to the canonical `OLLAMA_HOST` env var. The URL helpers below
// (`api_endpoint_url`, `ollama_host_root`) absorb any base shape so users who
// set `OLLAMA_HOST=http://host:11434` still reach the right endpoint; the
// core-side constant and env-fallback fixes ship in Phase 4FH alongside the
// `OllamaConfig` field additions.
#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    transport: ProviderTransportConfig,
    compat: Option<LMStudioProvider>,
    /// Optional `keep_alive` value forwarded to every native chat request.
    /// Accepts Ollama's documented forms — duration strings (`"5m"`,
    /// `"24h"`), integer seconds (`"30"`), `"0"` to evict immediately,
    /// or `"-1"` to keep the model resident forever. Read from
    /// `OllamaConfig.keep_alive` once Phase 4FH lands the squeezy-core
    /// field; populated as `None` until then.
    keep_alive: Option<String>,
    /// Optional bearer token for Ollama Cloud / reverse-proxy-protected
    /// self-hosted Ollama. Read from `OllamaConfig.api_key` once Phase
    /// 4FH lands the squeezy-core field; populated as `None` until then.
    api_key: Option<String>,
}

impl OllamaProvider {
    pub fn from_config(config: &OllamaConfig) -> Self {
        let base_url = config.base_url.trim_end_matches('/').to_string();
        let compat = match config.route_style {
            OllamaRoute::Native => None,
            OllamaRoute::OpenAiCompatible => Some(LMStudioProvider::from_config(&LMStudioConfig {
                base_url: openai_compat_base_url(&base_url),
                api_key: None,
                transport: config.transport,
            })),
        };
        Self {
            client: shared_client(&config.transport),
            base_url,
            transport: config.transport,
            compat,
            // TODO(audit H-16): plumb `OllamaConfig.keep_alive` once Phase 4FH
            // adds the field to squeezy-core. Read path is wired through
            // `with_keep_alive` and `request_body` today.
            keep_alive: None,
            // TODO(audit Low/OAuth): plumb `OllamaConfig.api_key` once Phase
            // 4FH adds the field to squeezy-core. Read path is wired through
            // `with_api_key` and the native request layer today.
            api_key: None,
        }
    }

    /// Override the optional `keep_alive` value used on every native chat
    /// request. Returns `self` for chaining; intended for use sites that
    /// construct the provider directly (e.g. tests, future config-bridge
    /// glue) ahead of the Phase 4FH config field landing.
    pub fn with_keep_alive(mut self, keep_alive: impl Into<String>) -> Self {
        self.keep_alive = Some(keep_alive.into());
        self
    }

    /// Override the optional bearer token used on every native request
    /// (e.g. Ollama Cloud, reverse-proxy-protected self-host). Returns
    /// `self` for chaining; same provisional plumb as `with_keep_alive`.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    #[cfg(test)]
    pub(crate) fn request_body(request: &LlmRequest) -> Value {
        Self::request_body_with(request, None)
    }

    pub(crate) fn request_body_with(request: &LlmRequest, keep_alive: Option<&str>) -> Value {
        // Canonicalize tool-call ids and synthesize placeholders for
        // orphan tool results before building Ollama's `messages`
        // array. Ollama's native route drops the `call_id` on the
        // wire entirely (it pairs by surrounding role/tool blocks),
        // but normalization still synthesizes the assistant
        // tool-call message that an orphan tool-result needs to sit
        // after — without that the `role:"tool"` message appears in
        // the chat with no preceding assistant call and the model
        // gets confused about the conversation order.
        let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
        let mut body = json!({
            "model": request.model,
            "messages": ollama_messages(&request.instructions, &normalized_input),
            "stream": true,
        });
        // Ollama's server default for `num_ctx` is 4096 tokens
        // (`OLLAMA_CONTEXT_LENGTH=4096`). 4096 fits the system prompt + a
        // single short turn; agent workloads with tool descriptions,
        // history, and tool outputs blow through it instantly and Ollama
        // silently drops the oldest messages. Tool-calling reliability
        // collapses below ~16k. Stamp 32k by default so every native
        // chat request gets a workable window. Callers that have probed
        // the model's true `model_info.*.context_length` via
        // `fetch_ollama_context_window` can override at the agent layer.
        // Reference: opencode providers docs (Ollama), Ollama FAQ.
        let mut options = json!({ "num_ctx": DEFAULT_NUM_CTX });
        if let Some(max_output_tokens) = request.max_output_tokens {
            options["num_predict"] = json!(max_output_tokens);
        }
        body["options"] = options;
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
        // Pass through configured `keep_alive` so the server retains the
        // model between turns (or evicts it eagerly). Ollama's default
        // is 5 minutes; agents idling longer pay the full load tax on
        // resume. Accepts duration strings (`"5m"`, `"24h"`), integer
        // seconds (`"30"`), `"0"` for immediate eviction, or `"-1"` to
        // pin the model indefinitely.
        if let Some(value) = keep_alive {
            body["keep_alive"] = json!(value);
        }
        // Thinking-model support. Ollama 0.6+ exposes a `think` request
        // parameter that gates the model's reasoning trace into a separate
        // `message.thinking` channel for qwen3 / deepseek-r1 / gpt-oss.
        // Engage it when the caller asked for reasoning explicitly via
        // `reasoning_effort`, or when the requested model is in the known
        // thinking-capable allow-list. gpt-oss takes `"low" | "medium" |
        // "high"`; other models take `true`.
        if let Some(value) = think_value_for_request(request) {
            body["think"] = value;
        }
        body
    }
}

/// Compute the value to send for `body["think"]` on a native Ollama chat
/// request. Returns `None` when the request is not asking for reasoning and
/// the model is not in the thinking-capable allow-list.
fn think_value_for_request(request: &LlmRequest) -> Option<Value> {
    let model: &str = &request.model;
    let is_gpt_oss = is_gpt_oss_model(model);
    let wants_reasoning =
        request.reasoning_effort.is_some() || is_thinking_capable_model(model) || is_gpt_oss;
    if !wants_reasoning {
        return None;
    }
    if is_gpt_oss {
        // gpt-oss takes the OpenAI-style effort string. Default to "medium"
        // when the caller did not specify so we still engage thinking.
        let level = request
            .reasoning_effort
            .map(|effort| effort.as_str())
            .unwrap_or("medium");
        return Some(Value::String(level.to_string()));
    }
    Some(Value::Bool(true))
}

/// True when the model id matches one of Ollama's documented thinking-capable
/// families. The match is intentionally a case-insensitive substring check so
/// tags / quantization suffixes (`qwen3:8b-instruct-q4_0`) still hit.
fn is_thinking_capable_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("qwen3") || lower.contains("deepseek-r1") || lower.contains("deepseek-v3.1")
}

/// True when the model id matches the `gpt-oss` family. Split out from
/// `is_thinking_capable_model` because gpt-oss takes the OpenAI-style
/// effort string instead of a bare boolean.
fn is_gpt_oss_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gpt-oss")
}

pub async fn fetch_ollama_context_window(base_url: &str, model: &str) -> Option<u64> {
    // 250 ms was fine on localhost but too tight for any remote / Tailscale /
    // Docker-networked Ollama. 1 s keeps the probe snappy on healthy boxes
    // while letting cold-cache or slow-link servers actually answer.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .ok()?;
    let url = api_endpoint_url(base_url, "show");
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

/// Fetch the set of capabilities Ollama advertises for `model` via `/api/show`.
///
/// Returns `None` on any transport / parse failure (caller should treat as
/// "capabilities unknown" rather than "no capabilities"). The returned
/// strings come straight from Ollama's `capabilities` array — at the time of
/// writing the documented members are `"completion"`, `"tools"`,
/// `"thinking"`, `"vision"`, `"insert"`, and `"embedding"`.
///
/// Intended for gating tool-bearing requests against models whose Modelfile
/// has no `tools` template — those models silently no-op the tool list,
/// producing baffling agent UX. Callers that have already loaded the model
/// can short-circuit by passing `Some` through to the request layer.
///
/// TODO(audit M-21): the matching `LlmRequest::ensure_tool_support` helper
/// lives in `crates/squeezy-llm/src/lib.rs` and is out of this file's scope;
/// the gate will hook this helper through that ensure_* method in a follow-up
/// commit on a sibling-owned file.
// Re-exported by `crates/squeezy-llm/src/lib.rs` in the same Phase 4FH commit
// that adds `LlmRequest::ensure_tool_support`; without that, the in-crate
// dead-code lint trips even though we want the symbol public.
#[allow(dead_code)]
pub async fn fetch_ollama_capabilities(base_url: &str, model: &str) -> Option<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .ok()?;
    let url = api_endpoint_url(base_url, "show");
    let value: Value = client
        .post(url)
        .json(&json!({ "model": model }))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    ollama_capabilities_from_show(&value)
}

pub(crate) fn ollama_capabilities_from_show(value: &Value) -> Option<Vec<String>> {
    value
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
}

pub async fn fetch_ollama_model_names(base_url: &str) -> Vec<String> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let url = api_endpoint_url(base_url, "tags");
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
            // Modelfile parameters can quote the value (`num_ctx "8192"`)
            // or wrap it in single quotes; strip both before parsing so
            // the fallback survives hand-built and older-server cases.
            (Some("num_ctx"), Some(value)) => value.trim_matches(['"', '\'']).parse().ok(),
            _ => None,
        }
    })
}

/// Translate a Native-style Ollama base URL (`http://host:port/api`) into the
/// OpenAI-compatible root (`http://host:port/v1`). If the caller already wrote
/// a `/v1` suffix we leave it alone, and an unsuffixed root gets `/v1`
/// appended so users can give us either shape.
pub(crate) fn openai_compat_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        return trimmed.to_string();
    }
    if let Some(root) = trimmed.strip_suffix("/api") {
        return format!("{root}/v1");
    }
    format!("{trimmed}/v1")
}

/// Strip any trailing `/api`, `/v1`, or trailing slash from an Ollama base URL
/// so the bare host root (`http://host:port`) is left. Users follow Ollama's
/// upstream convention and set `OLLAMA_HOST=http://host:port`; squeezy's
/// default bakes `/api` into the configured base. Helpers route every native
/// endpoint through `api_endpoint_url` so the host root is always recovered
/// before the per-endpoint path (`/api/chat`, `/api/show`, ...) is appended.
pub(crate) fn ollama_host_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if let Some(root) = trimmed.strip_suffix("/api") {
        return root.trim_end_matches('/').to_string();
    }
    if let Some(root) = trimmed.strip_suffix("/v1") {
        return root.trim_end_matches('/').to_string();
    }
    trimmed.to_string()
}

/// Build a fully-qualified native Ollama endpoint URL given any user-supplied
/// base shape and a bare endpoint name (e.g. `"chat"`, `"show"`, `"pull"`,
/// `"tags"`). Always emits `<host_root>/api/<endpoint>` regardless of whether
/// the caller's base URL ended in `/api`, `/v1`, a trailing slash, or nothing.
pub(crate) fn api_endpoint_url(base_url: &str, endpoint: &str) -> String {
    let host = ollama_host_root(base_url);
    let path = endpoint.trim_start_matches('/');
    format!("{host}/api/{path}")
}

impl LlmProvider for OllamaProvider {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        if let Err(err) = request.ensure_vision_support("ollama") {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        if let Some(compat) = &self.compat {
            return compat.stream_response(request, cancel);
        }
        let client = self.client.clone();
        let url = api_endpoint_url(&self.base_url, "chat");
        let body = Self::request_body_with(&request, self.keep_alive.as_deref());
        let transport = self.transport;
        let api_key = self.api_key.clone();

        Box::pin(try_stream! {
            let response = send_with_retry(RetryPolicy::provider_requests(transport), &cancel, || {
                let mut builder = client.post(&url).json(&body);
                if let Some(token) = api_key.as_deref() {
                    builder = builder.bearer_auth(token);
                }
                builder
            }).await?;
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
            let mut server_model_slot: Option<String> = None;
            let mut server_model_echo = crate::ServerModelEcho::default();
            let mut bytes = response.bytes_stream();
            loop {
                let polled = tokio::select! {
                    _ = cancel.cancelled() => {
                        // Mid-stream cancellation skips Ollama's terminal
                        // `done: true` frame, so the agent loop never sees a
                        // `Completed` event and accounting under-reports
                        // usage on every cancelled local turn. Emit a final
                        // `Completed { stop_reason: None }` after the
                        // `Cancelled` marker so consumers terminate cleanly
                        // (mirrors LM Studio's early-termination drain).
                        yield LlmEvent::Cancelled;
                        yield LlmEvent::Completed {
                            response_id: None,
                            cost: CostSnapshot::default(),
                            stop_reason: None,
                            reasoning_only_stop: false,
                        };
                        return;
                    }
                    next = timeout(idle_timeout(transport), bytes.next()) => next,
                };
                let next = polled.map_err(|_| {
                    SqueezyError::ProviderStream("Ollama stream idle timeout".to_string())
                })?;
                let Some(chunk) = next else { break; };
                let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
                for line in decoder.push(&chunk) {
                    let parsed = parse_ollama_line(&line, &mut server_model_slot)?;
                    if let Some(server) = server_model_slot.take()
                        && let Some(echo) = server_model_echo.observe(&request.model, &server)
                    {
                        yield echo;
                    }
                    for event in parsed {
                        yield event;
                    }
                }
            }
            for line in decoder.finish() {
                let parsed = parse_ollama_line(&line, &mut server_model_slot)?;
                if let Some(server) = server_model_slot.take()
                    && let Some(echo) = server_model_echo.observe(&request.model, &server)
                {
                    yield echo;
                }
                for event in parsed {
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
            // Ollama's native chat API puts images on the message itself
            // (`{"role": "user", "content": "...", "images": ["<b64>"]}`)
            // instead of a content-block array. When the previous message
            // is a user-text turn (the usual "what is this?" prompt),
            // attach the image to *that* message — vision models pair the
            // image with the most recent user text and an empty-content
            // image-only turn after the text turn changes the semantics.
            // Fall back to a standalone image-only user message when
            // there is no preceding user-text message to attach to.
            LlmInputItem::Image {
                media_type: _,
                bytes,
            } => {
                let encoded = BASE64_STANDARD.encode(bytes.as_ref());
                let attached = messages.last_mut().is_some_and(|last| {
                    if last.get("role").and_then(Value::as_str) != Some("user") {
                        return false;
                    }
                    let has_text = last
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty());
                    if !has_text {
                        return false;
                    }
                    let images = last
                        .as_object_mut()
                        .expect("messages built as json objects above")
                        .entry("images".to_string())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Value::Array(arr) = images {
                        arr.push(Value::String(encoded.clone()));
                        return true;
                    }
                    false
                });
                if !attached {
                    messages.push(json!({
                        "role": "user",
                        "content": "",
                        "images": [encoded],
                    }));
                }
            }
            // Ollama has no signed reasoning replay format. Skip on replay.
            LlmInputItem::Reasoning(_) => {}
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

fn parse_ollama_line(line: &str, server_model_slot: &mut Option<String>) -> Result<Vec<LlmEvent>> {
    let value: Value = serde_json::from_str(line)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid Ollama JSON: {err}")))?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(SqueezyError::ProviderStream(error.to_string()));
    }

    if server_model_slot.is_none()
        && let Some(server_model) = value.get("model").and_then(Value::as_str)
        && !server_model.is_empty()
    {
        // Ollama echoes `model` on every NDJSON chunk. Capture the
        // first occurrence — the outer stream loop drains it and
        // emits `ServerModel` once when the canonical tag (e.g.
        // `llama3:latest` → `llama3:8b-instruct-q4_0`) differs from
        // the user-supplied request model.
        *server_model_slot = Some(server_model.to_string());
    }

    let mut events = Vec::new();
    // Ollama 0.6+ surfaces reasoning traces on a dedicated `message.thinking`
    // field when the request set `think: true` (or the gpt-oss effort
    // string). Emit it on the reasoning channel so the agent's reasoning-only
    // stop detection and TUI separator can see it instead of bleeding into
    // TextDelta.
    //
    // TODO(audit H-17): there is no `ReasoningPayload::Ollama` variant yet,
    // so we do not emit a terminal `ReasoningDone` — Ollama also has no
    // signed replay format that would let us round-trip the trace into a
    // follow-up turn (see `LlmInputItem::Reasoning(_)` handling above).
    // Phase 4FH lands the payload variant when it touches the core type.
    if let Some(thinking) = value
        .get("message")
        .and_then(|message| message.get("thinking"))
        .and_then(Value::as_str)
        && !thinking.is_empty()
    {
        events.push(LlmEvent::ReasoningDelta {
            text: thinking.to_string(),
            kind: crate::ReasoningKind::Text,
        });
    }
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
            let arguments = match function.get("arguments") {
                None => Value::Object(Default::default()),
                // Ollama normally returns `arguments` already parsed as a
                // JSON object. Smaller / quantized OSS models that learned
                // OpenAI conventions sometimes emit it as a JSON-encoded
                // string instead. Parse the string so the tool registry
                // sees a structured value; on parse failure attach the
                // shared INVALID_TOOL_ARGUMENTS marker so the agent can
                // surface a clear error instead of silently mishandling.
                // Mirrors `lmstudio.rs:drain_tool_calls`.
                Some(Value::String(raw)) => {
                    let raw_text = raw.clone();
                    serde_json::from_str::<Value>(raw).unwrap_or_else(|err| {
                        json!({
                            crate::INVALID_TOOL_ARGUMENTS_KEY: true,
                            crate::INVALID_TOOL_ARGUMENTS_ERROR_KEY: err.to_string(),
                            crate::INVALID_TOOL_ARGUMENTS_RAW_KEY: raw_text,
                        })
                    })
                }
                Some(other) => other.clone(),
            };
            events.push(LlmEvent::ToolCall(LlmToolCall {
                call_id: format!("ollama_call_{index}"),
                name,
                arguments,
            }));
        }
    }
    if value.get("done").and_then(Value::as_bool) == Some(true) {
        let raw_reason = value.get("done_reason").and_then(Value::as_str);
        // Ollama emits intermediate `{"done":true,"done_reason":"load"}` and
        // `"unload"` housekeeping frames around model lifecycle events (model
        // mapped into memory, model evicted under `keep_alive: 0`). Those
        // frames are not turn terminals — the actual generation chunks
        // follow. Treat them as no-ops so the stream loop keeps polling for
        // the real terminal frame instead of closing the turn with zero
        // tokens. See `crates/squeezy-llm/src/lib.rs:StopReason::from_ollama`
        // for the normalized mapping of the real terminal reasons (`stop`,
        // `length`).
        if matches!(raw_reason, Some("load") | Some("unload")) {
            return Ok(events);
        }
        let stop_reason = raw_reason
            .map(crate::StopReason::from_ollama)
            .or(Some(crate::StopReason::EndTurn));
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
            stop_reason,
            reasoning_only_stop: false,
        });
    }
    Ok(events)
}

/// Streaming event emitted by [`pull_model`].
///
/// Ollama's `POST /api/pull` returns newline-delimited JSON; each line lands
/// on the stream as one of these variants. The stream terminates either with
/// [`PullEvent::Success`] or by surfacing a transport / parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullEvent {
    /// Human-readable status message (`"pulling manifest"`, `"verifying digest"`).
    Status(String),
    /// Byte-level progress for one layer digest.
    Progress {
        digest: String,
        total: Option<u64>,
        completed: Option<u64>,
    },
    /// Pull finished successfully.
    Success,
}

/// Boxed stream alias matching `LlmStream`'s shape for [`pull_model`].
pub type PullStream = Pin<Box<dyn Stream<Item = Result<PullEvent>> + Send>>;

/// Stream model-pull progress from an Ollama server's `/api/pull` endpoint.
///
/// The returned stream emits one event per NDJSON line. Server-side errors
/// (`{"error": "..."}` payloads) are surfaced as `Err(ProviderStream(_))`
/// and end the stream. Cancelling `cancel` shuts the stream down cleanly.
///
/// `base_url` may be any common shape — `http://host:11434`,
/// `http://host:11434/`, `http://host:11434/api`, or `http://host:11434/v1`.
/// [`api_endpoint_url`] normalizes to the canonical host root before joining
/// `/api/pull`, so users who follow Ollama's upstream `OLLAMA_HOST` env var
/// (no `/api` suffix) reach the right endpoint instead of silently 404-ing.
pub fn pull_model(base_url: &str, model: &str, cancel: CancellationToken) -> PullStream {
    pull_model_with_transport(base_url, model, cancel, ProviderTransportConfig::default())
}

/// Variant of [`pull_model`] that lets callers pin the transport config used
/// for the underlying HTTP client + idle-timeout policy. Constructs the
/// request through the shared connection pool so chat and pull traffic share
/// TCP/TLS sessions and bounds idle waits — a hung Ollama pull aborts with a
/// `ProviderStream` timeout instead of pinning the TUI forever.
///
/// TODO(audit M-19): de-dupe concurrent identical pulls behind a
/// `Mutex<HashMap<String, broadcast::Receiver<PullEvent>>>` so two
/// simultaneous pulls of `qwen3-coder` share one socket and one event
/// stream. The surface is large enough to deserve its own commit alongside
/// the TUI progress hookup; punted until the model picker grows a "pull
/// missing" flow (audit MEDIUM-2).
pub fn pull_model_with_transport(
    base_url: &str,
    model: &str,
    cancel: CancellationToken,
    transport: ProviderTransportConfig,
) -> PullStream {
    let client = shared_client(&transport);
    let url = api_endpoint_url(base_url, "pull");
    let body = json!({ "model": model, "stream": true });
    let idle = idle_timeout(transport);

    Box::pin(try_stream! {
        let response = tokio::select! {
            _ = cancel.cancelled() => {
                return;
            }
            result = client.post(&url).json(&body).send() => result,
        }
        .map_err(|err| SqueezyError::ProviderRequest(format!("ollama pull request failed: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "failed to read error response".to_string());
            Err(SqueezyError::ProviderRequest(format!(
                "ollama pull {status}: {message}"
            )))?;
            unreachable!("provider error returned above");
        }

        let mut decoder = JsonLineDecoder::default();
        let mut bytes = response.bytes_stream();
        loop {
            let polled = tokio::select! {
                _ = cancel.cancelled() => return,
                next = timeout(idle, bytes.next()) => next,
            };
            let next = polled.map_err(|_| {
                SqueezyError::ProviderStream("Ollama pull stream idle timeout".to_string())
            })?;
            let Some(chunk) = next else { break; };
            let chunk = chunk.map_err(|err| SqueezyError::ProviderStream(err.to_string()))?;
            for line in decoder.push(&chunk) {
                match parse_pull_line(&line)? {
                    Some(event @ PullEvent::Success) => {
                        yield event;
                        return;
                    }
                    Some(event) => yield event,
                    None => {}
                }
            }
        }
        for line in decoder.finish() {
            match parse_pull_line(&line)? {
                Some(event @ PullEvent::Success) => {
                    yield event;
                    return;
                }
                Some(event) => yield event,
                None => {}
            }
        }
    })
}

/// Parse one NDJSON line emitted by Ollama's `/api/pull` endpoint. Returns
/// `Ok(None)` for lines that carry neither a recognisable status nor
/// progress field (Ollama occasionally emits empty `{}` keep-alive frames).
pub(crate) fn parse_pull_line(line: &str) -> Result<Option<PullEvent>> {
    let value: Value = serde_json::from_str(line)
        .map_err(|err| SqueezyError::ProviderStream(format!("invalid Ollama pull JSON: {err}")))?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(SqueezyError::ProviderStream(format!(
            "ollama pull error: {error}"
        )));
    }
    // The server returns `status: "success"` on the final line. Treat it as
    // the success terminator regardless of any other fields present.
    if value.get("status").and_then(Value::as_str) == Some("success") {
        return Ok(Some(PullEvent::Success));
    }
    // A line carrying a digest is a per-layer download progress update,
    // even when `total`/`completed` are missing on intermediate frames.
    if let Some(digest) = value.get("digest").and_then(Value::as_str) {
        let total = value.get("total").and_then(Value::as_u64);
        let completed = value.get("completed").and_then(Value::as_u64);
        return Ok(Some(PullEvent::Progress {
            digest: digest.to_string(),
            total,
            completed,
        }));
    }
    if let Some(status) = value.get("status").and_then(Value::as_str) {
        return Ok(Some(PullEvent::Status(status.to_string())));
    }
    Ok(None)
}

#[cfg(test)]
#[path = "ollama_tests.rs"]
mod tests;
