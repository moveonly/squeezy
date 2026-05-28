use std::collections::BTreeMap;
use std::sync::Arc;

use async_stream::try_stream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use squeezy_core::{AnthropicConfig, CostSnapshot, ProviderTransportConfig, Result, SqueezyError};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    AnthropicThinkingBlock, AnthropicThinkingKind, LlmEvent, LlmInputItem, LlmProvider, LlmRequest,
    LlmStream, LlmToolCall, ReasoningKind, ReasoningPayload,
    anthropic_betas::anthropic_header_value,
    cache_policy::{CachePolicy, CacheRetention, json_markers, should_apply_caching},
    credentials::{ApiKeySource, resolve_api_key_with_inline, static_api_key_source},
    oauth::{anthropic_oauth_beta_header, is_anthropic_oauth_token},
    overflow::{OverflowSignal, Usage as OverflowUsage, classify_terminal},
    retry::{RetryPolicy, idle_timeout, send_with_auth_retry, with_stream_retry},
    sse::SseDecoder,
    transport::shared_client,
};

const ANTHROPIC_PROVIDER_NAME: &str = "anthropic";

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_ANTHROPIC_MAX_OUTPUT_TOKENS: u64 = 64_000;

/// Identity preamble Anthropic requires on OAuth-driven requests so
/// the call counts against the Claude Pro/Max subscription quota
/// rather than failing the OAuth quota check. Mirrors pi's verbatim
/// string. The user's real instructions ride after this in a second
/// system block.
const OAUTH_SYSTEM_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// `User-Agent` value that marks a Squeezy-issued OAuth request as
/// Claude-Code-compatible. Anthropic gates the OAuth quota on this
/// identity envelope (beta header + UA + `x-app`); changing the
/// values risks the platform rejecting the request.
const OAUTH_USER_AGENT: &str = "claude-cli/2.1.0";

#[derive(Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: Arc<dyn ApiKeySource>,
    base_url: String,
    transport: ProviderTransportConfig,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("client", &self.client)
            .field("api_key", &self.api_key)
            .field("base_url", &self.base_url)
            .field("transport", &self.transport)
            .finish()
    }
}

impl AnthropicProvider {
    pub fn from_config(config: &AnthropicConfig) -> Result<Self> {
        let api_key =
            resolve_api_key_with_inline(config.api_key.as_deref(), &config.api_key_env)?.value;
        Ok(Self::with_api_key_source(
            static_api_key_source(api_key, "anthropic"),
            config.base_url.trim_end_matches('/').to_string(),
            config.transport,
        ))
    }

    /// Construct the provider against an already-built credential
    /// source. Used by the OAuth subscription providers (Claude
    /// Pro/Max) so a rotating access token can flow through the same
    /// HTTP path without rebuilding the client on every refresh.
    pub fn with_api_key_source(
        api_key: Arc<dyn ApiKeySource>,
        base_url: String,
        transport: ProviderTransportConfig,
    ) -> Self {
        Self {
            client: shared_client(&transport),
            api_key,
            base_url,
            transport,
        }
    }

    /// Build the `/messages` JSON body. Parameterized on the auth
    /// scheme so the OAuth (Claude Pro/Max) path can prepend the
    /// Claude Code identity preamble to `system`: Anthropic gates
    /// the OAuth quota on this envelope; the API-key path doesn't
    /// need it.
    pub(crate) fn request_body(request: &LlmRequest, auth: AnthropicAuthScheme) -> Value {
        let policy = CachePolicy::AUTO;
        let prompt_caching = should_apply_caching("anthropic", request);
        let retention = if prompt_caching {
            request.effective_cache_spec().retention
        } else {
            CacheRetention::None
        };
        let max_tokens = request
            .max_output_tokens
            .map(u64::from)
            .or_else(|| {
                crate::model_info_for("anthropic", &request.model)
                    .and_then(|info| info.limits)
                    .map(|limits| limits.max_output_tokens)
            })
            .unwrap_or(DEFAULT_ANTHROPIC_MAX_OUTPUT_TOKENS);
        // Canonicalize cross-provider tool-call ids and synthesize
        // placeholders for orphan tool results BEFORE the
        // Anthropic-specific message rewrite. Anthropic rejects raw
        // OpenAI Responses `fc_…|…` ids (regex + length cap) and
        // rejects `tool_result` blocks whose `tool_use_id` has no
        // matching `tool_use` earlier in the same conversation; both
        // failure modes are common after a mid-session
        // `anthropic/claude-X → openai/gpt-Y → anthropic/...` swap.
        let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
        let mut body = json!({
            "model": request.model,
            "system": anthropic_system(
                &request.instructions,
                prompt_caching && policy.system,
                auth,
                retention,
            ),
            "messages": anthropic_messages(&normalized_input, prompt_caching, policy, retention),
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
            if prompt_caching && policy.tools {
                json_markers::mark_last_stable_tool(&mut tool_values, retention);
            }
            body["tools"] = Value::Array(tool_values);
        }
        body
    }
}

/// Identifies whether the next HTTP attempt will authenticate with a
/// raw API key (`x-api-key`) or an OAuth bearer token. Used to drive
/// the OAuth identity envelope — Anthropic gates the Claude Pro/Max
/// quota on a Claude-Code-shaped request, so OAuth-driven requests
/// have to prepend a fixed system preamble, set
/// `Authorization: Bearer`, and stamp the Claude Code beta header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnthropicAuthScheme {
    ApiKey,
    Oauth,
}

