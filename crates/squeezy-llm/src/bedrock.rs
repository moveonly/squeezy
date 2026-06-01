use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use async_stream::try_stream;
use aws_config::{BehaviorVersion, SdkConfig};
use aws_sdk_bedrockruntime::types::ConverseStreamOutput;
use aws_sdk_bedrockruntime::{
    Client as BedrockClient,
    config::{Region, Token as BedrockToken},
    error::SdkError,
    primitives::event_stream::EventReceiver,
    types::{
        CachePointBlock, CachePointType, CacheTtl, ContentBlock, ContentBlockDelta,
        ContentBlockStart, ConversationRole, DocumentBlock, DocumentFormat, DocumentSource,
        ImageBlock, ImageFormat, ImageSource, InferenceConfiguration, Message,
        ReasoningContentBlock, ReasoningContentBlockDelta, ReasoningTextBlock, SystemContentBlock,
        Tool, ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock,
        ToolSpecification, ToolUseBlock,
    },
};
use aws_smithy_types::{Blob, Document, Number};
use serde_json::Value;
use squeezy_core::{BedrockConfig, CostSnapshot, ProviderTransportConfig, Result, SqueezyError};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    AnthropicThinkingBlock, AnthropicThinkingKind, CacheRetention, LlmEvent, LlmInputItem,
    LlmProvider, LlmRequest, LlmStream, LlmToolCall, LlmToolSpec, ReasoningKind, ReasoningPayload,
    anthropic_betas::bedrock_extra_body_betas,
    cache_policy::{DYNAMIC_TOOL_NAME_PREFIX, should_apply_caching},
    retry::{RetryPolicy, idle_timeout, with_stream_retry},
};

/// Bedrock's per-image byte limit for Claude on Converse — 3.75 MiB.
/// AWS docs cap Claude images at 3.75 MB; the SDK surfaces the
/// constraint as `ValidationException` with an opaque message, so we
/// guard up-front with a structured error pointing operators at the
/// offending image. Nova models lift this to ~20 MB but squeezy
/// currently routes all Bedrock turns through the Claude allow-list so
/// the lower limit is the safe default.
const BEDROCK_IMAGE_MAX_BYTES: usize = 3_932_160; // 3.75 * 1024 * 1024

/// Anthropic's hard floor for `thinking.budget_tokens` on Bedrock —
/// the API rejects any request below this with
/// `invalid_request_error`. Matches the Anthropic-native floor
/// (`anthropic.rs:34`).
const BEDROCK_MIN_THINKING_BUDGET_TOKENS: u64 = 1024;

/// Tokens reserved for the assistant's reply on top of the thinking
/// budget. Anthropic-on-Bedrock requires `max_tokens > budget_tokens`;
/// a 1024-token reply headroom keeps the assistant from being
/// truncated immediately after thinking completes while still leaving
/// thinking the bulk of the budget. Matches `anthropic.rs:41`.
const BEDROCK_THINKING_REPLY_HEADROOM_TOKENS: u64 = 1024;

/// Fallback `max_tokens` when the request leaves it unset AND the
/// model registry does not carry an explicit `max_output_tokens`
/// limit. Mirrors the Anthropic-native default
/// (`anthropic.rs:30`).
const BEDROCK_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 64_000;

#[derive(Debug, Clone)]
pub struct BedrockProvider {
    region: String,
    base_url: Option<String>,
    bearer_token: Option<String>,
    /// Operator-defined cost-allocation tags forwarded on every
    /// ConverseStream invocation (F16pi-bedrock-request-metadata-tags).
    request_metadata: std::collections::BTreeMap<String, String>,
    transport: ProviderTransportConfig,
    shared: Arc<tokio::sync::OnceCell<SdkConfig>>,
}

impl BedrockProvider {
    pub fn from_config(config: &BedrockConfig) -> Result<Self> {
        Ok(Self {
            region: config.region.clone(),
            base_url: config.base_url.clone(),
            bearer_token: config.bearer_token.clone(),
            request_metadata: config.request_metadata.clone(),
            transport: config.transport,
            shared: Arc::new(tokio::sync::OnceCell::new()),
        })
    }

    async fn client(&self) -> Result<BedrockClient> {
        let region = self.region.clone();
        let base_url = self.base_url.clone();
        let shared = self
            .shared
            .get_or_init(|| async move { load_aws_config(region, base_url).await })
            .await;
        // Bedrock API keys carry an expiry — short-term keys last up
        // to 12h, long-term keys 1/5/30/90/365d. Re-read the env var
        // on every `client()` call so a rotated `AWS_BEARER_TOKEN_BEDROCK`
        // is picked up without restarting the agent. The configured
        // value (loaded at provider-construction time) is the fallback
        // — env wins so the live shell can override the config without
        // forcing operators to rebuild the provider.
        let bearer_token = current_bearer_token(self.bearer_token.as_deref());
        build_bedrock_client(shared, bearer_token.as_deref())
    }
}

/// Resolve the bearer token to use on the next Bedrock client build.
/// Prefers the live `AWS_BEARER_TOKEN_BEDROCK` env var so a rotated
/// shell secret takes effect immediately; falls back to the
/// provider-construction-time value when the env is unset. An empty
/// or whitespace-only env value is treated as "unset" so a shell that
/// exports the var blank doesn't poison the bearer header.
fn current_bearer_token(fallback: Option<&str>) -> Option<String> {
    match std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok() {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                fallback.map(|s| s.to_string())
            } else {
                Some(trimmed.to_string())
            }
        }
        None => fallback.map(|s| s.to_string()),
    }
}

async fn load_aws_config(region: String, base_url: Option<String>) -> SdkConfig {
    // `aws_config::defaults` already wires the standard credential
    // provider chain (env → ~/.aws/credentials → IMDS / container
    // roles). We only override region + optional endpoint here so the
    // chain itself is whatever the AWS SDK ships as best practice.
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region));
    if let Some(url) = base_url {
        loader = loader.endpoint_url(url);
    }
    loader.load().await
}

/// Choose between bearer-token auth and the AWS default credential
/// chain when constructing a Bedrock Runtime client.
///
/// * When `bearer_token` is `Some(non_empty)` we route through Bedrock's
///   HTTP bearer-auth scheme — clearing any inherited SigV4 credentials
///   from the shared `SdkConfig` so the auth-scheme resolver cannot
///   silently fall back to SigV4 when both routes are present.
/// * Otherwise we trust whatever `aws_config::defaults` resolved into
///   the shared config and only surface a `ProviderNotConfigured` error
///   when the SDK was unable to install a credentials provider at all.
pub(crate) fn build_bedrock_client(
    shared: &SdkConfig,
    bearer_token: Option<&str>,
) -> Result<BedrockClient> {
    if let Some(raw) = bearer_token {
        let token = raw.trim();
        if token.is_empty() {
            return Err(SqueezyError::ProviderNotConfigured(
                "AWS_BEARER_TOKEN_BEDROCK is set but empty; unset it or provide a non-empty token"
                    .to_string(),
            ));
        }
        let mut builder = aws_sdk_bedrockruntime::config::Builder::from(shared);
        builder.set_credentials_provider(None);
        let client_config = builder
            .bearer_token(BedrockToken::new(token.to_string(), None))
            .build();
        return Ok(BedrockClient::from_conf(client_config));
    }
    if shared.credentials_provider().is_none() {
        return Err(SqueezyError::ProviderNotConfigured(
            "AWS credentials not found; set AWS_BEARER_TOKEN_BEDROCK for bearer auth, run `aws configure`, set AWS_PROFILE, or provide AWS environment variables"
                .to_string(),
        ));
    }
    Ok(BedrockClient::new(shared))
}

