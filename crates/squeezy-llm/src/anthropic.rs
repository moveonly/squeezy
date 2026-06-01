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

/// Anthropic's hard floor for `thinking.budget_tokens` — the API rejects
/// any request below this with `invalid_request_error`.
const ANTHROPIC_MIN_THINKING_BUDGET_TOKENS: u64 = 1024;

/// Tokens we reserve for the assistant's reply on top of the thinking
/// budget. Anthropic requires `max_tokens > budget_tokens`; a 1024-token
/// reply headroom keeps the assistant from being truncated immediately
/// after thinking completes while still leaving thinking the bulk of
/// the budget.
const ANTHROPIC_THINKING_REPLY_HEADROOM_TOKENS: u64 = 1024;

/// Beta header required for `output_config.effort`, used alongside
/// `thinking.type=adaptive` on Claude 4.6+ models. The API rejects
/// `thinking.type=enabled` for those models and directs callers here.
const EFFORT_BETA_HEADER: &str = "effort-2025-11-24";

/// Anthropic Opus and Sonnet from 4.6 onward are trained on adaptive
/// thinking and reject `thinking.type=enabled`; budget is controlled via
/// `output_config.effort` instead. Version is parsed from the model id
/// (e.g. `claude-opus-4-7` → `(4, 7)`) so newer releases like opus-4-8
/// or sonnet-5-0 pick up adaptive without a code change. Haiku and any
/// pre-4.6 model fall back to the explicit-budget form.
pub(crate) fn model_uses_adaptive_thinking(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    ["opus", "sonnet"]
        .iter()
        .any(|family| extract_claude_version(&lower, family).is_some_and(|v| v >= (4, 6)))
}

fn extract_claude_version(model: &str, family: &str) -> Option<(u32, u32)> {
    let needle = format!("{family}-");
    let start = model.find(&needle)? + needle.len();
    let mut parts = model[start..].split('-');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn anthropic_effort_label(effort: squeezy_core::ReasoningEffort) -> &'static str {
    match effort {
        squeezy_core::ReasoningEffort::Low => "low",
        squeezy_core::ReasoningEffort::Medium => "medium",
        squeezy_core::ReasoningEffort::High => "high",
        squeezy_core::ReasoningEffort::XHigh => "max",
    }
}

