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
        CachePointBlock, CachePointType, ContentBlock, ContentBlockDelta, ContentBlockStart,
        ConversationRole, ImageBlock, ImageFormat, ImageSource, InferenceConfiguration, Message,
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
    AnthropicThinkingBlock, AnthropicThinkingKind, LlmEvent, LlmInputItem, LlmProvider, LlmRequest,
    LlmStream, LlmToolCall, LlmToolSpec, ReasoningKind, ReasoningPayload,
    anthropic_betas::bedrock_extra_body_betas,
    cache_policy::{DYNAMIC_TOOL_NAME_PREFIX, should_apply_caching},
    retry::idle_timeout,
};

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
        build_bedrock_client(shared, self.bearer_token.as_deref())
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
        Box::pin(try_stream! {
            let client_result = tokio::select! {
                _ = cancel.cancelled() => {
                    yield LlmEvent::Cancelled;
                    return;
                }
                result = provider.client() => result,
            };
            let client = client_result?;
            let model = request.model.to_string();
            let prompt_caching = should_apply_caching("bedrock", &request);
            let mut builder = client.converse_stream().model_id(&model);
            for block in system_blocks(&request.instructions, prompt_caching)? {
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
            for message in conversation_messages(&normalized_input, prompt_caching)? {
                builder = builder.messages(message);
            }
            if let Some(config) = tool_configuration(&request.tools, prompt_caching)? {
                builder = builder.tool_config(config);
            }
            if let Some(inference) = inference_configuration(&request) {
                builder = builder.inference_config(inference);
            }
            let mut extra_fields: std::collections::HashMap<String, Document> =
                std::collections::HashMap::new();
            if let Some(effort) = request.reasoning_effort
                && crate::capabilities_for("bedrock", &model)
                    .is_some_and(|caps| caps.reasoning_effort)
            {
                let budget = i128::from(effort.thinking_budget_tokens());
                let thinking = Document::Object(
                    [
                        (
                            "type".to_string(),
                            Document::String("enabled".to_string()),
                        ),
                        (
                            "budget_tokens".to_string(),
                            Document::Number(Number::PosInt(budget as u64)),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                );
                extra_fields.insert("thinking".to_string(), thinking);
            }
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

            let mut stream = response.stream;
            let mut state = BedrockStreamState::default();
            loop {
                let polled = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield LlmEvent::Cancelled;
                        return;
                    }
                    next = timeout(idle_timeout(transport), recv_event(&mut stream)) => next,
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
}

async fn recv_event(
    stream: &mut EventReceiver<
        ConverseStreamOutput,
        aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError,
    >,
) -> Result<Option<ConverseStreamOutput>> {
    stream
        .recv()
        .await
        .map_err(|err| SqueezyError::ProviderStream(format!("Bedrock event stream error: {err}")))
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
                            match block.signature.as_mut() {
                                Some(existing) => existing.push_str(&sig),
                                None => block.signature = Some(sig),
                            }
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
        _ => Ok(Vec::new()),
    }
}

fn hex_encode(blob: &Blob) -> String {
    let bytes = blob.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
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

fn cache_point_block() -> Result<CachePointBlock> {
    CachePointBlock::builder()
        .r#type(CachePointType::Default)
        .build()
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("failed to build Bedrock cachePoint: {err}"))
        })
}

pub(crate) fn system_blocks(
    instructions: &str,
    prompt_caching: bool,
) -> Result<Vec<SystemContentBlock>> {
    if instructions.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut blocks = vec![SystemContentBlock::Text(instructions.to_string())];
    if prompt_caching {
        blocks.push(SystemContentBlock::CachePoint(cache_point_block()?));
    }
    Ok(blocks)
}

pub(crate) fn conversation_messages(
    input: &[LlmInputItem],
    prompt_caching: bool,
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
            // Bedrock Converse accepts document content blocks up to
            // 4.5 MB (pdf/csv/docx/xlsx/html/txt/md). Per-provider
            // lowering lands in Phase 4; for now we debug-log and
            // skip so the request still ships.
            LlmInputItem::Document { name, .. } => {
                tracing::debug!(
                    target: "squeezy_llm::bedrock",
                    name = name.as_str(),
                    "bedrock document content block not yet implemented; skipping",
                );
            }
        }
    }
    if prompt_caching {
        append_cache_point_to_last_user(&mut messages)?;
    }
    Ok(messages)
}

fn append_cache_point_to_last_user(messages: &mut [Message]) -> Result<()> {
    let Some(index) = messages
        .iter()
        .rposition(|message| *message.role() == ConversationRole::User)
    else {
        return Ok(());
    };
    let target = &messages[index];
    let mut content = target.content().to_vec();
    content.push(ContentBlock::CachePoint(cache_point_block()?));
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
    prompt_caching: bool,
) -> Result<Option<ToolConfiguration>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut tools = Vec::with_capacity(specs.len() + usize::from(prompt_caching));
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
    if prompt_caching
        && let Some(idx) = specs
            .iter()
            .rposition(|spec| !spec.name.starts_with(DYNAMIC_TOOL_NAME_PREFIX))
    {
        tools.insert(idx + 1, Tool::CachePoint(cache_point_block()?));
    }
    let config = ToolConfiguration::builder()
        .set_tools(Some(tools))
        .build()
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("failed to build Bedrock toolConfig: {err}"))
        })?;
    Ok(Some(config))
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
    ImageBlock::builder()
        .format(format)
        .source(ImageSource::Bytes(Blob::new(bytes.as_ref().to_vec())))
        .build()
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("failed to build Bedrock image block: {err}"))
        })
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