/// Bedrock accepts at most four `cachePoint` markers per Converse
/// request across `tools`, `system`, and `messages`. The hard cap is
/// documented at
/// <https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html>;
/// a 5th breakpoint triggers a `ValidationException` that takes the
/// whole turn down non-retryably.
///
/// `BreakpointBudget` mirrors anthropic.rs's `BreakpointBudget`
/// (mirror of opencode's `Bedrock.Cache.Breakpoints` slot allocator)
/// so the lowering layer can decide which sections receive a marker
/// in invalidation-priority order (tools change least frequently,
/// system next, messages most often — longer-TTL must precede
/// shorter-TTL on Bedrock). When the budget is exhausted the
/// allocator drops the next request and emits a single `tracing::warn!`
/// per dropped marker so a future per-skill policy that overflows
/// the cap surfaces in logs instead of failing every request.
#[derive(Debug)]
struct BreakpointBudget {
    remaining: usize,
    dropped: usize,
}

impl BreakpointBudget {
    /// Bedrock's hard cap on `cachePoint` markers per Converse request.
    const CAP: usize = 4;

    fn new() -> Self {
        Self {
            remaining: Self::CAP,
            dropped: 0,
        }
    }

    /// Try to consume one slot for the named section. Returns `true`
    /// when the marker should be emitted, `false` when the budget is
    /// exhausted (and warns once per dropped marker so operators see
    /// when a per-skill policy outgrew Bedrock's 4-breakpoint cap).
    fn consume(&mut self, section: &'static str) -> bool {
        if self.remaining == 0 {
            self.dropped = self.dropped.saturating_add(1);
            tracing::warn!(
                provider = "bedrock",
                section,
                cap = Self::CAP,
                dropped = self.dropped,
                "bedrock cachePoint breakpoint dropped: per-request cap exceeded",
            );
            return false;
        }
        self.remaining -= 1;
        true
    }
}

impl LlmProvider for BedrockProvider {
    fn name(&self) -> &'static str {
        "bedrock"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        if let Err(err) = request.ensure_vision_support("bedrock") {
            return Box::pin(futures_util::stream::once(async move { Err(err) }));
        }
        let provider = self.clone();
        let transport = provider.transport;
        // Mid-stream `ModelStreamErrorException`, `ThrottlingException`,
        // and `InternalServerException` are documented as retryable by
        // AWS. Wrap each attempt in `with_stream_retry` so a transient
        // hiccup mid-flight reconnects (with a `StreamSkipState` to
        // suppress the already-yielded prefix) instead of tearing the
        // whole turn down. The AWS SDK's own retry policy only covers
        // the initial `send()` — once the event stream is open, only
        // squeezy's retry harness can recover the connection.
        let attempt_cancel = cancel.clone();
        let make_attempt = move || -> LlmStream {
            bedrock_stream_attempt(provider.clone(), request.clone(), attempt_cancel.clone())
        };
        with_stream_retry(
            "bedrock",
            RetryPolicy::provider_stream(transport),
            cancel,
            make_attempt,
        )
    }
}

fn bedrock_stream_attempt(
    provider: BedrockProvider,
    request: LlmRequest,
    cancel: CancellationToken,
) -> LlmStream {
    let transport = provider.transport;
    Box::pin(try_stream! {
        let client_result = tokio::select! {
            _ = cancel.cancelled() => {
                yield LlmEvent::Cancelled;
                return;
            }
            result = provider.client() => result,
        };
        let client = client_result?;
        let requested_model = request.model.to_string();
        // Newer Anthropic Claude families (Sonnet 4.5 / Opus 4.6 /
        // Sonnet 4.6) on Bedrock require a cross-region inference
        // profile and reject the bare `anthropic.claude-*` id with
        // `ValidationException`. Rewrite to `us./eu./apac./jp.`
        // based on the configured region; ARNs and ids that already
        // carry a profile prefix pass through verbatim. The
        // resolved id is surfaced via `LlmEvent::ServerModel` so
        // the transcript / cost-attribution layers see the actual
        // model that produced the turn.
        let model = apply_inference_profile_prefix(&requested_model, &provider.region);
        if model != requested_model {
            tracing::info!(
                provider = "bedrock",
                requested = %requested_model,
                resolved = %model,
                "rewrote Bedrock model id to cross-region inference profile"
            );
        }
        let prompt_caching = should_apply_caching("bedrock", &request);
        // Honor `CacheRetention::Long` end-to-end so Bedrock cache
        // points carry `ttl: 1h` instead of silently degrading to
        // the 5-minute default. When caching is disabled at the
        // policy gate we pass `None` so no breakpoints are emitted.
        let retention = if prompt_caching {
            request.effective_cache_spec().retention
        } else {
            CacheRetention::None
        };
        // Bedrock's hard cap is 4 `cachePoint` blocks per request,
        // ordered tools -> system -> messages (longer-TTL must
        // precede shorter-TTL). The auto policy currently emits
        // 3 markers (tools tail, system tail, latest user block),
        // so the cap is not exceeded yet — but a future per-skill
        // policy that adds a 5th breakpoint would hard-fail every
        // request with a `ValidationException`. Mirror opencode's
        // `Bedrock.Cache.Breakpoints` allocator: consume in
        // invalidation-priority order (least-volatile first) and
        // drop+warn on overflow rather than 4xx-ing the turn.
        let mut budget = BreakpointBudget::new();
        let emit_tools_cache = retention != CacheRetention::None
            && !request.tools.is_empty()
            && budget.consume("tools");
        let emit_system_cache = retention != CacheRetention::None
            && !request.instructions.trim().is_empty()
            && budget.consume("system");
        let emit_messages_cache = retention != CacheRetention::None && budget.consume("messages");
        let tools_retention = if emit_tools_cache {
            retention
        } else {
            CacheRetention::None
        };
        let system_retention = if emit_system_cache {
            retention
        } else {
            CacheRetention::None
        };
        let messages_retention = if emit_messages_cache {
            retention
        } else {
            CacheRetention::None
        };
        let mut builder = client.converse_stream().model_id(&model);
        for block in system_blocks(&request.instructions, system_retention)? {
            builder = builder.system(block);
        }
        // Canonicalize cross-provider tool-call ids and
        // synthesize placeholders for orphan tool results before
        // building Bedrock `toolUse` / `toolResult` blocks.
        // Bedrock's Converse API enforces the same Anthropic
        // pairing rules (every `toolResult.toolUseId` must match
        // a prior `toolUse.toolUseId` in the conversation) so a
        // mid-session swap from a non-Anthropic provider can
        // produce ids Bedrock either rejects on shape or fails
        // to match.
        let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
        for message in conversation_messages(&normalized_input, messages_retention)? {
            builder = builder.messages(message);
        }
        if let Some(config) = tool_configuration(
            &request.tools,
            tools_retention,
            request.tool_choice.as_deref(),
        )? {
            builder = builder.tool_config(config);
        }
        if let Some(inference) = inference_configuration(&request) {
            builder = builder.inference_config(inference);
        }
        let mut extra_fields: std::collections::HashMap<String, Document> =
            std::collections::HashMap::new();
        apply_thinking_extra_fields(&mut extra_fields, &request, &model);
        let body_betas = bedrock_extra_body_betas(&request.beta_headers);
        if !body_betas.is_empty() {
            let beta_array = body_betas
                .iter()
                .map(|beta| Document::String(beta.as_ref().to_string()))
                .collect();
            extra_fields.insert("anthropic_beta".to_string(), Document::Array(beta_array));
        }
        if !extra_fields.is_empty() {
            builder =
                builder.additional_model_request_fields(Document::Object(extra_fields));
        }
        if let Some(metadata) = bedrock_request_metadata_map(&provider.request_metadata) {
            builder = builder.set_request_metadata(Some(metadata));
        }

        let send_result = tokio::select! {
            _ = cancel.cancelled() => {
                yield LlmEvent::Cancelled;
                return;
            }
            result = builder.send() => result,
        };
        let response = send_result.map_err(sdk_error_to_squeezy)?;

        yield LlmEvent::Started;

        // Surface the auto-applied inference-profile prefix so the
        // TUI / transcript / cost-attribution layers see the actual
        // model that produced the turn. The tracker also serves the
        // M-15 follow-up emission from
        // `messageStop.additionalModelResponseFields` (an application-
        // inference-profile may backfill from a different foundation
        // model than the bare prefix). Only consume here when the
        // prefix actually changed the id — otherwise the pass-through
        // call would seal the tracker and the messageStop echo would
        // never fire.
        let mut server_model_echo = crate::ServerModelEcho::default();
        if model != requested_model
            && let Some(echo) = server_model_echo.observe(&requested_model, &model)
        {
            yield echo;
        }

        let mut stream = response.stream;
        let mut state = BedrockStreamState::default();
        loop {
            let polled = tokio::select! {
                _ = cancel.cancelled() => {
                    yield LlmEvent::Cancelled;
                    return;
                }
                next = timeout(
                    bedrock_idle_timeout(transport, &request, &model),
                    recv_event(&mut stream),
                ) => next,
            };
            let event = polled.map_err(|_| {
                SqueezyError::ProviderStream("Bedrock stream idle timeout".to_string())
            })??;
            let Some(event) = event else { break; };
            for llm_event in handle_bedrock_event(event, &mut state)? {
                yield llm_event;
            }
        }
        if !state.saw_message_stop {
            Err(SqueezyError::ProviderStream(
                "Bedrock stream ended without messageStop".to_string(),
            ))?;
        }
        if !state.saw_metadata {
            tracing::warn!(
                provider = "bedrock",
                model = %model,
                "Bedrock stream ended without metadata event; usage tokens unavailable for this turn"
            );
        }
        // M-15: if Bedrock echoed a resolved model id on
        // `messageStop.additionalModelResponseFields` (typical for
        // application-inference-profile routing that backfills from a
        // different foundation model), surface it through the same
        // `ServerModelEcho` tracker that observed the inference-profile
        // prefix rewrite at stream start. `observe` is idempotent: when
        // the prefix rewrite already emitted, this call is a no-op; when
        // the rewrite was a pass-through (id matched verbatim), the echo
        // is the first chance the agent sees the resolved id.
        if let Some(echoed) = state.echoed_model.as_deref()
            && let Some(echo_event) = server_model_echo.observe(&requested_model, echoed)
        {
            yield echo_event;
        }
        if let Some(payload) = state.flush_reasoning() {
            yield LlmEvent::ReasoningDone(payload);
        }
        yield LlmEvent::Completed {
            response_id: None,
            cost: state.cost(),
            stop_reason: state.stop_reason.clone(),
            reasoning_only_stop: false,
        };
    })
}

