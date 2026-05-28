use std::sync::Arc;

use aws_sdk_bedrockruntime::types::{
    CachePointType, ContentBlock, ConversationRole, ConverseStreamMetadataEvent,
    ConverseStreamOutput, MessageStopEvent, StopReason, SystemContentBlock, TokenUsage,
    ToolInputSchema,
};
use aws_smithy_types::{Document, Number};
use serde_json::json;

use super::{
    BedrockStreamState, conversation_messages, handle_bedrock_event, json_to_document,
    system_blocks, tool_configuration,
};
use crate::anthropic_betas::bedrock_extra_body_betas;
use crate::{LlmInputItem, LlmToolSpec};

#[test]
fn system_blocks_skip_blank_instructions() {
    assert!(system_blocks("", false).expect("ok").is_empty());
    assert!(system_blocks("   \n  ", false).expect("ok").is_empty());
    assert!(system_blocks("", true).expect("ok").is_empty());
}

#[test]
fn system_blocks_emit_single_text_block() {
    let blocks = system_blocks("be helpful", false).expect("ok");
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        SystemContentBlock::Text(text) => assert_eq!(text, "be helpful"),
        _ => panic!("expected Text system block"),
    }
}

#[test]
fn system_blocks_append_cache_point_when_caching_enabled() {
    let blocks = system_blocks("be helpful", true).expect("ok");
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], SystemContentBlock::Text(text) if text == "be helpful"));
    let SystemContentBlock::CachePoint(cache_point) = &blocks[1] else {
        panic!("expected CachePoint after system text, got {:?}", blocks[1]);
    };
    assert_eq!(*cache_point.r#type(), CachePointType::Default);
}

#[test]
fn conversation_messages_merge_consecutive_user_turns() {
    let messages = conversation_messages(
        &[
            LlmInputItem::UserText("hello".to_string()),
            LlmInputItem::UserText("again".to_string()),
            LlmInputItem::AssistantText("hi".to_string()),
        ],
        false,
    )
    .expect("build messages");

    assert_eq!(messages.len(), 2);
    assert_eq!(*messages[0].role(), ConversationRole::User);
    assert_eq!(messages[0].content().len(), 2);
    assert_eq!(*messages[1].role(), ConversationRole::Assistant);
}

#[test]
fn conversation_messages_round_trip_tool_call_and_result() {
    let messages = conversation_messages(
        &[
            LlmInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "search".to_string(),
                arguments: json!({"query": "rust"}),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "ok".to_string(),
            },
        ],
        false,
    )
    .expect("build messages");

    assert_eq!(messages.len(), 2);
    assert_eq!(*messages[0].role(), ConversationRole::Assistant);
    match &messages[0].content()[0] {
        ContentBlock::ToolUse(tool) => {
            assert_eq!(tool.tool_use_id(), "call_1");
            assert_eq!(tool.name(), "search");
        }
        _ => panic!("expected tool use block"),
    }
    assert_eq!(*messages[1].role(), ConversationRole::User);
    match &messages[1].content()[0] {
        ContentBlock::ToolResult(result) => {
            assert_eq!(result.tool_use_id(), "call_1");
        }
        _ => panic!("expected tool result block"),
    }
}