/// Identity preamble Anthropic requires on OAuth-driven requests so
/// the call counts against the Claude Pro/Max subscription quota
/// rather than failing the OAuth quota check. Anthropic pins the
/// exact string. The user's real instructions ride after this in a
/// second system block.
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
        // 4-slot budget enforces Anthropic's hard 4-breakpoint
        // cache_control cap. Invalidation order (most-volatile last):
        // tools change the least frequently, system next, messages
        // most often — so we allocate tools first, then system,
        // then messages. When a future caller-supplied marker (skill
        // layer, multi-system blocks) pushes the count past 4 we
        // drop the most-volatile slot rather than 400 the request.
        let mut budget = BreakpointBudget::new();
        let tools_marker =
            prompt_caching && policy.tools && !request.tools.is_empty() && budget.consume("tools");
        let system_marker = prompt_caching && policy.system && budget.consume("system");
        let messages_marker = prompt_caching && budget.consume("messages");
        let mut body = json!({
            "model": request.model,
            "system": anthropic_system(
                &request.instructions,
                system_marker,
                auth,
                retention,
            ),
            "messages": anthropic_messages(&normalized_input, messages_marker, policy, retention),
            "max_tokens": max_tokens,
            "stream": true,
        });
        if let Some(effort) = request.reasoning_effort
            && crate::capabilities_for("anthropic", &request.model)
                .is_some_and(|caps| caps.reasoning_effort)
        {
            if model_uses_adaptive_thinking(&request.model) {
                // Claude 4.6+ opus/sonnet reject `thinking.type=enabled`
                // and want the budget conveyed through
                // `output_config.effort` instead.
                body["thinking"] = json!({ "type": "adaptive" });
                body["output_config"] = json!({
                    "effort": anthropic_effort_label(effort),
                });
            } else {
                // Anthropic requires `budget_tokens >= 1024` AND
                // `max_tokens > budget_tokens`. When `max_output_tokens` is
                // too small to satisfy both at once, emitting `thinking`
                // earns a hard 400 on every turn. Skip the block in that
                // case and warn so the operator can either raise
                // `max_output_tokens` or unset `reasoning_effort`.
                let ceiling = max_tokens.saturating_sub(ANTHROPIC_THINKING_REPLY_HEADROOM_TOKENS);
                if ceiling >= ANTHROPIC_MIN_THINKING_BUDGET_TOKENS {
                    let budget = u64::from(effort.thinking_budget_tokens())
                        .min(ceiling)
                        .max(ANTHROPIC_MIN_THINKING_BUDGET_TOKENS);
                    body["thinking"] = json!({
                        "type": "enabled",
                        "budget_tokens": budget,
                    });
                } else {
                    tracing::warn!(
                        provider = "anthropic",
                        model = %request.model,
                        max_output_tokens = max_tokens,
                        min_required = ANTHROPIC_MIN_THINKING_BUDGET_TOKENS
                            + ANTHROPIC_THINKING_REPLY_HEADROOM_TOKENS,
                        "anthropic thinking disabled: max_output_tokens too small to satisfy \
                         thinking.budget_tokens >= 1024 with a reply headroom; raise \
                         max_output_tokens or clear reasoning_effort to silence this warning"
                    );
                }
            }
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
            if tools_marker {
                json_markers::mark_last_stable_tool(&mut tool_values, retention);
            }
            body["tools"] = Value::Array(tool_values);
        }
        body
    }
}

/// Anthropic accepts at most four `cache_control` breakpoints per
/// request across `tools`, `system`, and `messages`. Beyond the cap
/// the Messages API returns a 400 with
/// `invalid_request_error: cache_control breakpoint limit exceeded`.
///
/// `BreakpointBudget` mirrors opencode's `Cache.Breakpoints` slot
/// allocator (`packages/llm/src/protocols/anthropic-messages.ts:239-247`)
/// so the lowering layer can sit in front of every helper that emits
/// a marker, decrement on consumption, and drop-and-warn when the
/// budget is exhausted. Today the auto policy only ever consumes 3
/// slots (tools / system / messages), but the cap is enforced
/// defensively so any future caller-supplied marker (skill-loaded
/// tool def, multi-system blocks) skips silently instead of
/// 4xx-ing the request.
struct BreakpointBudget {
    remaining: u32,
    dropped: u32,
}

impl BreakpointBudget {
    /// Anthropic's hard cap on `cache_control` breakpoints per
    /// request. Documented at
    /// <https://platform.claude.com/docs/en/docs/build-with-claude/prompt-caching>.
    const CAP: u32 = 4;

    fn new() -> Self {
        Self {
            remaining: Self::CAP,
            dropped: 0,
        }
    }