async fn recv_event(
    stream: &mut EventReceiver<
        ConverseStreamOutput,
        aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError,
    >,
) -> Result<Option<ConverseStreamOutput>> {
    stream.recv().await.map_err(classify_stream_sdk_error)
}

/// Classify a `SdkError<ConverseStreamOutputError, _>` into the
/// cross-provider `SqueezyError` envelope so the retry harness and
/// downstream observers can react to terminal-vs-retryable failure
/// modes distinctly.
///
/// AWS documents `ModelStreamErrorException` /
/// `ThrottlingException` / `InternalServerException` /
/// `ServiceUnavailableException` as retryable and
/// `ValidationException` as terminal. The Smithy stream surface
/// collapses both into the same `recv()` error type; without the
/// downcast a `ValidationException` (bad tool schema, malformed
/// image) would be retried pointlessly. We don't add new
/// `SqueezyError` variants — terminal failures land on
/// `ProviderRequest` (the existing 4xx envelope) and retryable
/// failures land on `ProviderStream` so `is_retryable_stream_error`
/// in retry.rs continues to drive reconnection.
fn classify_stream_sdk_error(
    err: SdkError<
        aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError,
        aws_smithy_types::event_stream::RawMessage,
    >,
) -> SqueezyError {
    use aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError;
    // Non-service errors (transport, timeout, dispatch) are always
    // retryable. Surface them on `ProviderStream` with a context
    // hint about the SDK-side cause.
    if !matches!(err, SdkError::ServiceError(_)) {
        let kind = match &err {
            SdkError::ConstructionFailure(_) => "construction-failure",
            SdkError::TimeoutError(_) => "timeout",
            SdkError::DispatchFailure(_) => "dispatch-failure",
            SdkError::ResponseError(_) => "response-error",
            SdkError::ServiceError(_) => "service-error",
            _ => "unknown",
        };
        return SqueezyError::ProviderStream(format!("Bedrock event stream {kind}: {err}"));
    }
    // Service error: downcast into the Smithy discriminant and emit
    // distinct envelopes by retryability class. `ValidationException`
    // is the only deterministic-failure variant — everything else is
    // a transient class the retry harness should retry.
    let inner = err.into_service_error();
    match inner {
        ConverseStreamOutputError::ValidationException(e) => {
            SqueezyError::ProviderRequest(format!("Bedrock event stream ValidationException: {e}"))
        }
        ConverseStreamOutputError::ThrottlingException(e) => {
            SqueezyError::ProviderStream(format!("Bedrock event stream ThrottlingException: {e}"))
        }
        ConverseStreamOutputError::ModelStreamErrorException(e) => SqueezyError::ProviderStream(
            format!("Bedrock event stream ModelStreamErrorException: {e}"),
        ),
        ConverseStreamOutputError::InternalServerException(e) => SqueezyError::ProviderStream(
            format!("Bedrock event stream InternalServerException: {e}"),
        ),
        ConverseStreamOutputError::ServiceUnavailableException(e) => SqueezyError::ProviderStream(
            format!("Bedrock event stream ServiceUnavailableException: {e}"),
        ),
        // `Unhandled` covers unknown error codes the SDK couldn't
        // model. Treat as retryable since callers shouldn't burn the
        // budget on a deterministic re-shape of a transient.
        other => {
            SqueezyError::ProviderStream(format!("Bedrock event stream unhandled error: {other}"))
        }
    }
}

#[derive(Debug, Default)]
struct BedrockStreamState {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    tool_blocks: HashMap<i32, PartialToolUse>,
    reasoning_blocks: HashMap<i32, AnthropicThinkingBlock>,
    finished_reasoning: Vec<AnthropicThinkingBlock>,
    saw_message_stop: bool,
    stop_reason: Option<crate::StopReason>,
    saw_metadata: bool,
    /// Model id echoed by Bedrock on `messageStop.additionalModelResponseFields`.
    /// When an application-inference-profile ARN routes to a different
    /// backing foundation model, Bedrock surfaces the resolved id here so
    /// cost attribution / transcript can pin the actually-billed model.
    /// Routed through [`crate::ServerModelEcho`] after the stream loop
    /// completes (emits at most one [`LlmEvent::ServerModel`] per turn).
    echoed_model: Option<String>,
}