#[test]
fn conversation_messages_append_cache_point_to_last_user_message() {
    let messages = conversation_messages(
        &[
            LlmInputItem::UserText("first".to_string()),
            LlmInputItem::AssistantText("ack".to_string()),
            LlmInputItem::UserText("second".to_string()),
        ],
        true,
    )
    .expect("build messages");

    assert_eq!(messages.len(), 3);
    let final_user = messages.last().expect("at least one message");
    assert_eq!(*final_user.role(), ConversationRole::User);
    let content = final_user.content();
    assert_eq!(content.len(), 2, "user text + cache point block");
    assert!(matches!(&content[0], ContentBlock::Text(text) if text == "second"));
    let ContentBlock::CachePoint(cache_point) = &content[1] else {
        panic!(
            "expected trailing CachePoint on final user message, got {:?}",
            content[1]
        );
    };
    assert_eq!(*cache_point.r#type(), CachePointType::Default);

    // No other message should carry a cache point.
    let mid_user = &messages[0];
    assert_eq!(*mid_user.role(), ConversationRole::User);
    for block in mid_user.content() {
        assert!(
            !matches!(block, ContentBlock::CachePoint(_)),
            "earlier user message should not carry a cache point"
        );
    }
}

#[test]
fn conversation_messages_skip_cache_point_when_no_user_message() {
    let messages = conversation_messages(&[LlmInputItem::AssistantText("solo".to_string())], true)
        .expect("build messages");

    assert_eq!(messages.len(), 1);
    assert_eq!(*messages[0].role(), ConversationRole::Assistant);
    for block in messages[0].content() {
        assert!(
            !matches!(block, ContentBlock::CachePoint(_)),
            "assistant message should not carry a cache point"
        );
    }
}

#[test]
fn tool_configuration_round_trips_json_schema() {
    let specs = vec![
        LlmToolSpec {
            name: "search".to_string(),
            description: "Web search".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }),
            strict: false,
        }
        .into(),
    ];
    let config = tool_configuration(&specs, false)
        .expect("ok")
        .expect("present");
    assert_eq!(config.tools().len(), 1);
    let tool_spec = match &config.tools()[0] {
        aws_sdk_bedrockruntime::types::Tool::ToolSpec(spec) => spec,
        other => panic!("expected ToolSpec, got {other:?}"),
    };
    assert_eq!(tool_spec.name(), "search");
    let ToolInputSchema::Json(document) = tool_spec.input_schema().expect("schema") else {
        panic!("expected JSON schema")
    };
    let Document::Object(map) = document else {
        panic!("expected object document");
    };
    assert!(map.contains_key("properties"));
    assert!(map.contains_key("required"));
}