    /// Try to consume one slot for the named section. Returns `true`
    /// when the marker should be emitted, `false` when the budget is
    /// exhausted (and warns once per dropped marker so operators can
    /// see when their caller layout outgrew the cap).
    fn consume(&mut self, section: &'static str) -> bool {
        if self.remaining == 0 {
            self.dropped = self.dropped.saturating_add(1);
            tracing::warn!(
                provider = "anthropic",
                section,
                cap = Self::CAP,
                dropped = self.dropped,
                "anthropic cache_control breakpoint dropped: per-request cap exceeded",
            );
            return false;
        }
        self.remaining -= 1;
        true
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
            LlmInputItem::FunctionCallOutput {
                call_id, output, ..
            } => push_anthropic_message(
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
                            // Anthropic streams the encrypted blob over
                            // `signature_delta` for redacted blocks, but
                            // the `content_block_start` frame can also
                            // populate `data` directly. Round-trip
                            // whichever field actually got populated so a
                            // multi-turn continuation never ships an
                            // empty `data: ""` payload (which Anthropic
                            // rejects with `invalid_request_error` or
                            // silently breaks reasoning continuity).
                            let data = block
                                .data
                                .clone()
                                .or_else(|| block.signature.clone())
                                .unwrap_or_default();
                            json!({
                                "type": "redacted_thinking",
                                "data": data,
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
            // Document attachments lower via Anthropic's `document`
            // content block; per-provider implementation lands in
            // Phase 4. Until then we skip with a debug log so the
            // request still ships instead of erroring at the wire.
            LlmInputItem::Document { name, .. } => {
                tracing::debug!(
                    target: "squeezy_llm::anthropic",
                    name = name.as_str(),
                    "anthropic document content block not yet implemented; skipping",
                );
            }
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
        let needs_effort_beta = request.reasoning_effort.is_some()
            && model_uses_adaptive_thinking(&request.model)
            && crate::capabilities_for("anthropic", &request.model)
                .is_some_and(|caps| caps.reasoning_effort);
        let mut effective_betas: Vec<Arc<str>> = request.beta_headers.iter().cloned().collect();
        if needs_effort_beta
            && !effective_betas
                .iter()
                .any(|beta| beta.as_ref() == EFFORT_BETA_HEADER)
        {
            effective_betas.push(Arc::<str>::from(EFFORT_BETA_HEADER));
        }
        let caller_beta_header = anthropic_header_value(&effective_betas);
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
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "failed to read error response".to_string());
            // Pre-stream HTTP error path. Anthropic surfaces overflow as a
            // 400 with `prompt is too long: …` in the body; emit the
            // classifier signal additively before propagating the error
            // so the agent can react instead of looping into the same call.
            // The overflow classifier still inspects the raw body — its
            // pattern set keys on the verbatim provider prose, not on the
            // humanised TUI line.
            let raw_for_classifier = format!("{status}: {body}");
            if let Some(signal) = classify_terminal(
                ANTHROPIC_PROVIDER_NAME,
                None,
                Some(&raw_for_classifier),
                None,
                true,
            ) {
                yield LlmEvent::ContextOverflow {
                    provider: ANTHROPIC_PROVIDER_NAME.to_string(),
                    signal,
                };
            }
            // Humanise the JSON envelope before propagating: the status
            // line and turn-failed banner used to print the raw payload
            // and a bogus "retry" hint on 400s. The normaliser extracts
            // `error.message` + `request_id`, encodes a retry verdict
            // via [`NON_RETRYABLE_MARKER`], and falls back to the raw
            // shape when the body is not a recognisable envelope.
            let formatted = crate::anthropic_error::format_for_provider_error(status, &body);
            Err(SqueezyError::ProviderRequest(formatted))?;
            unreachable!("provider error returned above");
        };

        yield LlmEvent::Started;

        let mut decoder = SseDecoder::default();
        let mut state = AnthropicStreamState::default();
        let mut server_model_echo = crate::ServerModelEcho::default();
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
                let parsed = parse_anthropic_event(&event, &mut state);
                // Mid-stream `event: error` may have stashed an
                // overflow signal on state; yield the additive
                // `ContextOverflow` event before propagating the
                // terminal error so the agent sees the same shape
                // as the pre-200 HTTP-error path.
                if let Some(signal) = state.pending_overflow_signal.take() {
                    yield LlmEvent::ContextOverflow {
                        provider: ANTHROPIC_PROVIDER_NAME.to_string(),
                        signal,
                    };
                }
                let parsed = parsed?;
                // `message_start` populates `state.server_model` but
                // yields no `LlmEvent`s, so drain the field at the
                // frame boundary rather than per-event to make sure
                // `ServerModel` lands even on the first frame.
                if let Some(server) = state.server_model.take()
                    && let Some(echo) = server_model_echo.observe(&request.model, &server)
                {
                    yield echo;
                }
                for llm_event in parsed {
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
            let parsed = parse_anthropic_event(&event, &mut state);
            if let Some(signal) = state.pending_overflow_signal.take() {
                yield LlmEvent::ContextOverflow {
                    provider: ANTHROPIC_PROVIDER_NAME.to_string(),
                    signal,
                };
            }
            let parsed = parsed?;
            if let Some(server) = state.server_model.take()
                && let Some(echo) = server_model_echo.observe(&request.model, &server)
            {
                yield echo;
            }
            for llm_event in parsed {
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
/// `used` totals the normalised `input_tokens + output_tokens` from
/// the snapshot's `cost()` view (i.e. the full prompt the model saw,
/// cached + uncached + cache-write, plus output) so a turn that fills
/// the prompt budget *or* spends the budget on output can both
/// surface as `SilentUsage` when the result equals or exceeds the
/// model's context window.
fn overflow_event_for_completed(
    state: &AnthropicStreamState,
    max_window: Option<u64>,
    saw_visible_output: bool,
) -> Option<LlmEvent> {
    // Use the normalised cost view so the "used" total reflects the
    // full prompt the model saw (including cached and cache-write
    // tokens) rather than the small uncached delta. Reading
    // `state.input_tokens` directly here would silently under-fire the
    // SilentUsage classifier on cache-hit turns.
    let cost = state.cost();
    let used = cost
        .input_tokens
        .unwrap_or(0)
        .saturating_add(cost.output_tokens.unwrap_or(0));
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
    /// Server-echoed model id captured from `message_start.message.model`
    /// the first time the field is seen. Drained by the outer attempt
    /// loop to drive [`crate::ServerModelEcho`] before any other event
    /// is yielded downstream. `None` until Anthropic announces the
    /// chosen model (the docs guarantee `message_start` is the first
    /// SSE frame of every successful turn) and once the outer loop
    /// has drained it, it stays `None` for the rest of the stream.
    server_model: Option<String>,
    /// Overflow signal captured by the mid-stream `event: error`
    /// handler. The outer attempt loop drains this between
    /// `parse_anthropic_event` and the `?` propagation so a
    /// `model_context_window_exceeded` mid-stream surfaces a
    /// `ContextOverflow` event additively before the terminal error,
    /// matching the pre-200 HTTP-error path. `None` for the healthy
    /// case and for stream errors that don't fit any overflow
    /// pattern. See `parse_anthropic_event`'s `"error"` branch.
    pending_overflow_signal: Option<OverflowSignal>,
}

impl AnthropicStreamState {
    fn cost(&self) -> CostSnapshot {
        // Normalise to the cross-provider convention used in
        // `CostSnapshot`: `input_tokens` is the **total** prompt the
        // model saw (uncached + cache read + cache write), and the
        // breakdown lives in `cached_input_tokens` /
        // `cache_write_input_tokens`. Anthropic's Messages API ships
        // `usage.input_tokens` as the uncached delta only, so we fold
        // the cache counters back in here. Without this, a reader of
        // `frames.jsonl` sees a tiny `input_tokens` value on a cache-hit
        // turn and is misled into thinking the prompt was short.
        let base = self.input_tokens;
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_write = self.cache_creation_input_tokens.unwrap_or(0);
        let total_input = base.map(|b| b.saturating_add(cache_read).saturating_add(cache_write));
        CostSnapshot {
            input_tokens: total_input,
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
                // Capture the server-chosen model id so the outer
                // attempt loop can compare it against `request.model`
                // and emit `LlmEvent::ServerModel` when Anthropic
                // silently substitutes (regional fallback, alias
                // resolution, etc.). The parser writes the field
                // here; the outer loop drains it before any other
                // event is yielded downstream.
                state.server_model = message
                    .get("model")
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
                        match block.kind {
                            // Visible thinking blocks carry a separate
                            // `signature` field that we round-trip
                            // alongside the cleartext.
                            AnthropicThinkingKind::Thinking => match block.signature.as_mut() {
                                Some(existing) => existing.push_str(&signature),
                                None => block.signature = Some(signature),
                            },
                            // Redacted thinking blocks carry their
                            // encrypted payload over the `signature_delta`
                            // wire frame (Anthropic uses the same frame
                            // for both shapes — for `redacted_thinking`
                            // there is no cleartext to ship and the
                            // `data` field on the start frame may be
                            // empty until the deltas land). Accumulate
                            // into `block.data` so the replay path can
                            // emit the full encrypted blob; without
                            // this, the multi-turn round-trip ships
                            // `"data": ""` and Anthropic 4xx-s the
                            // continuation or silently breaks reasoning
                            // continuity. See `.audit/providers/anthropic.md`
                            // HIGH #2 and
                            // <https://platform.claude.com/docs/en/build-with-claude/extended-thinking#multi-turn-conversations-with-thinking>.
                            AnthropicThinkingKind::Redacted => match block.data.as_mut() {
                                Some(existing) => existing.push_str(&signature),
                                None => block.data = Some(signature),
                            },
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
            // Mid-stream `event: error` after a 200 OK can carry any
            // shape Anthropic might surface — `overloaded_error`,
            // `rate_limit_error`, `model_context_window_exceeded`,
            // `invalid_request_error`, or `api_error`. The pre-200
            // path runs `classify_terminal` and emits a
            // `ContextOverflow` signal; the post-200 path used to
            // wrap every variant in `ProviderStream`, so the retry
            // layer would happily reconnect 5x against an immutable
            // failure (`model_context_window_exceeded`) or pile on
            // load (`overloaded_error`). We mirror the pre-200
            // categorization here: classify the (type, message)
            // pair through `classify_terminal`, push the resulting
            // overflow signal onto `state.pending_overflow_signal`,
            // and route the resulting `SqueezyError` through
            // `ProviderRequest` (with the non-retryable marker for
            // hard-config errors) instead of `ProviderStream` so
            // the outer attempt loop can yield the overflow event
            // before propagating the error.
            let error_obj = value.get("error");
            let error_type = error_obj
                .and_then(|error| error.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string();
            let message = error_obj
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Anthropic stream error")
                .to_string();
            let raw_for_classifier = format!("{error_type}: {message}");
            state.pending_overflow_signal = classify_terminal(
                ANTHROPIC_PROVIDER_NAME,
                None,
                Some(&raw_for_classifier),
                None,
                true,
            );
            let human = format!("Anthropic stream error ({error_type}): {message}");
            let err = match error_type.as_str() {
                // Retryable transient errors: keep the retry path
                // open but route through `ProviderRequest` so the
                // pre/post-200 paths surface in the same shape.
                "overloaded_error" | "rate_limit_error" | "api_error" => {
                    SqueezyError::ProviderRequest(human)
                }
                // Hard-config errors and overflow: stamp the
                // non-retryable marker so the TUI can suppress the
                // retry suffix; the marker is also what future
                // retry-layer work consumes to short-circuit the
                // reconnect loop.
                "invalid_request_error"
                | "model_context_window_exceeded"
                | "not_found_error"
                | "authentication_error"
                | "permission_error" => SqueezyError::ProviderRequest(format!(
                    "{}{human}",
                    crate::anthropic_error::NON_RETRYABLE_MARKER
                )),
                // Unknown error class: surface as a stream error so
                // the retry layer still attempts a reconnect (which
                // is what we want for genuinely transient transport
                // shapes), but with the typed message attached.
                _ => SqueezyError::ProviderStream(human),
            };
            Err(err)
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