impl BedrockStreamState {
    fn cost(&self) -> CostSnapshot {
        // Bedrock routes Claude models and inherits Anthropic's
        // Messages-API convention where `usage.inputTokens` is the
        // **uncached delta only**. Normalise to the cross-provider
        // convention shared by OpenAI / Google / Ollama / compatible:
        // `input_tokens` is the total prompt the model saw, and the
        // cached share lives in `cached_input_tokens`. See the matching
        // comment on `AnthropicStreamState::cost()`.
        let base = self.input_tokens;
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_write = self.cache_write_input_tokens.unwrap_or(0);
        let total_input = base.map(|b| b.saturating_add(cache_read).saturating_add(cache_write));
        CostSnapshot {
            input_tokens: total_input,
            output_tokens: self.output_tokens,
            reasoning_output_tokens: None,
            cached_input_tokens: self.cache_read_input_tokens,
            cache_write_input_tokens: self.cache_write_input_tokens,
            estimated_usd_micros: None,
        }
    }

    fn flush_reasoning(&mut self) -> Option<ReasoningPayload> {
        if self.finished_reasoning.is_empty() {
            return None;
        }
        Some(ReasoningPayload::Anthropic {
            blocks: std::mem::take(&mut self.finished_reasoning),
        })
    }
}

#[derive(Debug, Default)]
struct PartialToolUse {
    tool_use_id: String,
    name: String,
    input_json: String,
}

fn handle_bedrock_event(
    event: ConverseStreamOutput,
    state: &mut BedrockStreamState,
) -> Result<Vec<LlmEvent>> {
    match event {
        ConverseStreamOutput::MessageStart(_) => Ok(Vec::new()),
        ConverseStreamOutput::ContentBlockStart(start) => {
            let Some(ContentBlockStart::ToolUse(tool)) = start.start else {
                return Ok(Vec::new());
            };
            state.tool_blocks.insert(
                start.content_block_index,
                PartialToolUse {
                    tool_use_id: tool.tool_use_id,
                    name: tool.name,
                    input_json: String::new(),
                },
            );
            Ok(Vec::new())
        }
        ConverseStreamOutput::ContentBlockDelta(delta) => {
            match delta.delta {
                Some(ContentBlockDelta::Text(text)) => Ok(vec![LlmEvent::TextDelta(text)]),
                Some(ContentBlockDelta::ToolUse(tool_delta)) => {
                    if let Some(tool) = state.tool_blocks.get_mut(&delta.content_block_index) {
                        tool.input_json.push_str(&tool_delta.input);
                    }
                    Ok(Vec::new())
                }
                Some(ContentBlockDelta::ReasoningContent(reasoning)) => {
                    let index = delta.content_block_index;
                    let block = state.reasoning_blocks.entry(index).or_insert_with(|| {
                        AnthropicThinkingBlock {
                            kind: AnthropicThinkingKind::Thinking,
                            text: String::new(),
                            signature: None,
                            data: None,
                        }
                    });
                    match reasoning {
                        ReasoningContentBlockDelta::Text(text) => {
                            block.text.push_str(&text);
                            if text.is_empty() {
                                Ok(Vec::new())
                            } else {
                                Ok(vec![LlmEvent::ReasoningDelta {
                                    text,
                                    kind: ReasoningKind::Text,
                                }])
                            }
                        }
                        ReasoningContentBlockDelta::Signature(sig) => {
                            // Anthropic's reasoning `signature` is a
                            // full opaque base64 token attached to the
                            // closing reasoning block — not a streaming
                            // buffer. The Bedrock API has historically
                            // emitted it once per block; concatenating
                            // multiple `Signature` deltas would yield a
                            // corrupted signature that Anthropic
                            // rejects on the next-turn replay. If the
                            // upstream ever splits the signature across
                            // multiple deltas (the type is
                            // `#[non_exhaustive]`) we treat the latest
                            // value as authoritative and warn so the
                            // operator can flag it.
                            if block.signature.is_some() {
                                tracing::warn!(
                                    provider = "bedrock",
                                    block = ?index,
                                    "Bedrock emitted multiple reasoning Signature deltas; \
                                     replacing prior value with latest (Anthropic semantics \
                                     expect a single opaque blob, not a streaming buffer)",
                                );
                            }
                            block.signature = Some(sig);
                            Ok(Vec::new())
                        }
                        ReasoningContentBlockDelta::RedactedContent(blob) => {
                            block.kind = AnthropicThinkingKind::Redacted;
                            block.data = Some(hex_encode(&blob));
                            Ok(Vec::new())
                        }
                        _ => Ok(Vec::new()),
                    }
                }
                _ => Ok(Vec::new()),
            }
        }
        ConverseStreamOutput::ContentBlockStop(stop) => {
            if let Some(reasoning) = state.reasoning_blocks.remove(&stop.content_block_index) {
                state.finished_reasoning.push(reasoning);
                return Ok(Vec::new());
            }
            let Some(tool) = state.tool_blocks.remove(&stop.content_block_index) else {
                return Ok(Vec::new());
            };
            let arguments = if tool.input_json.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&tool.input_json).map_err(|err| {
                    SqueezyError::ProviderStream(format!(
                        "invalid Bedrock toolUse input JSON: {err}"
                    ))
                })?
            };
            Ok(vec![LlmEvent::ToolCall(LlmToolCall {
                call_id: tool.tool_use_id,
                name: tool.name,
                arguments,
            })])
        }
        ConverseStreamOutput::MessageStop(stop) => {
            state.saw_message_stop = true;
            state.stop_reason = Some(crate::StopReason::from_bedrock(stop.stop_reason().as_str()));
            // Application-inference-profile routing can land the turn
            // on a different backing foundation model than the caller
            // requested. Bedrock surfaces the resolved id in
            // `messageStop.additionalModelResponseFields` (Anthropic-on-
            // Bedrock typically emits a top-level `model` string).
            // Persist into state so the stream loop can route it
            // through `ServerModelEcho` after `MessageStop`.
            if let Some(fields) = stop.additional_model_response_fields()
                && let Some(echoed) = extract_echoed_model(fields)
            {
                state.echoed_model = Some(echoed);
            }
            Ok(Vec::new())
        }
        ConverseStreamOutput::Metadata(meta) => {
            state.saw_metadata = true;
            if let Some(usage) = meta.usage {
                state.input_tokens = Some(u64::try_from(usage.input_tokens).unwrap_or(0));
                state.output_tokens = Some(u64::try_from(usage.output_tokens).unwrap_or(0));
                state.cache_read_input_tokens = usage
                    .cache_read_input_tokens
                    .and_then(|n| u64::try_from(n).ok());
                state.cache_write_input_tokens = usage
                    .cache_write_input_tokens
                    .and_then(|n| u64::try_from(n).ok());
            }
            Ok(Vec::new())
        }
        // `ConverseStreamOutput` is `#[non_exhaustive]`; future Bedrock
        // features (citation deltas, guardrail traces, native
        // multi-modal output blocks, etc.) surface as new variants.
        // Log the discriminant so silent feature drift is observable
        // in tracing logs instead of disappearing into `Vec::new()`.
        other => {
            tracing::debug!(
                provider = "bedrock",
                variant = ?std::mem::discriminant(&other),
                "unhandled Bedrock ConverseStreamOutput variant; dropping"
            );
            Ok(Vec::new())
        }
    }
}