impl AnthropicAuthScheme {
    fn for_key(key: &str) -> Self {
        if is_anthropic_oauth_token(key) {
            Self::Oauth
        } else {
            Self::ApiKey
        }
    }
}

/// Combine the caller's `anthropic-beta` header value with the OAuth
/// beta opt-in. The OAuth marker has to be present on every Claude
/// Pro/Max request or Anthropic will route the call to the API-key
/// quota (or reject it). Returns `None` when neither side has any
/// value so the caller can skip the header entirely.
fn merge_oauth_beta_header(caller: Option<&str>, auth: AnthropicAuthScheme) -> Option<String> {
    match auth {
        AnthropicAuthScheme::ApiKey => caller.map(str::to_string),
        AnthropicAuthScheme::Oauth => {
            let oauth = anthropic_oauth_beta_header();
            let merged = match caller {
                Some(value) if !value.trim().is_empty() => {
                    let mut seen: Vec<&str> = Vec::new();
                    for token in oauth.split(',').chain(value.split(',')) {
                        let trimmed = token.trim();
                        if trimmed.is_empty() || seen.contains(&trimmed) {
                            continue;
                        }
                        seen.push(trimmed);
                    }
                    seen.join(",")
                }
                _ => oauth.to_string(),
            };
            Some(merged)
        }
    }
}

fn anthropic_system(
    instructions: &str,
    prompt_caching: bool,
    auth: AnthropicAuthScheme,
    retention: CacheRetention,
) -> Value {
    let identity_first = matches!(auth, AnthropicAuthScheme::Oauth);
    if !prompt_caching {
        if identity_first {
            return Value::Array(vec![
                json!({
                    "type": "text",
                    "text": OAUTH_SYSTEM_IDENTITY,
                }),
                json!({
                    "type": "text",
                    "text": instructions,
                }),
            ]);
        }
        return json!(instructions);
    }
    if identity_first {
        let mut blocks = vec![json!({
            "type": "text",
            "text": OAUTH_SYSTEM_IDENTITY,
        })];
        let with_user = json_markers::system_array_with_marker(instructions, retention);
        if let Value::Array(items) = with_user {
            blocks.extend(items);
        } else {
            blocks.push(with_user);
        }
        return Value::Array(blocks);
    }
    json_markers::system_array_with_marker(instructions, retention)
}

fn anthropic_messages(
    input: &[LlmInputItem],
    prompt_caching: bool,
    policy: CachePolicy,
    retention: CacheRetention,
) -> Value {
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
            LlmInputItem::Image { media_type, bytes } => push_anthropic_message(
                &mut messages,
                "user",
                vec![json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": BASE64_STANDARD.encode(bytes.as_ref()),
                    },
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
        match policy.messages {
            crate::cache_policy::MessageStrategy::LatestUserMessage => {
                json_markers::mark_last_user_block(&mut messages, retention);
            }
        }
    }
    Value::Array(messages)
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
        if let Err(err) = request.ensure_vision_support("anthropic") {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let url = format!("{}/messages", self.base_url);
        let caller_beta_header = anthropic_header_value(&request.beta_headers);
        let transport = self.transport;
        let request_for_attempts = request.clone();

        let attempt_cancel = cancel.clone();
        let make_attempt = move || -> LlmStream {
            anthropic_stream_attempt(
                client.clone(),
                api_key.clone(),
                url.clone(),
                request_for_attempts.clone(),
                caller_beta_header.clone(),
                transport,
                attempt_cancel.clone(),
            )
        };

        with_stream_retry(
            "anthropic",
            RetryPolicy::provider_stream(transport),
            cancel,
            make_attempt,
        )
    }
}