#[test]
fn tool_configuration_appends_cache_point_after_last_tool() {
    let specs = vec![
        LlmToolSpec {
            name: "search".to_string(),
            description: "Web search".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
        LlmToolSpec {
            name: "lookup".to_string(),
            description: "Lookup".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
    ];
    let config = tool_configuration(&specs, true)
        .expect("ok")
        .expect("present");
    let tools = config.tools();
    assert_eq!(tools.len(), 3, "two specs plus trailing cache point");
    assert!(matches!(
        &tools[0],
        aws_sdk_bedrockruntime::types::Tool::ToolSpec(spec) if spec.name() == "search"
    ));
    assert!(matches!(
        &tools[1],
        aws_sdk_bedrockruntime::types::Tool::ToolSpec(spec) if spec.name() == "lookup"
    ));
    let aws_sdk_bedrockruntime::types::Tool::CachePoint(cache_point) = &tools[2] else {
        panic!("expected trailing Tool::CachePoint, got {:?}", tools[2]);
    };
    assert_eq!(*cache_point.r#type(), CachePointType::Default);
}

#[test]
fn tool_configuration_returns_none_when_empty() {
    assert!(tool_configuration(&[], false).expect("ok").is_none());
    assert!(
        tool_configuration(&[], true).expect("ok").is_none(),
        "no tools means no tool config, even when caching is requested"
    );
}

#[test]
fn json_to_document_preserves_numeric_kinds() {
    let document = json_to_document(&json!({
        "u": 42,
        "i": -7,
        "f": 3.5,
        "b": true,
        "n": null,
        "arr": [1, "two"],
    }));
    let Document::Object(map) = document else {
        panic!("expected object document");
    };
    assert!(matches!(
        map.get("u"),
        Some(Document::Number(Number::PosInt(42)))
    ));
    assert!(matches!(
        map.get("i"),
        Some(Document::Number(Number::NegInt(-7)))
    ));
    assert!(matches!(
        map.get("f"),
        Some(Document::Number(Number::Float(_)))
    ));
    assert!(matches!(map.get("b"), Some(Document::Bool(true))));
    assert!(matches!(map.get("n"), Some(Document::Null)));
    let Some(Document::Array(arr)) = map.get("arr") else {
        panic!("expected array document");
    };
    assert_eq!(arr.len(), 2);
}

#[test]
fn beta_headers_route_into_extra_body_params_on_bedrock() {
    // Bedrock's gateway strips non-standard HTTP headers; the routing
    // helper must keep only the body-param-eligible subset, which the
    // provider then attaches to `additional_model_request_fields`.
    let betas: Arc<[Arc<str>]> = Arc::from(vec![
        Arc::<str>::from("context-1m-2025-08-07"),
        Arc::<str>::from("interleaved-thinking-2025-05-14"),
        Arc::<str>::from("claude-code-20250219"),
    ]);
    let body_betas = bedrock_extra_body_betas(&betas);
    let body_strs: Vec<&str> = body_betas.iter().map(|b| b.as_ref()).collect();
    assert_eq!(
        body_strs,
        vec!["context-1m-2025-08-07", "interleaved-thinking-2025-05-14"],
        "Bedrock subset must drop header-only betas (claude-code-*) and preserve order",
    );
}

#[test]
fn metadata_event_records_usage_tokens() {
    let mut state = BedrockStreamState::default();
    let usage = TokenUsage::builder()
        .input_tokens(123)
        .output_tokens(45)
        .total_tokens(168)
        .build()
        .expect("build usage");
    let metadata = ConverseStreamMetadataEvent::builder().usage(usage).build();
    let events = handle_bedrock_event(ConverseStreamOutput::Metadata(metadata), &mut state)
        .expect("handle metadata");
    assert!(events.is_empty());
    assert!(state.saw_metadata);
    assert_eq!(state.input_tokens, Some(123));
    assert_eq!(state.output_tokens, Some(45));
}

#[test]
fn message_stop_without_metadata_leaves_usage_unset() {
    let mut state = BedrockStreamState::default();
    let stop = MessageStopEvent::builder()
        .stop_reason(StopReason::EndTurn)
        .build()
        .expect("build stop");
    let events = handle_bedrock_event(ConverseStreamOutput::MessageStop(stop), &mut state)
        .expect("handle stop");
    assert!(events.is_empty());
    assert!(state.saw_message_stop);
    assert!(
        !state.saw_metadata,
        "metadata flag should remain false when only messageStop has arrived"
    );
    let cost = state.cost();
    assert!(
        cost.input_tokens.is_none() && cost.output_tokens.is_none(),
        "cost reports None when Metadata never arrived, signalling missing usage rather than zero"
    );
}

#[test]
fn conversation_messages_emit_image_content_block() {
    use aws_sdk_bedrockruntime::types::ImageFormat;

    let bytes: Arc<[u8]> = Arc::from(vec![
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3,
    ]);
    let messages = conversation_messages(
        &[
            LlmInputItem::UserText("describe this".to_string()),
            LlmInputItem::Image {
                media_type: "image/png".to_string(),
                bytes: bytes.clone(),
            },
        ],
        false,
    )
    .expect("build messages");

    // User text + image coalesce into a single user message with two
    // content blocks so the Converse API sees them as one multimodal
    // turn.
    assert_eq!(messages.len(), 1);
    assert_eq!(*messages[0].role(), ConversationRole::User);
    let content = messages[0].content();
    assert_eq!(content.len(), 2);
    assert!(matches!(&content[0], ContentBlock::Text(text) if text == "describe this"));
    let ContentBlock::Image(image) = &content[1] else {
        panic!(
            "expected ContentBlock::Image for image input, got {:?}",
            content[1]
        );
    };
    assert_eq!(image.format(), &ImageFormat::Png);
    let source = image.source().expect("image source");
    let blob = source.as_bytes().expect("Bytes source");
    assert_eq!(blob.as_ref(), bytes.as_ref());
}

#[test]
fn conversation_messages_reject_unknown_image_mime() {
    let bytes: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4]);
    let err = conversation_messages(
        &[LlmInputItem::Image {
            media_type: "image/avif".to_string(),
            bytes,
        }],
        false,
    )
    .expect_err("unsupported MIME must surface an explicit ProviderRequest error");
    let message = err.to_string();
    assert!(
        message.contains("image/avif"),
        "error must mention the unsupported MIME: {message}"
    );
}