/// Populate the Bedrock `additional_model_request_fields` map with the
/// `thinking` / `output_config` blocks the configured model expects.
///
/// Routes by family:
/// * Adaptive-thinking Claude 4.6+ (opus / sonnet) — emits
///   `thinking={type:adaptive}` + `output_config={effort:...}`. The bare
///   `enabled+budget_tokens` form earns a hard 400 on these models.
/// * Pre-4.6 Claude (3.7 sonnet, opus 4.0/4.5, haiku 4.5) — emits
///   `thinking={type:enabled, budget_tokens:N}` after enforcing the
///   `max_tokens > budget_tokens + 1024` invariant the upstream
///   requires. If the configured `max_output_tokens` is too small the
///   block is skipped with a `tracing::warn!` so the operator can react
///   instead of seeing every turn 400.
///
/// Skips emission entirely when `reasoning_effort` is unset or when the
/// model registry says the model does not support reasoning. Caller
/// owns the map so it can lower additional fields (anthropic_beta, …)
/// onto the same object.
pub(crate) fn apply_thinking_extra_fields(
    extra_fields: &mut HashMap<String, Document>,
    request: &LlmRequest,
    model: &str,
) {
    let Some(effort) = request.reasoning_effort else {
        return;
    };
    if !crate::capabilities_for("bedrock", model).is_some_and(|caps| caps.reasoning_effort) {
        return;
    }
    let max_tokens = request
        .max_output_tokens
        .map(u64::from)
        .or_else(|| {
            crate::model_info_for("bedrock", model)
                .and_then(|info| info.limits)
                .map(|limits| limits.max_output_tokens)
        })
        .unwrap_or(BEDROCK_DEFAULT_MAX_OUTPUT_TOKENS);
    compute_thinking_extra_fields(extra_fields, model, effort, max_tokens);
}