fn anthropic_stream_attempt(
    client: reqwest::Client,
    api_key: Arc<dyn ApiKeySource>,
    url: String,
    request: LlmRequest,
    caller_beta_header: Option<String>,
    transport: ProviderTransportConfig,
    cancel: CancellationToken,
) -> LlmStream {
    Box::pin(try_stream! {
        let response = send_with_auth_retry(
            &api_key,
            RetryPolicy::provider_requests(transport),
            &cancel,
            |key| {
                let auth = AnthropicAuthScheme::for_key(key);
                let body = AnthropicProvider::request_body(&request, auth);
                let beta_header = merge_oauth_beta_header(caller_beta_header.as_deref(), auth);
                let mut builder = client
                    .post(&url)
                    .header("anthropic-version", ANTHROPIC_VERSION);
                builder = match auth {
                    AnthropicAuthScheme::Oauth => builder
                        .header("authorization", format!("Bearer {key}"))
                        .header("user-agent", OAUTH_USER_AGENT)
                        .header("x-app", "cli"),
                    AnthropicAuthScheme::ApiKey => builder.header("x-api-key", key),
                };
                if let Some(value) = beta_header.as_deref() {
                    builder = builder.header("anthropic-beta", value);
                }
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
            let formatted = format!("{status}: {message}");
            // Pre-stream HTTP error path. Anthropic surfaces overflow as a
            // 400 with `prompt is too long: …` in the body; emit the
            // classifier signal additively before propagating the error
            // so the agent can react instead of looping into the same call.
            if let Some(signal) = classify_terminal(
                ANTHROPIC_PROVIDER_NAME,
                None,
                Some(&formatted),
                None,
                true,
            ) {
                yield LlmEvent::ContextOverflow {
                    provider: ANTHROPIC_PROVIDER_NAME.to_string(),
                    signal,
                };
            }
            Err(SqueezyError::ProviderRequest(formatted))?;
            unreachable!("provider error returned above");
        };

        yield LlmEvent::Started;

        let mut decoder = SseDecoder::default();
        let mut state = AnthropicStreamState::default();
        let mut saw_completed = false;
        let mut saw_visible_output = false;
        // Resolved context window for the SilentUsage path. `None` when the
        // model is unknown to the local registry (e.g. an aggregator alias);
        // the classifier just skips the usage path in that case.
        let max_window = crate::model_info_for(ANTHROPIC_PROVIDER_NAME, &request.model)
            .and_then(|info| info.limits)
            .map(|limits| limits.context_window_tokens);
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
                    match &llm_event {
                        LlmEvent::TextDelta(text) if !text.is_empty() => {
                            saw_visible_output = true;
                        }
                        LlmEvent::ToolCall(_) => {
                            saw_visible_output = true;
                        }
                        LlmEvent::Completed { .. } => {
                            saw_completed = true;
                            if let Some(event) =
                                overflow_event_for_completed(&state, max_window, saw_visible_output)
                            {
                                yield event;
                            }
                        }
                        _ => {}
                    }
                    yield llm_event;
                }
            }
        }

        for event in decoder.finish() {
            for llm_event in parse_anthropic_event(&event, &mut state)? {
                match &llm_event {
                    LlmEvent::TextDelta(text) if !text.is_empty() => {
                        saw_visible_output = true;
                    }
                    LlmEvent::ToolCall(_) => {
                        saw_visible_output = true;
                    }
                    LlmEvent::Completed { .. } => {
                        saw_completed = true;
                        if let Some(event) =
                            overflow_event_for_completed(&state, max_window, saw_visible_output)
                        {
                            yield event;
                        }
                    }
                    _ => {}
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

/// Run the triple-path overflow classifier against the Anthropic
/// stream state at `message_stop`. Returns an additive
/// [`LlmEvent::ContextOverflow`] when any path fires; the caller
/// yields it immediately before the canonical [`LlmEvent::Completed`].
///
/// `used` totals reported `input_tokens + output_tokens` so a turn
/// that fills the prompt budget *or* spends the budget on output
/// can both surface as `SilentUsage` when the result equals or
/// exceeds the model's context window.
fn overflow_event_for_completed(
    state: &AnthropicStreamState,
    max_window: Option<u64>,
    saw_visible_output: bool,
) -> Option<LlmEvent> {
    let used = state
        .input_tokens
        .unwrap_or(0)
        .saturating_add(state.output_tokens.unwrap_or(0));
    let usage = max_window.map(|max| OverflowUsage { used, max });
    let signal: OverflowSignal = classify_terminal(
        ANTHROPIC_PROVIDER_NAME,
        state.stop_reason.as_deref(),
        None,
        usage.as_ref(),
        !saw_visible_output,
    )?;
    Some(LlmEvent::ContextOverflow {
        provider: ANTHROPIC_PROVIDER_NAME.to_string(),
        signal,
    })
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
            // Surface `stop_reason` to the agent instead of converting
            // `max_tokens` into a transport error here. The agent's turn
            // loop branches on the normalized `StopReason` so all providers
            // share one recovery path (max-tokens truncation, refusal,
            // empty end_turn, etc.) rather than each provider failing in
            // its own dialect.
            let stop_reason = state
                .stop_reason
                .as_deref()
                .map(crate::StopReason::from_anthropic);
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
                stop_reason,
                reasoning_only_stop: false,
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
