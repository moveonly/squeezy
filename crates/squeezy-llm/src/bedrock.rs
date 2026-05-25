use std::{collections::HashMap, sync::Arc};

use async_stream::try_stream;
use aws_config::{BehaviorVersion, SdkConfig};
use aws_sdk_bedrockruntime::types::ConverseStreamOutput;
use aws_sdk_bedrockruntime::{
    Client as BedrockClient,
    config::Region,
    error::SdkError,
    primitives::event_stream::EventReceiver,
    types::{
        ContentBlock, ContentBlockDelta, ContentBlockStart, ConversationRole, Message,
        SystemContentBlock, Tool, ToolConfiguration, ToolInputSchema, ToolResultBlock,
        ToolResultContentBlock, ToolSpecification, ToolUseBlock,
    },
};
use aws_smithy_types::{Document, Number};
use serde_json::Value;
use squeezy_core::{BedrockConfig, CostSnapshot, ProviderTransportConfig, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

use crate::{LlmEvent, LlmInputItem, LlmProvider, LlmRequest, LlmStream, LlmToolCall, LlmToolSpec};

#[derive(Debug, Clone)]
pub struct BedrockProvider {
    region: String,
    base_url: Option<String>,
    transport: ProviderTransportConfig,
    shared: Arc<tokio::sync::OnceCell<SdkConfig>>,
}

impl BedrockProvider {
    pub fn from_config(config: &BedrockConfig) -> Result<Self> {
        Ok(Self {
            region: config.region.clone(),
            base_url: config.base_url.clone(),
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
        if shared.credentials_provider().is_none() {
            return Err(SqueezyError::ProviderNotConfigured(
                "AWS credentials not found; configure with `aws configure`, AWS_PROFILE, or environment variables"
                    .to_string(),
            ));
        }
        Ok(BedrockClient::new(shared))
    }
}

async fn load_aws_config(region: String, base_url: Option<String>) -> SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region));
    if let Some(url) = base_url {
        loader = loader.endpoint_url(url);
    }
    loader.load().await
}

impl LlmProvider for BedrockProvider {
    fn name(&self) -> &'static str {
        "bedrock"
    }

    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream {
        let provider = self.clone();
        // `stream_idle_timeout_ms` is plumbed for future use; the AWS SDK
        // already enforces its own per-event timeouts via the smithy
        // runtime, and adding a second tokio::time::timeout layer here
        // would require pinning the event receiver in place. Tracked as a
        // follow-up.
        let _ = provider.transport;
        Box::pin(try_stream! {
            let client_result = tokio::select! {
                _ = cancel.cancelled() => {
                    yield LlmEvent::Cancelled;
                    return;
                }
                result = provider.client() => result,
            };
            let client = client_result?;
            let model = request.model.clone();
            let mut builder = client.converse_stream().model_id(model);
            for block in system_blocks(&request.instructions) {
                builder = builder.system(block);
            }
            for message in conversation_messages(&request.input)? {
                builder = builder.messages(message);
            }
            if let Some(config) = tool_configuration(&request.tools)? {
                builder = builder.tool_config(config);
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
                    next = recv_event(&mut stream) => next,
                };
                let event = polled?;
                let Some(event) = event else { break; };
                if let Some(llm_event) = handle_bedrock_event(event, &mut state)? {
                    yield llm_event;
                }
            }
            if !state.saw_message_stop {
                Err(SqueezyError::ProviderStream(
                    "Bedrock stream ended without messageStop".to_string(),
                ))?;
            }
            yield LlmEvent::Completed {
                response_id: None,
                cost: state.cost(),
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
    saw_message_stop: bool,
}

impl BedrockStreamState {
    fn cost(&self) -> CostSnapshot {
        CostSnapshot {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            reasoning_output_tokens: None,
            cached_input_tokens: self.cache_read_input_tokens,
            cache_write_input_tokens: self.cache_write_input_tokens,
            estimated_usd_micros: None,
        }
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
) -> Result<Option<LlmEvent>> {
    match event {
        ConverseStreamOutput::MessageStart(_) => Ok(None),
        ConverseStreamOutput::ContentBlockStart(start) => {
            let Some(ContentBlockStart::ToolUse(tool)) = start.start else {
                return Ok(None);
            };
            state.tool_blocks.insert(
                start.content_block_index,
                PartialToolUse {
                    tool_use_id: tool.tool_use_id,
                    name: tool.name,
                    input_json: String::new(),
                },
            );
            Ok(None)
        }
        ConverseStreamOutput::ContentBlockDelta(delta) => match delta.delta {
            Some(ContentBlockDelta::Text(text)) => Ok(Some(LlmEvent::TextDelta(text))),
            Some(ContentBlockDelta::ToolUse(tool_delta)) => {
                if let Some(tool) = state.tool_blocks.get_mut(&delta.content_block_index) {
                    tool.input_json.push_str(&tool_delta.input);
                }
                Ok(None)
            }
            _ => Ok(None),
        },
        ConverseStreamOutput::ContentBlockStop(stop) => {
            let Some(tool) = state.tool_blocks.remove(&stop.content_block_index) else {
                return Ok(None);
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
            Ok(Some(LlmEvent::ToolCall(LlmToolCall {
                call_id: tool.tool_use_id,
                name: tool.name,
                arguments,
            })))
        }
        ConverseStreamOutput::MessageStop(_) => {
            state.saw_message_stop = true;
            Ok(None)
        }
        ConverseStreamOutput::Metadata(meta) => {
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
            Ok(None)
        }
        _ => Ok(None),
    }
}

pub(crate) fn system_blocks(instructions: &str) -> Vec<SystemContentBlock> {
    if instructions.trim().is_empty() {
        Vec::new()
    } else {
        vec![SystemContentBlock::Text(instructions.to_string())]
    }
}

pub(crate) fn conversation_messages(input: &[LlmInputItem]) -> Result<Vec<Message>> {
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
            LlmInputItem::FunctionCallOutput { call_id, output } => {
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
        }
    }
    Ok(messages)
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

pub(crate) fn tool_configuration(specs: &[Arc<LlmToolSpec>]) -> Result<Option<ToolConfiguration>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut tools = Vec::with_capacity(specs.len());
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
    let config = ToolConfiguration::builder()
        .set_tools(Some(tools))
        .build()
        .map_err(|err| {
            SqueezyError::ProviderRequest(format!("failed to build Bedrock toolConfig: {err}"))
        })?;
    Ok(Some(config))
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