/// Inner core of [`apply_thinking_extra_fields`] without the
/// reasoning-effort or capabilities gates. Pure function of `model`,
/// `effort`, and `max_tokens` so test fixtures can assert wire shapes
/// for both branches (adaptive vs enabled+budget) without registering
/// every candidate model in `models.json`.
pub(crate) fn compute_thinking_extra_fields(
    extra_fields: &mut HashMap<String, Document>,
    model: &str,
    effort: squeezy_core::ReasoningEffort,
    max_tokens: u64,
) {
    if crate::anthropic::model_uses_adaptive_thinking(model) {
        // Claude 4.6+ opus/sonnet on Bedrock reject
        // `thinking.type=enabled` and want the budget conveyed through
        // `output_config.effort` instead. Mirrors `anthropic.rs:186-193`.
        extra_fields.insert(
            "thinking".to_string(),
            Document::Object(
                [("type".to_string(), Document::String("adaptive".to_string()))]
                    .into_iter()
                    .collect(),
            ),
        );
        extra_fields.insert(
            "output_config".to_string(),
            Document::Object(
                [(
                    "effort".to_string(),
                    Document::String(bedrock_effort_label(effort).to_string()),
                )]
                .into_iter()
                .collect(),
            ),
        );
        return;
    }
    // Anthropic requires `budget_tokens >= 1024` AND
    // `max_tokens > budget_tokens`. When `max_output_tokens` is too
    // small to satisfy both at once, emitting `thinking` earns a hard
    // 400 on every turn. Skip the block in that case and warn so the
    // operator can raise `max_output_tokens` or unset
    // `reasoning_effort`.
    let ceiling = max_tokens.saturating_sub(BEDROCK_THINKING_REPLY_HEADROOM_TOKENS);
    if ceiling >= BEDROCK_MIN_THINKING_BUDGET_TOKENS {
        let budget = u64::from(effort.thinking_budget_tokens())
            .min(ceiling)
            .max(BEDROCK_MIN_THINKING_BUDGET_TOKENS);
        extra_fields.insert(
            "thinking".to_string(),
            Document::Object(
                [
                    ("type".to_string(), Document::String("enabled".to_string())),
                    (
                        "budget_tokens".to_string(),
                        Document::Number(Number::PosInt(budget)),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        );
    } else {
        tracing::warn!(
            provider = "bedrock",
            model = %model,
            max_output_tokens = max_tokens,
            min_required =
                BEDROCK_MIN_THINKING_BUDGET_TOKENS + BEDROCK_THINKING_REPLY_HEADROOM_TOKENS,
            "bedrock thinking disabled: max_output_tokens too small to satisfy \
             thinking.budget_tokens >= 1024 with a reply headroom; raise \
             max_output_tokens or clear reasoning_effort to silence this warning"
        );
    }
}

/// Map cross-provider [`ReasoningEffort`](squeezy_core::ReasoningEffort) to
/// the lowercase string Anthropic's adaptive-thinking surface expects on
/// Bedrock (`output_config.effort`). Mirrors `anthropic.rs:70-77` so the
/// Bedrock and Anthropic-native paths agree on the wire shape.
pub(crate) fn bedrock_effort_label(effort: squeezy_core::ReasoningEffort) -> &'static str {
    match effort {
        squeezy_core::ReasoningEffort::Low => "low",
        squeezy_core::ReasoningEffort::Medium => "medium",
        squeezy_core::ReasoningEffort::High => "high",
        squeezy_core::ReasoningEffort::XHigh => "max",
    }
}

/// Multiplier applied to the base stream idle timeout when the turn
/// is configured for high-effort reasoning on an adaptive-thinking
/// Claude family. Adaptive-thinking Claude 4.6+ can spend several
/// minutes thinking between visible deltas at max effort; the
/// cross-provider 300-second default is calibrated for steady-state
/// streaming and trips a false-positive idle timeout on these
/// configurations. Scale wide enough (2x) to cover documented worst-
/// case thinking gaps without hiding genuine stalls.
const BEDROCK_ADAPTIVE_THINKING_IDLE_TIMEOUT_MULTIPLIER: u32 = 2;

/// Compute the per-event idle timeout for a Bedrock stream. Starts
/// from the cross-provider [`idle_timeout`] base and doubles it when
/// both:
/// * the effective reasoning effort (request override OR the model's
///   `default_reasoning_effort` capability fallback) is `High` or
///   `XHigh`, and
/// * the model is in the adaptive-thinking Claude family
///   ([`crate::anthropic::model_uses_adaptive_thinking`]).
///
/// Other configurations keep the steady-state default so a stalled
/// non-reasoning turn still surfaces as a timeout instead of hanging.
pub(crate) fn bedrock_idle_timeout(
    transport: ProviderTransportConfig,
    request: &LlmRequest,
    model: &str,
) -> std::time::Duration {
    let base = idle_timeout(transport);
    // `default_reasoning_effort` from the Phase 1 ModelCapabilities
    // table covers the model-recommended baseline when the caller
    // left `reasoning_effort` unset. Without that fallback an agent
    // that always relies on the registry default would never get the
    // scaled timeout even on adaptive-thinking models that always
    // think for several minutes at high effort.
    let effective_effort = request.reasoning_effort.or_else(|| {
        crate::capabilities_for("bedrock", model).and_then(|caps| caps.default_reasoning_effort)
    });
    let is_high_effort = matches!(
        effective_effort,
        Some(squeezy_core::ReasoningEffort::High | squeezy_core::ReasoningEffort::XHigh)
    );
    if is_high_effort && crate::anthropic::model_uses_adaptive_thinking(model) {
        base * BEDROCK_ADAPTIVE_THINKING_IDLE_TIMEOUT_MULTIPLIER
    } else {
        base
    }
}

/// Map an AWS region id (`us-east-1`, `eu-west-1`, `ap-southeast-2`,
/// `jp-east-1`, etc.) to the cross-region inference profile prefix
/// Bedrock expects on Claude foundation models.
///
/// Returns `None` for regions that don't have a defined prefix
/// (`us-gov-*`, future regions, etc.) — the caller passes the model
/// id through verbatim in that case. The mapping mirrors clear-code's
/// `getBedrockRegionPrefix` and the static prefix list at
/// <https://docs.aws.amazon.com/bedrock/latest/userguide/inference-api-restrictions.html>:
/// `us-*` → `us`, `eu-*` → `eu`, `ap-*` → `apac`, `jp-*` → `jp`.
pub(crate) fn region_prefix(region: &str) -> Option<&'static str> {
    let region = region.to_ascii_lowercase();
    // GovCloud (`us-gov-east-1`, `us-gov-west-1`) doesn't carry
    // inference-profile prefixes; the rewrite falls back to verbatim
    // so the upstream rejection points operators at their
    // model/region mismatch instead of squeezy silently routing
    // through a wrong prefix.
    if region.starts_with("us-gov-") {
        None
    } else if region.starts_with("us-") {
        Some("us")
    } else if region.starts_with("eu-") {
        Some("eu")
    } else if region.starts_with("ap-") {
        Some("apac")
    } else if region.starts_with("jp-") {
        Some("jp")
    } else {
        None
    }
}

/// Rewrite `anthropic.claude-*` foundation-model ids to the
/// cross-region inference profile form (`us.anthropic.claude-*` /
/// `eu.anthropic.claude-*` / etc.) when the configured AWS region
/// requires one. Ids that already carry a profile prefix, ARNs
/// (`arn:aws:bedrock:...:inference-profile/...` or
/// `application-inference-profile/...`), and non-Anthropic vendor ids
/// pass through verbatim — the caller has already told Bedrock how it
/// wants to be routed and we must not double-prefix.
///
/// Newer Claude families (Sonnet 4.5, Opus 4.6, Sonnet 4.6) on Bedrock
/// reject the bare `anthropic.claude-*` form with `ValidationException`
/// because they only ship via cross-region inference profiles. Without
/// the rewrite an operator who pointed squeezy at `claude-sonnet-4-6`
/// has to manually pre-prefix every model id in their config.
pub(crate) fn apply_inference_profile_prefix(model: &str, region: &str) -> String {
    // ARN: caller specified the inference profile directly.
    if model.starts_with("arn:") {
        return model.to_string();
    }
    // Already prefixed (`us.`, `eu.`, `apac.`, `jp.`, `global.`).
    if let Some((head, _)) = model.split_once('.')
        && matches!(head, "us" | "eu" | "apac" | "jp" | "global")
    {
        return model.to_string();
    }
    // Only Anthropic Claude models on Bedrock require the cross-region
    // profile rewrite today; Mistral / Cohere / Amazon Titan ship as
    // on-demand throughput.
    if !model.starts_with("anthropic.claude-") {
        return model.to_string();
    }
    let Some(prefix) = region_prefix(region) else {
        return model.to_string();
    };
    format!("{prefix}.{model}")
}

/// Pull a server-echoed model id out of Bedrock's
/// `messageStop.additionalModelResponseFields`. Anthropic-on-Bedrock
/// typically emits a top-level `model` string when the resolved
/// backing model differs from the requested id; some integrations also
/// use `server_model`. The helper accepts either key and returns the
/// first non-empty match. Returns `None` for shapes we don't
/// understand (array, missing string, blank value) so the stream loop
/// silently falls back to "no echo" rather than fabricating one.
pub(crate) fn extract_echoed_model(fields: &Document) -> Option<String> {
    let Document::Object(map) = fields else {
        return None;
    };
    for key in ["model", "server_model"] {
        if let Some(Document::String(value)) = map.get(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn hex_encode(blob: &Blob) -> String {
    use std::fmt::Write as _;
    let bytes = blob.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // `write!` into the pre-allocated `String` skips the
        // per-byte `format!` heap allocation that previously fired
        // once per byte. `write!` into a `String` is infallible;
        // `unwrap()` is the documented idiom.
        write!(&mut out, "{b:02x}").unwrap();
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Build a Bedrock `CachePointBlock`, opting into the 1-hour TTL when
/// the caller asked for [`CacheRetention::Long`]. Bedrock honors
/// `ttl: "1h"` for Claude Opus 4.5 / Sonnet 4.5 / Haiku 4.5; without
/// the setter the breakpoint silently degrades to the 5-minute
/// provider default, defeating the intent of the cross-provider
/// `CacheSpec` knob. `CacheRetention::Short` and `None` map to the
/// 5-minute default by omitting `ttl`.
pub(crate) fn cache_point_block(retention: CacheRetention) -> Result<CachePointBlock> {
    let mut builder = CachePointBlock::builder().r#type(CachePointType::Default);
    if retention == CacheRetention::Long {
        builder = builder.ttl(CacheTtl::OneHour);
    }
    builder.build().map_err(|err| {
        SqueezyError::ProviderRequest(format!("failed to build Bedrock cachePoint: {err}"))
    })
}

pub(crate) fn system_blocks(
    instructions: &str,
    retention: CacheRetention,
) -> Result<Vec<SystemContentBlock>> {
    if instructions.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut blocks = vec![SystemContentBlock::Text(instructions.to_string())];
    if retention != CacheRetention::None {
        blocks.push(SystemContentBlock::CachePoint(cache_point_block(
            retention,
        )?));
    }
    Ok(blocks)
}

pub(crate) fn conversation_messages(
    input: &[LlmInputItem],
    retention: CacheRetention,
) -> Result<Vec<Message>> {
    let mut messages: Vec<Message> = Vec::new();
    let mut tool_names_by_id: HashMap<String, String> = HashMap::new();
    for item in input {
        match item {
            LlmInputItem::UserText(text) => push_message(
                &mut messages,
                ConversationRole::User,
                ContentBlock::Text(text.clone()),
            )?,
            LlmInputItem::AssistantText(text) => push_message(
                &mut messages,
                ConversationRole::Assistant,
                ContentBlock::Text(text.clone()),
            )?,
            LlmInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                tool_names_by_id.insert(call_id.clone(), name.clone());
                let tool_use = ToolUseBlock::builder()
                    .tool_use_id(call_id)
                    .name(name)
                    .input(json_to_document(arguments))
                    .build()
                    .map_err(|err| {
                        SqueezyError::ProviderRequest(format!(
                            "failed to build Bedrock toolUse: {err}"
                        ))
                    })?;
                push_message(
                    &mut messages,
                    ConversationRole::Assistant,
                    ContentBlock::ToolUse(tool_use),
                )?;
            }
            LlmInputItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                let tool_result = ToolResultBlock::builder()
                    .tool_use_id(call_id)
                    .content(ToolResultContentBlock::Text(output.clone()))
                    .build()
                    .map_err(|err| {
                        SqueezyError::ProviderRequest(format!(
                            "failed to build Bedrock toolResult: {err}"
                        ))
                    })?;
                push_message(
                    &mut messages,
                    ConversationRole::User,
                    ContentBlock::ToolResult(tool_result),
                )?;
            }
            LlmInputItem::Image { media_type, bytes } => {
                let image = bedrock_image_block(media_type, bytes)?;
                push_message(
                    &mut messages,
                    ConversationRole::User,
                    ContentBlock::Image(image),
                )?;
            }
            LlmInputItem::Reasoning(ReasoningPayload::Anthropic { blocks }) => {
                for block in blocks {
                    let reasoning = match block.kind {
                        AnthropicThinkingKind::Thinking => {
                            let mut builder = ReasoningTextBlock::builder().text(&block.text);
                            if let Some(sig) = &block.signature {
                                builder = builder.signature(sig);
                            }
                            let text_block = builder.build().map_err(|err| {
                                SqueezyError::ProviderRequest(format!(
                                    "failed to build Bedrock reasoning text: {err}"
                                ))
                            })?;
                            ReasoningContentBlock::ReasoningText(text_block)
                        }
                        AnthropicThinkingKind::Redacted => {
                            let data = block
                                .data
                                .as_deref()
                                .and_then(hex_decode)
                                .unwrap_or_default();
                            ReasoningContentBlock::RedactedContent(Blob::new(data))
                        }
                    };
                    push_message(
                        &mut messages,
                        ConversationRole::Assistant,
                        ContentBlock::ReasoningContent(reasoning),
                    )?;
                }
            }
            // Reasoning items from other providers are dropped when replaying to Bedrock.
            LlmInputItem::Reasoning(_) => {}
            LlmInputItem::Document {
                media_type,
                name,
                bytes,
            } => {
                let document = bedrock_document_block(media_type, name, bytes)?;
                push_message(
                    &mut messages,
                    ConversationRole::User,
                    ContentBlock::Document(document),
                )?;
            }
        }
    }
    if retention != CacheRetention::None {
        append_cache_point_to_last_user(&mut messages, retention)?;
    }
    Ok(messages)
}

fn append_cache_point_to_last_user(
    messages: &mut [Message],
    retention: CacheRetention,
) -> Result<()> {
    let Some(index) = messages
        .iter()
        .rposition(|message| *message.role() == ConversationRole::User)
    else {
        return Ok(());
    };
    let target = &messages[index];
    let mut content = target.content().to_vec();
    content.push(ContentBlock::CachePoint(cache_point_block(retention)?));
    let rebuilt = Message::builder()
        .role(ConversationRole::User)
        .set_content(Some(content))
        .build()
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!(
                "failed to attach Bedrock cachePoint to user message: {err}"
            ))
        })?;
    messages[index] = rebuilt;
    Ok(())
}

fn push_message(
    messages: &mut Vec<Message>,
    role: ConversationRole,
    block: ContentBlock,
) -> Result<()> {
    if let Some(last) = messages.last_mut()
        && *last.role() == role
    {
        let mut content = last.content().to_vec();
        content.push(block);
        let rebuilt = Message::builder()
            .role(role)
            .set_content(Some(content))
            .build()
            .map_err(|err| {
                SqueezyError::ProviderRequest(format!("failed to merge Bedrock message: {err}"))
            })?;
        *last = rebuilt;
        return Ok(());
    }
    let message = Message::builder()
        .role(role)
        .content(block)
        .build()
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("failed to build Bedrock message: {err}"))
        })?;
    messages.push(message);
    Ok(())
}

pub(crate) fn tool_configuration(
    specs: &[Arc<LlmToolSpec>],
    retention: CacheRetention,
    tool_choice: Option<&str>,
) -> Result<Option<ToolConfiguration>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let caching = retention != CacheRetention::None;
    let mut tools = Vec::with_capacity(specs.len() + usize::from(caching));
    for spec in specs {
        let schema = ToolInputSchema::Json(json_to_document(&spec.parameters));
        let tool_spec = ToolSpecification::builder()
            .name(&spec.name)
            .description(&spec.description)
            .input_schema(schema)
            .build()
            .map_err(|err| {
                SqueezyError::ProviderRequest(format!("failed to build Bedrock tool spec: {err}"))
            })?;
        tools.push(Tool::ToolSpec(tool_spec));
    }
    if caching
        && let Some(idx) = specs
            .iter()
            .rposition(|spec| !spec.name.starts_with(DYNAMIC_TOOL_NAME_PREFIX))
    {
        tools.insert(idx + 1, Tool::CachePoint(cache_point_block(retention)?));
    }
    let mut builder = ToolConfiguration::builder().set_tools(Some(tools));
    if let Some(choice) = bedrock_tool_choice(tool_choice)? {
        builder = builder.tool_choice(choice);
    }
    let config = builder.build().map_err(|err| {
        SqueezyError::ProviderRequest(format!("failed to build Bedrock toolConfig: {err}"))
    })?;
    Ok(Some(config))
}

/// Map cross-provider `LlmRequest.tool_choice` to Bedrock's
/// [`ToolChoice`] enum. The cross-provider surface uses the OpenAI
/// vocabulary (`auto`, `required`, `<tool_name>`); the helper
/// normalizes case and routes:
///
/// * `Some("auto")` → [`ToolChoice::Auto`] — model picks freely.
/// * `Some("required")` / `Some("any")` → [`ToolChoice::Any`] —
///   the model MUST emit a tool call (no free-form reply allowed).
///   Tool-shy models like Mistral / Nova benefit from `Any` to force a
///   call. Mirrors the opencode mapping
///   (`bedrock-converse.ts:232-238`).
/// * `Some(name)` (any other non-empty value, optionally prefixed
///   `tool:`) → [`ToolChoice::Tool`] with the literal tool name. The
///   caller has already advertised this name in `LlmRequest.tools`;
///   Bedrock rejects the request otherwise.
/// * `None` / `Some("")` → `Ok(None)`, leaving the field unset so the
///   provider applies its default (typically `auto`).
pub(crate) fn bedrock_tool_choice(
    raw: Option<&str>,
) -> Result<Option<aws_sdk_bedrockruntime::types::ToolChoice>> {
    use aws_sdk_bedrockruntime::types::{
        AnyToolChoice, AutoToolChoice, SpecificToolChoice, ToolChoice,
    };
    let Some(value) = raw else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "auto" => Ok(Some(ToolChoice::Auto(AutoToolChoice::builder().build()))),
        "required" | "any" => Ok(Some(ToolChoice::Any(AnyToolChoice::builder().build()))),
        _ => {
            // Strip an optional `tool:` prefix so callers using the
            // OpenAI Responses convention can route through without a
            // separate code path.
            let name = trimmed.strip_prefix("tool:").unwrap_or(trimmed).trim();
            if name.is_empty() {
                return Ok(None);
            }
            let specific = SpecificToolChoice::builder()
                .name(name.to_string())
                .build()
                .map_err(|err| {
                    SqueezyError::ProviderRequest(format!(
                        "failed to build Bedrock SpecificToolChoice for `{name}`: {err}"
                    ))
                })?;
            Ok(Some(ToolChoice::Tool(specific)))
        }
    }
}

/// Lower the cross-provider sampling knobs on [`LlmRequest`] into a
/// Bedrock `InferenceConfiguration`. Returns `None` when none of the
/// fields are set so callers omit the field entirely instead of
/// shipping an empty object — the Converse API treats absent and empty
/// equivalently, but skipping it keeps the wire payload minimal and
/// makes the "all defaults" case observable in request logs.
///
/// `max_output_tokens`, `temperature`, `top_p`, and `stop` all map
/// 1:1 to the SDK fields. Token counts are clamped to `i32::MAX`
/// because the SDK uses a signed 32-bit field where the cross-provider
/// surface is `u32`.
pub(crate) fn inference_configuration(request: &LlmRequest) -> Option<InferenceConfiguration> {
    if request.max_output_tokens.is_none()
        && request.temperature.is_none()
        && request.top_p.is_none()
        && request.stop.is_empty()
    {
        return None;
    }
    let mut builder = InferenceConfiguration::builder();
    if let Some(max) = request.max_output_tokens {
        builder = builder.max_tokens(i32::try_from(max).unwrap_or(i32::MAX));
    }
    if let Some(temp) = request.temperature {
        builder = builder.temperature(temp);
    }
    if let Some(top_p) = request.top_p {
        builder = builder.top_p(top_p);
    }
    if !request.stop.is_empty() {
        for stop in &request.stop {
            builder = builder.stop_sequences(stop.clone());
        }
    }
    Some(builder.build())
}

/// Build a Bedrock `ImageBlock` from an `LlmInputItem::Image` payload.
/// Maps the canonical `image/{png,jpeg,gif,webp}` MIME strings to the
/// SDK's `ImageFormat` enum and wraps the raw bytes in a `Blob` so the
/// Converse API receives the binary payload directly (no base64 wrap —
/// the SDK does that on the wire). Returns a structured error for
/// unknown MIME types instead of silently dropping the image.
pub(crate) fn bedrock_image_block(media_type: &str, bytes: &Arc<[u8]>) -> Result<ImageBlock> {
    let format = match media_type.to_ascii_lowercase().as_str() {
        "image/png" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        other => {
            return Err(SqueezyError::ProviderRequest(format!(
                "Bedrock does not support image MIME `{other}`; expected one of image/png, image/jpeg, image/gif, image/webp",
            )));
        }
    };
    if bytes.len() > BEDROCK_IMAGE_MAX_BYTES {
        return Err(SqueezyError::ProviderRequest(format!(
            "image is {} bytes; exceeds Bedrock's {} byte limit for Claude vision. Resize before \
             attaching or switch to a Nova-class model that supports larger payloads",
            bytes.len(),
            BEDROCK_IMAGE_MAX_BYTES,
        )));
    }
    ImageBlock::builder()
        .format(format)
        .source(ImageSource::Bytes(Blob::new(bytes.as_ref().to_vec())))
        .build()
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("failed to build Bedrock image block: {err}"))
        })
}

/// Build a Bedrock `DocumentBlock` from an `LlmInputItem::Document`
/// payload. Maps canonical MIME strings (and a couple of common
/// vendor-prefixed equivalents) to the SDK's `DocumentFormat` enum and
/// wraps the raw bytes in a `Blob` so the Converse API receives the
/// binary payload directly. Returns a structured error for unknown
/// MIME types instead of silently dropping the document.
///
/// The Bedrock Converse API enforces a strict allow-list on the `name`
/// field (alphanumeric, single spaces, hyphens, parentheses, square
/// brackets) and rejects anything else with `ValidationException`.
/// We canonicalize here so a callers' arbitrary filename
/// (`/tmp/foo bar.pdf`) round-trips into a Bedrock-safe `name` instead
/// of failing every request.
pub(crate) fn bedrock_document_block(
    media_type: &str,
    name: &str,
    bytes: &Arc<[u8]>,
) -> Result<DocumentBlock> {
    let format = match media_type.to_ascii_lowercase().as_str() {
        "application/pdf" | "application/x-pdf" => DocumentFormat::Pdf,
        "text/csv" | "application/csv" => DocumentFormat::Csv,
        "application/msword" => DocumentFormat::Doc,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            DocumentFormat::Docx
        }
        "application/vnd.ms-excel" => DocumentFormat::Xls,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => DocumentFormat::Xlsx,
        "text/html" | "application/xhtml+xml" => DocumentFormat::Html,
        "text/markdown" | "text/x-markdown" => DocumentFormat::Md,
        "text/plain" => DocumentFormat::Txt,
        other => {
            return Err(SqueezyError::ProviderRequest(format!(
                "Bedrock does not support document MIME `{other}`; expected one of \
                 application/pdf, text/csv, application/msword, \
                 application/vnd.openxmlformats-officedocument.wordprocessingml.document, \
                 application/vnd.ms-excel, \
                 application/vnd.openxmlformats-officedocument.spreadsheetml.sheet, \
                 text/html, text/markdown, text/plain",
            )));
        }
    };
    let canonical_name = sanitize_bedrock_document_name(name);
    DocumentBlock::builder()
        .format(format)
        .name(canonical_name)
        .source(DocumentSource::Bytes(Blob::new(bytes.as_ref().to_vec())))
        .build()
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("failed to build Bedrock document block: {err}"))
        })
}

/// Canonicalize a caller-supplied filename for Bedrock's document
/// `name` field. The Converse API rejects anything outside
/// alphanumerics, single spaces, hyphens, parentheses, or square
/// brackets with `ValidationException`. We replace disallowed runs
/// with a single hyphen, collapse multi-space runs, and trim
/// surrounding whitespace so the name still resembles the caller's
/// intent. An entirely-disallowed input falls back to `document` so
/// the request can still ship.
fn sanitize_bedrock_document_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_replaced = false;
    let mut prev_space = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '(' | ')' | '[' | ']') {
            out.push(c);
            prev_replaced = false;
            prev_space = false;
        } else if c == ' ' {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
            prev_replaced = false;
        } else if !prev_replaced && !out.is_empty() {
            out.push('-');
            prev_replaced = true;
            prev_space = false;
        }
    }
    let trimmed = out.trim_matches(|c: char| c == ' ' || c == '-').to_string();
    if trimmed.is_empty() {
        "document".to_string()
    } else {
        trimmed
    }
}

pub(crate) fn json_to_document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(number) => {
            if let Some(int) = number.as_u64() {
                Document::Number(Number::PosInt(int))
            } else if let Some(int) = number.as_i64() {
                if int < 0 {
                    Document::Number(Number::NegInt(int))
                } else {
                    Document::Number(Number::PosInt(int as u64))
                }
            } else if let Some(float) = number.as_f64() {
                Document::Number(Number::Float(float))
            } else {
                Document::Null
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(values) => Document::Array(values.iter().map(json_to_document).collect()),
        Value::Object(map) => Document::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), json_to_document(value)))
                .collect(),
        ),
    }
}

/// Convert configured cost-allocation tags into the SDK shape the
/// Converse builder accepts. Returns `None` for an empty map so the
/// provider omits the `requestMetadata` field entirely instead of
/// sending an empty object — Bedrock treats absent and empty
/// equivalently, but skipping the field keeps the wire payload
/// minimal and makes the "no tags configured" case observable in
/// request logs.
pub(crate) fn bedrock_request_metadata_map(
    metadata: &BTreeMap<String, String>,
) -> Option<HashMap<String, String>> {
    if metadata.is_empty() {
        return None;
    }
    Some(
        metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

fn sdk_error_to_squeezy<E: std::fmt::Display, R>(error: SdkError<E, R>) -> SqueezyError {
    match &error {
        SdkError::ServiceError(_) => SqueezyError::ProviderRequest(error.to_string()),
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => {
            SqueezyError::ProviderStream(error.to_string())
        }
        _ => SqueezyError::ProviderRequest(error.to_string()),
    }
}

#[cfg(test)]
#[path = "bedrock_tests.rs"]
mod tests;
