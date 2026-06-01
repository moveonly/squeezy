use std::collections::BTreeMap;
use std::sync::Arc;

use aws_config::{BehaviorVersion, SdkConfig};
use aws_sdk_bedrockruntime::config::{Credentials, Region, SharedCredentialsProvider};
use aws_sdk_bedrockruntime::types::{
    CachePointType, CacheTtl, ContentBlock, ConversationRole, ConverseStreamMetadataEvent,
    ConverseStreamOutput, MessageStopEvent, StopReason, SystemContentBlock, TokenUsage,
    ToolInputSchema,
};
use aws_smithy_types::{Document, Number};
use serde_json::json;
use squeezy_core::SqueezyError;

use aws_sdk_bedrockruntime::operation::converse_stream::builders::ConverseStreamInputBuilder;
use squeezy_core::{BedrockConfig, ProviderTransportConfig};

use super::{
    BedrockProvider, BedrockStreamState, BreakpointBudget, apply_inference_profile_prefix,
    apply_thinking_extra_fields, bedrock_document_block, bedrock_effort_label,
    bedrock_request_metadata_map, bedrock_tool_choice, build_bedrock_client,
    compute_thinking_extra_fields, conversation_messages, current_bearer_token,
    handle_bedrock_event, inference_configuration, json_to_document, region_prefix,
    sanitize_bedrock_document_name, system_blocks, tool_configuration,
};
use crate::anthropic_betas::bedrock_extra_body_betas;
use crate::{CacheRetention, LlmInputItem, LlmRequest, LlmToolSpec};

#[test]
fn system_blocks_skip_blank_instructions() {
    assert!(
        system_blocks("", CacheRetention::None)
            .expect("ok")
            .is_empty()
    );
    assert!(
        system_blocks("   \n  ", CacheRetention::None)
            .expect("ok")
            .is_empty()
    );
    assert!(
        system_blocks("", CacheRetention::Short)
            .expect("ok")
            .is_empty()
    );
}

#[test]
fn system_blocks_emit_single_text_block() {
    let blocks = system_blocks("be helpful", CacheRetention::None).expect("ok");
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        SystemContentBlock::Text(text) => assert_eq!(text, "be helpful"),
        _ => panic!("expected Text system block"),
    }
}

#[test]
fn system_blocks_append_cache_point_when_caching_enabled() {
    let blocks = system_blocks("be helpful", CacheRetention::Short).expect("ok");
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], SystemContentBlock::Text(text) if text == "be helpful"));
    let SystemContentBlock::CachePoint(cache_point) = &blocks[1] else {
        panic!("expected CachePoint after system text, got {:?}", blocks[1]);
    };
    assert_eq!(*cache_point.r#type(), CachePointType::Default);
    // CacheRetention::Short omits ttl so Bedrock applies its 5-minute
    // default — the cross-provider knob for the historical behavior.
    assert!(
        cache_point.ttl().is_none(),
        "Short retention must not set ttl; got {:?}",
        cache_point.ttl(),
    );
}

#[test]
fn system_blocks_cache_point_carries_one_hour_ttl_for_long_retention() {
    // CacheRetention::Long must surface as `ttl: 1h` on the Bedrock
    // cache point so cross-provider Long retention actually amortizes
    // the cache write across multi-hour runs. Without the setter the
    // breakpoint silently degrades to the 5-minute default.
    let blocks = system_blocks("be helpful", CacheRetention::Long).expect("ok");
    let SystemContentBlock::CachePoint(cache_point) = &blocks[1] else {
        panic!("expected CachePoint after system text, got {:?}", blocks[1]);
    };
    assert_eq!(cache_point.ttl(), Some(&CacheTtl::OneHour));
}

#[test]
fn conversation_messages_merge_consecutive_user_turns() {
    let messages = conversation_messages(
        &[
            LlmInputItem::UserText("hello".to_string()),
            LlmInputItem::UserText("again".to_string()),
            LlmInputItem::AssistantText("hi".to_string()),
        ],
        CacheRetention::None,
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
                content_parts: None,
                is_error: false,
            },
        ],
        CacheRetention::None,
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
        CacheRetention::Short,
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
    let messages = conversation_messages(
        &[LlmInputItem::AssistantText("solo".to_string())],
        CacheRetention::Short,
    )
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
fn conversation_messages_cache_point_carries_one_hour_ttl_for_long() {
    // Cache markers on the last user message and tools tail must also
    // carry the 1-hour TTL when the caller asked for Long retention.
    // The TTL is what differentiates the cross-provider Long band from
    // the historical 5-minute default — without it the cache write
    // gets billed but the read pays full price every 5 minutes.
    let messages = conversation_messages(
        &[LlmInputItem::UserText("first".to_string())],
        CacheRetention::Long,
    )
    .expect("build messages");
    let final_user = messages.last().expect("at least one message");
    let ContentBlock::CachePoint(cache_point) = final_user
        .content()
        .last()
        .expect("user message must have content")
    else {
        panic!(
            "expected trailing CachePoint on final user message, got {:?}",
            final_user.content().last()
        );
    };
    assert_eq!(cache_point.ttl(), Some(&CacheTtl::OneHour));
}

#[test]
fn tool_configuration_cache_point_carries_one_hour_ttl_for_long() {
    let specs: Vec<Arc<LlmToolSpec>> = vec![
        LlmToolSpec {
            name: "search".to_string(),
            description: "Web search".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
    ];
    let config = tool_configuration(&specs, CacheRetention::Long, None)
        .expect("ok")
        .expect("present");
    let tools = config.tools();
    let aws_sdk_bedrockruntime::types::Tool::CachePoint(cache_point) = &tools[1] else {
        panic!("expected CachePoint after tool spec, got {:?}", tools[1]);
    };
    assert_eq!(cache_point.ttl(), Some(&CacheTtl::OneHour));
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
    let config = tool_configuration(&specs, CacheRetention::None, None)
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
    let config = tool_configuration(&specs, CacheRetention::Short, None)
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
    assert!(
        tool_configuration(&[], CacheRetention::None, None)
            .expect("ok")
            .is_none()
    );
    assert!(
        tool_configuration(&[], CacheRetention::Short, None)
            .expect("ok")
            .is_none(),
        "no tools means no tool config, even when caching is requested"
    );
}

#[test]
fn tool_configuration_cache_point_skips_mcp_prefixed_tools() {
    let specs: Vec<Arc<LlmToolSpec>> = vec![
        LlmToolSpec {
            name: "search".to_string(),
            description: "Web search".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
        LlmToolSpec {
            name: "mcp__example".to_string(),
            description: "Dynamic MCP tool".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
    ];
    let config = tool_configuration(&specs, CacheRetention::Short, None)
        .expect("ok")
        .expect("present");
    let tools = config.tools();
    assert_eq!(tools.len(), 3, "two specs plus cache point");
    assert!(matches!(
        &tools[0],
        aws_sdk_bedrockruntime::types::Tool::ToolSpec(spec) if spec.name() == "search"
    ));
    let aws_sdk_bedrockruntime::types::Tool::CachePoint(cache_point) = &tools[1] else {
        panic!(
            "expected CachePoint between stable tool and mcp__ tool, got {:?}",
            tools[1]
        );
    };
    assert_eq!(*cache_point.r#type(), CachePointType::Default);
    assert!(matches!(
        &tools[2],
        aws_sdk_bedrockruntime::types::Tool::ToolSpec(spec) if spec.name() == "mcp__example"
    ));

    let all_mcp: Vec<Arc<LlmToolSpec>> = vec![
        LlmToolSpec {
            name: "mcp__one".to_string(),
            description: "MCP one".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
        LlmToolSpec {
            name: "mcp__two".to_string(),
            description: "MCP two".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
    ];
    let config = tool_configuration(&all_mcp, CacheRetention::Short, None)
        .expect("ok")
        .expect("present");
    let tools = config.tools();
    assert_eq!(tools.len(), 2, "all-mcp list yields no cache point");
    for tool in tools {
        assert!(
            !matches!(tool, aws_sdk_bedrockruntime::types::Tool::CachePoint(_)),
            "no CachePoint should be emitted when every tool is mcp__-prefixed"
        );
    }
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
        CacheRetention::None,
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
fn config_request_metadata_appears_on_converse_stream_input() {
    // Cost-allocation tags supplied on `BedrockConfig.request_metadata`
    // must round-trip through `BedrockProvider` and land on
    // `ConverseStreamInput.request_metadata` so AWS billing can group
    // invocations by the operator's chosen labels. Empty maps stay
    // unset on the wire (the helper returns `None`) so we keep the
    // request payload minimal when no tags are configured.
    let mut tags = BTreeMap::new();
    tags.insert("team".to_string(), "platform".to_string());
    tags.insert("env".to_string(), "prod".to_string());

    let provider = BedrockProvider::from_config(&BedrockConfig {
        region: "us-east-1".to_string(),
        base_url: None,
        bearer_token: None,
        request_metadata: tags.clone(),
        transport: ProviderTransportConfig::default(),
    })
    .expect("provider builds from config with request_metadata");

    let sdk_map = bedrock_request_metadata_map(&provider.request_metadata)
        .expect("non-empty config tags must produce a HashMap, not None");
    let input = ConverseStreamInputBuilder::default()
        .model_id("test-model")
        .set_request_metadata(Some(sdk_map))
        .build()
        .expect("ConverseStreamInputBuilder::build is infallible for valid inputs");

    let echoed = input
        .request_metadata()
        .expect("request_metadata must be set on the input");
    assert_eq!(echoed.len(), 2);
    assert_eq!(echoed.get("team").map(String::as_str), Some("platform"));
    assert_eq!(echoed.get("env").map(String::as_str), Some("prod"));

    assert!(
        bedrock_request_metadata_map(&BTreeMap::new()).is_none(),
        "empty config tags must skip the field entirely so unconfigured callers don't ship an empty object"
    );
}

#[test]
fn inference_configuration_skipped_when_all_defaults() {
    // No sampling knobs set => omit the field entirely. The Converse
    // API treats absent and empty equivalently but skipping it keeps
    // the wire payload minimal and makes the "all defaults" case
    // observable in request logs.
    let request = LlmRequest::default();
    assert!(
        inference_configuration(&request).is_none(),
        "default request must not ship an InferenceConfiguration",
    );
}

#[test]
fn inference_configuration_maps_max_tokens_temperature_top_p_stop() {
    // max_output_tokens, temperature, top_p, and stop must all round-
    // trip from LlmRequest into the SDK builder so the Converse API
    // sees the caller's bounded reply window, deterministic sampling,
    // and explicit halt strings instead of the model's vendor defaults.
    let request = LlmRequest {
        max_output_tokens: Some(4096),
        temperature: Some(0.0),
        top_p: Some(0.85),
        stop: vec!["END".to_string(), "STOP".to_string()],
        ..LlmRequest::default()
    };

    let inference = inference_configuration(&request).expect("expected configuration");
    let input = ConverseStreamInputBuilder::default()
        .model_id("test-model")
        .inference_config(inference)
        .build()
        .expect("ConverseStreamInputBuilder::build is infallible for valid inputs");
    let echoed = input
        .inference_config()
        .expect("inference_config must round-trip");
    assert_eq!(echoed.max_tokens(), Some(4096));
    assert_eq!(echoed.temperature(), Some(0.0));
    assert_eq!(echoed.top_p(), Some(0.85));
    assert_eq!(echoed.stop_sequences(), &["END", "STOP"]);
}

#[test]
fn inference_configuration_clamps_max_tokens_overflow() {
    // u32::MAX exceeds i32::MAX; the helper must clamp rather than
    // wrap or panic so a configuration mistake doesn't take the
    // request down on the i32 boundary.
    let request = LlmRequest {
        max_output_tokens: Some(u32::MAX),
        ..LlmRequest::default()
    };
    let inference = inference_configuration(&request).expect("expected configuration");
    let input = ConverseStreamInputBuilder::default()
        .model_id("test-model")
        .inference_config(inference)
        .build()
        .expect("build");
    assert_eq!(
        input
            .inference_config()
            .expect("inference_config must round-trip")
            .max_tokens(),
        Some(i32::MAX),
    );
}

#[test]
fn conversation_messages_reject_unknown_image_mime() {
    let bytes: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4]);
    let err = conversation_messages(
        &[LlmInputItem::Image {
            media_type: "image/avif".to_string(),
            bytes,
        }],
        CacheRetention::None,
    )
    .expect_err("unsupported MIME must surface an explicit ProviderRequest error");
    let message = err.to_string();
    assert!(
        message.contains("image/avif"),
        "error must mention the unsupported MIME: {message}"
    );
}

#[test]
fn aws_bearer_token_env_routes_through_bedrock_bearer_auth() {
    // BedrockConfig carries the `AWS_BEARER_TOKEN_BEDROCK` env-var
    // value once squeezy-core has lifted it (squeezy-core owns env
    // discovery; squeezy-llm owns the auth wiring). With a bearer
    // token in hand the provider must build a Bedrock client even
    // when the shared SdkConfig has no SigV4 credentials at all —
    // that is the whole point of the bearer route. We deliberately
    // build a credential-less SdkConfig so a regression that ignores
    // the bearer token would surface as the explicit
    // `ProviderNotConfigured` error rather than an Ok client.
    let bedrock = squeezy_core::BedrockConfig {
        region: squeezy_core::DEFAULT_BEDROCK_REGION.to_string(),
        base_url: None,
        bearer_token: Some("bedrock-api-key-test".to_string()),
        request_metadata: BTreeMap::new(),
        transport: squeezy_core::ProviderTransportConfig::default(),
    };
    let provider = crate::BedrockProvider::from_config(&bedrock)
        .expect("BedrockProvider::from_config must accept a bearer-token-only config");
    drop(provider);

    let shared = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .build();
    assert!(
        shared.credentials_provider().is_none(),
        "test SdkConfig is intentionally credential-less",
    );
    let client = build_bedrock_client(&shared, bedrock.bearer_token.as_deref())
        .expect("bearer-token path must not require AWS credentials");
    drop(client);

    // Leading/trailing whitespace from a shell heredoc must not
    // poison the bearer header — `build_bedrock_client` trims before
    // wrapping the token in the SDK identity type. An all-whitespace
    // bearer is treated as "missing token" and surfaces a clean error
    // instead of an unauthenticated request.
    let trimmed = build_bedrock_client(&shared, Some("  bedrock-api-key-test\n"))
        .expect("whitespace-padded bearer token must be normalised, not rejected");
    drop(trimmed);
    let empty = build_bedrock_client(&shared, Some("   "))
        .expect_err("an all-whitespace bearer token must be rejected explicitly");
    assert!(
        matches!(&empty, SqueezyError::ProviderNotConfigured(message)
            if message.contains("AWS_BEARER_TOKEN_BEDROCK")),
        "empty bearer must surface ProviderNotConfigured mentioning the env var: {empty:?}",
    );
}

#[test]
fn missing_bearer_token_falls_back_to_default_credential_chain() {
    // Without a bearer token the provider trusts whatever the AWS
    // default credential chain resolved (env → ~/.aws/credentials →
    // IMDS / container roles). We exercise both legs of that branch:
    //
    // 1. A credential-bearing SdkConfig must yield Ok(client) so the
    //    SDK can sign with SigV4 the way it always has.
    // 2. A credential-less SdkConfig must surface ProviderNotConfigured
    //    with a message that points operators at the recovery paths
    //    (bearer token env, `aws configure`, AWS_PROFILE, raw env
    //    vars) — silently returning an unusable client would mask the
    //    misconfiguration until the first network request.
    let creds = Credentials::new("AKIDEXAMPLE", "SECRETEXAMPLE", None, None, "squeezy-test");
    let with_creds = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(SharedCredentialsProvider::new(creds))
        .build();
    let client = build_bedrock_client(&with_creds, None)
        .expect("default chain with credentials must build a client");
    drop(client);

    let without_creds = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .build();
    let err = build_bedrock_client(&without_creds, None)
        .expect_err("default chain with no credentials must surface ProviderNotConfigured");
    let SqueezyError::ProviderNotConfigured(message) = &err else {
        panic!("expected ProviderNotConfigured, got {err:?}");
    };
    assert!(
        message.contains("AWS_BEARER_TOKEN_BEDROCK"),
        "error must point operators at the bearer-token recovery path: {message}",
    );
    assert!(
        message.contains("aws configure") || message.contains("AWS_PROFILE"),
        "error must also point at the standard credential-chain recovery paths: {message}",
    );
}

#[test]
fn region_prefix_maps_known_aws_regions() {
    // Static prefix list from AWS docs: us-* → us, eu-* → eu,
    // ap-* → apac, jp-* → jp. GovCloud / future regions return None
    // so the caller passes the model id through verbatim.
    assert_eq!(region_prefix("us-east-1"), Some("us"));
    assert_eq!(region_prefix("us-west-2"), Some("us"));
    assert_eq!(region_prefix("eu-central-1"), Some("eu"));
    assert_eq!(region_prefix("ap-southeast-2"), Some("apac"));
    assert_eq!(region_prefix("jp-east-1"), Some("jp"));
    // Mixed case must canonicalise; AWS region ids ship lowercase but
    // defensive parsing keeps the contract well-defined.
    assert_eq!(region_prefix("US-EAST-1"), Some("us"));
    // Unknown / GovCloud / non-region strings fall through to None so
    // the caller pre-prefixes (or accepts the upstream rejection) on
    // their own.
    assert!(region_prefix("us-gov-west-1").is_none());
    assert!(region_prefix("local").is_none());
    assert!(region_prefix("").is_none());
}

#[test]
fn apply_inference_profile_prefix_rewrites_bare_anthropic_models() {
    // Newer Anthropic Claude families on Bedrock require a cross-
    // region inference profile and reject the bare `anthropic.claude-*`
    // id with `ValidationException`. The helper must rewrite to the
    // configured region's prefix so on-demand throughput failures
    // surface as the actually-resolved id and operators don't have to
    // pre-prefix every model in their config.
    assert_eq!(
        apply_inference_profile_prefix("anthropic.claude-sonnet-4-6-20251001-v1:0", "us-east-1",),
        "us.anthropic.claude-sonnet-4-6-20251001-v1:0",
    );
    assert_eq!(
        apply_inference_profile_prefix("anthropic.claude-opus-4-6-20251101-v1:0", "eu-central-1",),
        "eu.anthropic.claude-opus-4-6-20251101-v1:0",
    );
    assert_eq!(
        apply_inference_profile_prefix(
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            "ap-southeast-2",
        ),
        "apac.anthropic.claude-haiku-4-5-20251001-v1:0",
    );
}

#[test]
fn apply_inference_profile_prefix_passes_already_prefixed_ids_through() {
    // Operators who already opted in must not see double prefixes.
    // `us.`, `eu.`, `apac.`, `jp.`, and `global.` round-trip verbatim.
    for prefixed in [
        "us.anthropic.claude-sonnet-4-6-20251001-v1:0",
        "eu.anthropic.claude-opus-4-6-20251101-v1:0",
        "apac.anthropic.claude-haiku-4-5-20251001-v1:0",
        "jp.anthropic.claude-sonnet-4-6-20251001-v1:0",
        "global.anthropic.claude-sonnet-4-6-20251001-v1:0",
    ] {
        assert_eq!(
            apply_inference_profile_prefix(prefixed, "us-east-1"),
            prefixed,
            "already-prefixed id `{prefixed}` must pass through verbatim",
        );
    }
}

#[test]
fn apply_inference_profile_prefix_passes_arns_through() {
    // ARNs (inference-profile or application-inference-profile)
    // carry their own routing and must not be touched.
    let arn = "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-4-6-20251001-v1:0";
    assert_eq!(apply_inference_profile_prefix(arn, "us-east-1"), arn);
    let app_arn =
        "arn:aws:bedrock:eu-central-1:123456789012:application-inference-profile/my-profile";
    assert_eq!(
        apply_inference_profile_prefix(app_arn, "eu-central-1"),
        app_arn
    );
}

#[test]
fn apply_inference_profile_prefix_skips_non_anthropic_vendors() {
    // Mistral / Cohere / Amazon Titan ship as on-demand throughput
    // without inference profiles. The helper must not silently
    // route them through Anthropic's cross-region prefix.
    assert_eq!(
        apply_inference_profile_prefix("amazon.titan-text-express-v1", "us-east-1"),
        "amazon.titan-text-express-v1",
    );
    assert_eq!(
        apply_inference_profile_prefix("mistral.mistral-large-2407-v1:0", "us-east-1"),
        "mistral.mistral-large-2407-v1:0",
    );
}

#[test]
fn apply_inference_profile_prefix_falls_back_on_unmapped_region() {
    // GovCloud / future regions with no defined prefix pass the id
    // through verbatim — the upstream rejection points the operator at
    // their model/region mismatch instead of squeezy silently routing
    // through a wrong prefix.
    assert_eq!(
        apply_inference_profile_prefix(
            "anthropic.claude-sonnet-4-6-20251001-v1:0",
            "us-gov-west-1",
        ),
        "anthropic.claude-sonnet-4-6-20251001-v1:0",
    );
}

#[test]
fn bedrock_effort_label_maps_each_variant() {
    use squeezy_core::ReasoningEffort;
    // The label set must match Anthropic's adaptive-thinking surface
    // exactly so the Bedrock and Anthropic-native paths agree on
    // `output_config.effort`.
    assert_eq!(bedrock_effort_label(ReasoningEffort::Low), "low");
    assert_eq!(bedrock_effort_label(ReasoningEffort::Medium), "medium");
    assert_eq!(bedrock_effort_label(ReasoningEffort::High), "high");
    assert_eq!(bedrock_effort_label(ReasoningEffort::XHigh), "max");
}

#[test]
fn compute_thinking_extra_fields_emits_adaptive_for_claude_4_6_plus() {
    // Adaptive-thinking opus/sonnet 4.6+ reject the bare
    // `enabled+budget_tokens` form on Bedrock; the helper must emit
    // `thinking={type:adaptive}` + `output_config={effort:...}` so the
    // request reaches the model in the schema it expects.
    use squeezy_core::ReasoningEffort;
    let mut fields = std::collections::HashMap::new();
    compute_thinking_extra_fields(
        &mut fields,
        "anthropic.claude-opus-4-6-20251101-v1:0",
        ReasoningEffort::High,
        32_768,
    );
    let Document::Object(thinking) = fields.get("thinking").expect("thinking emitted") else {
        panic!("expected Document::Object for thinking");
    };
    assert!(matches!(
        thinking.get("type"),
        Some(Document::String(s)) if s == "adaptive"
    ));
    assert!(
        !thinking.contains_key("budget_tokens"),
        "adaptive shape must not carry budget_tokens; got {:?}",
        thinking,
    );
    let Document::Object(output_config) =
        fields.get("output_config").expect("output_config emitted")
    else {
        panic!("expected Document::Object for output_config");
    };
    assert!(matches!(
        output_config.get("effort"),
        Some(Document::String(s)) if s == "high"
    ));

    // Sonnet 4.6 follows the same family rule.
    let mut sonnet_fields = std::collections::HashMap::new();
    compute_thinking_extra_fields(
        &mut sonnet_fields,
        "us.anthropic.claude-sonnet-4-6-20251001-v1:0",
        ReasoningEffort::Low,
        32_768,
    );
    assert!(sonnet_fields.contains_key("output_config"));
    let Document::Object(thinking) = sonnet_fields.get("thinking").unwrap() else {
        panic!("expected Document::Object");
    };
    assert!(matches!(
        thinking.get("type"),
        Some(Document::String(s)) if s == "adaptive"
    ));
}

#[test]
fn compute_thinking_extra_fields_emits_enabled_budget_for_pre_4_6_claude() {
    // Pre-4.6 Claude (3.7 sonnet, opus 4.0/4.5, haiku 4.5) must keep
    // the `enabled+budget_tokens` shape and must not carry the
    // `output_config` block.
    use squeezy_core::ReasoningEffort;
    let mut fields = std::collections::HashMap::new();
    compute_thinking_extra_fields(
        &mut fields,
        "anthropic.claude-haiku-4-5-20251001-v1:0",
        ReasoningEffort::Medium,
        32_768,
    );
    let Document::Object(thinking) = fields.get("thinking").expect("thinking emitted") else {
        panic!("expected Document::Object for thinking");
    };
    assert!(matches!(
        thinking.get("type"),
        Some(Document::String(s)) if s == "enabled"
    ));
    assert!(
        matches!(thinking.get("budget_tokens"), Some(Document::Number(_))),
        "non-adaptive shape must carry budget_tokens; got {:?}",
        thinking,
    );
    assert!(
        !fields.contains_key("output_config"),
        "non-adaptive shape must not carry output_config",
    );
}

#[test]
fn compute_thinking_extra_fields_skips_when_max_tokens_too_small() {
    // The pre-4.6 path enforces `max_tokens > budget_tokens + 1024`.
    // When the configured `max_output_tokens` cannot satisfy both the
    // 1024 budget floor and the 1024 reply headroom, skip emission so
    // every turn doesn't 400.
    use squeezy_core::ReasoningEffort;
    let mut fields = std::collections::HashMap::new();
    compute_thinking_extra_fields(
        &mut fields,
        "anthropic.claude-haiku-4-5-20251001-v1:0",
        ReasoningEffort::Low,
        1024, // 1024 - 1024 headroom = 0 < 1024 min budget
    );
    assert!(
        fields.is_empty(),
        "thinking must be skipped when max_tokens is too small; got {:?}",
        fields,
    );
}

#[test]
fn apply_thinking_extra_fields_no_emit_without_reasoning_effort() {
    // The outer helper is gated by `request.reasoning_effort.is_some()`
    // so legacy callers that never opt in keep the historical
    // no-thinking behavior.
    let mut fields = std::collections::HashMap::new();
    let request = LlmRequest::default();
    apply_thinking_extra_fields(
        &mut fields,
        &request,
        "anthropic.claude-haiku-4-5-20251001-v1:0",
    );
    assert!(
        fields.is_empty(),
        "no reasoning_effort means no thinking emission; got {:?}",
        fields,
    );
}

// `current_bearer_token` reads a real environment variable. Tests
// that set/clear `AWS_BEARER_TOKEN_BEDROCK` must execute serially
// because Rust runs tests in parallel by default and env mutation is
// process-global. Each test guards itself with a single mutex to
// prevent racing with a sibling test that observes the variable.
static BEARER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn current_bearer_token_prefers_live_env_var_over_fallback() {
    let _guard = BEARER_ENV_LOCK.lock().unwrap();
    let original = std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok();
    // SAFETY: lock taken above + restore on drop below; single-process tests.
    unsafe {
        std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", "rotated-token-12345");
    }
    let resolved = current_bearer_token(Some("config-time-fallback"));
    assert_eq!(
        resolved.as_deref(),
        Some("rotated-token-12345"),
        "env wins so the live shell can rotate without rebuilding the provider",
    );
    // Whitespace must be trimmed so a shell heredoc doesn't poison
    // the bearer header.
    unsafe {
        std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", "   padded-token\n");
    }
    assert_eq!(
        current_bearer_token(Some("config-fallback")).as_deref(),
        Some("padded-token"),
    );
    match original {
        Some(value) => unsafe { std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", value) },
        None => unsafe { std::env::remove_var("AWS_BEARER_TOKEN_BEDROCK") },
    }
}

#[test]
fn current_bearer_token_falls_back_when_env_unset_or_blank() {
    let _guard = BEARER_ENV_LOCK.lock().unwrap();
    let original = std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok();
    // Unset env -> use fallback (config-time value).
    unsafe {
        std::env::remove_var("AWS_BEARER_TOKEN_BEDROCK");
    }
    assert_eq!(
        current_bearer_token(Some("config-value")).as_deref(),
        Some("config-value"),
    );
    assert!(
        current_bearer_token(None).is_none(),
        "no env and no fallback means no bearer (caller falls back to SigV4)",
    );
    // Blank/whitespace env -> treat as "unset" and use fallback.
    unsafe {
        std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", "   ");
    }
    assert_eq!(
        current_bearer_token(Some("config-value")).as_deref(),
        Some("config-value"),
        "blank env must not poison the bearer header",
    );
    match original {
        Some(value) => unsafe { std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", value) },
        None => unsafe { std::env::remove_var("AWS_BEARER_TOKEN_BEDROCK") },
    }
}

#[test]
fn apply_thinking_extra_fields_no_emit_when_capabilities_missing() {
    // A model id not in the registry returns `None` from
    // `capabilities_for`, so the helper short-circuits without
    // emitting thinking. Protects non-Anthropic vendors (mistral,
    // titan) from getting Anthropic-shaped thinking blocks.
    use squeezy_core::ReasoningEffort;
    let mut fields = std::collections::HashMap::new();
    let request = LlmRequest {
        reasoning_effort: Some(ReasoningEffort::High),
        ..LlmRequest::default()
    };
    apply_thinking_extra_fields(&mut fields, &request, "amazon.titan-text-express-v1");
    assert!(
        fields.is_empty(),
        "unregistered model must not get thinking; got {:?}",
        fields,
    );
}

#[test]
fn bedrock_tool_choice_maps_none_and_empty_to_none() {
    // Unset / empty / whitespace `tool_choice` must omit the field so
    // the provider applies its default (typically `auto`). Matches
    // squeezy's historical behavior before the X-04 plumbing.
    assert!(bedrock_tool_choice(None).expect("ok").is_none());
    assert!(bedrock_tool_choice(Some("")).expect("ok").is_none());
    assert!(bedrock_tool_choice(Some("   ")).expect("ok").is_none());
}

#[test]
fn bedrock_tool_choice_maps_auto_to_auto_variant() {
    // The OpenAI-style `auto` literal must land as
    // `ToolChoice::Auto(...)` so callers using the cross-provider
    // surface keep parity with Anthropic / OpenAI behavior.
    use aws_sdk_bedrockruntime::types::ToolChoice;
    let choice = bedrock_tool_choice(Some("auto"))
        .expect("ok")
        .expect("present");
    assert!(matches!(choice, ToolChoice::Auto(_)));
    // Case-insensitive — operators sometimes upper-case literals.
    let choice = bedrock_tool_choice(Some("AUTO"))
        .expect("ok")
        .expect("present");
    assert!(matches!(choice, ToolChoice::Auto(_)));
}

#[test]
fn bedrock_tool_choice_maps_required_and_any_to_any_variant() {
    // Tool-shy models (Mistral / Nova) benefit from `Any` so the
    // model MUST emit a tool call instead of free-form text. Both the
    // OpenAI literal `required` and Bedrock's literal `any` route to
    // the same variant.
    use aws_sdk_bedrockruntime::types::ToolChoice;
    let choice = bedrock_tool_choice(Some("required"))
        .expect("ok")
        .expect("present");
    assert!(matches!(choice, ToolChoice::Any(_)));
    let choice = bedrock_tool_choice(Some("any"))
        .expect("ok")
        .expect("present");
    assert!(matches!(choice, ToolChoice::Any(_)));
}

#[test]
fn bedrock_tool_choice_maps_specific_tool_name() {
    // Any other non-empty value names a specific tool the agent has
    // already advertised. Strips an optional `tool:` prefix so callers
    // using the OpenAI Responses convention can route through without
    // a separate code path.
    use aws_sdk_bedrockruntime::types::ToolChoice;
    let choice = bedrock_tool_choice(Some("search"))
        .expect("ok")
        .expect("present");
    let ToolChoice::Tool(specific) = choice else {
        panic!("expected ToolChoice::Tool");
    };
    assert_eq!(specific.name(), "search");

    let prefixed = bedrock_tool_choice(Some("tool:search"))
        .expect("ok")
        .expect("present");
    let ToolChoice::Tool(specific) = prefixed else {
        panic!("expected ToolChoice::Tool");
    };
    assert_eq!(specific.name(), "search");

    // An empty name after the prefix degrades to None instead of
    // failing the request — the caller asked for "no specific tool"
    // by passing only the prefix.
    assert!(bedrock_tool_choice(Some("tool:")).expect("ok").is_none());
}

#[test]
fn tool_configuration_forwards_tool_choice() {
    let specs: Vec<Arc<LlmToolSpec>> = vec![
        LlmToolSpec {
            name: "search".to_string(),
            description: "Web search".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
    ];
    let config = tool_configuration(&specs, CacheRetention::None, Some("required"))
        .expect("ok")
        .expect("present");
    let choice = config.tool_choice().expect("tool_choice must round-trip");
    assert!(matches!(
        choice,
        aws_sdk_bedrockruntime::types::ToolChoice::Any(_)
    ));
}

#[test]
fn bedrock_document_block_round_trips_pdf_bytes() {
    use aws_sdk_bedrockruntime::types::DocumentFormat;
    let bytes: Arc<[u8]> = Arc::from(b"%PDF-1.4 fake".to_vec());
    let block = bedrock_document_block("application/pdf", "report.pdf", &bytes)
        .expect("pdf must round-trip");
    assert_eq!(block.format(), &DocumentFormat::Pdf);
    // The Bedrock name allow-list rejects `.` so `report.pdf` becomes
    // `report-pdf`; the sanitizer keeps the caller's intent visible.
    assert_eq!(block.name(), "report-pdf");
    let source = block.source().expect("source must be set");
    let blob = source.as_bytes().expect("Bytes source");
    assert_eq!(blob.as_ref(), bytes.as_ref());
}

#[test]
fn bedrock_document_block_maps_each_supported_mime() {
    use aws_sdk_bedrockruntime::types::DocumentFormat;
    let bytes: Arc<[u8]> = Arc::from(vec![1u8, 2, 3]);
    for (mime, expected) in [
        ("application/pdf", DocumentFormat::Pdf),
        ("application/x-pdf", DocumentFormat::Pdf),
        ("text/csv", DocumentFormat::Csv),
        ("application/csv", DocumentFormat::Csv),
        ("application/msword", DocumentFormat::Doc),
        (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            DocumentFormat::Docx,
        ),
        ("application/vnd.ms-excel", DocumentFormat::Xls),
        (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            DocumentFormat::Xlsx,
        ),
        ("text/html", DocumentFormat::Html),
        ("application/xhtml+xml", DocumentFormat::Html),
        ("text/markdown", DocumentFormat::Md),
        ("text/x-markdown", DocumentFormat::Md),
        ("text/plain", DocumentFormat::Txt),
    ] {
        let block = bedrock_document_block(mime, "doc", &bytes)
            .unwrap_or_else(|err| panic!("{mime} must map: {err}"));
        assert_eq!(block.format(), &expected, "wrong format for {mime}");
    }
}

#[test]
fn bedrock_document_block_rejects_unknown_mime() {
    let bytes: Arc<[u8]> = Arc::from(vec![1u8]);
    let err = bedrock_document_block("application/zip", "doc", &bytes)
        .expect_err("unsupported MIME must surface an explicit ProviderRequest error");
    assert!(
        err.to_string().contains("application/zip"),
        "error must mention the unsupported MIME: {err}",
    );
}

#[test]
fn sanitize_bedrock_document_name_canonicalises_input() {
    // Allow-list: alphanumerics + single space + hyphen + ()/[]. Runs
    // of disallowed characters collapse to a single hyphen so the
    // resulting name still resembles the caller's intent.
    assert_eq!(
        sanitize_bedrock_document_name("/tmp/foo bar.pdf"),
        "tmp-foo bar-pdf",
    );
    // Multi-space collapses to a single space.
    assert_eq!(
        sanitize_bedrock_document_name("report   draft"),
        "report draft",
    );
    // Surrounding whitespace and runs of disallowed characters trim.
    assert_eq!(sanitize_bedrock_document_name("  ---@@@  "), "document");
    // Empty input gets a safe default name so the request can still
    // ship.
    assert_eq!(sanitize_bedrock_document_name(""), "document");
    // Allowed characters round-trip verbatim.
    assert_eq!(
        sanitize_bedrock_document_name("section-1 (draft) [v2]"),
        "section-1 (draft) [v2]",
    );
}

#[test]
fn conversation_messages_emit_document_content_block() {
    use aws_sdk_bedrockruntime::types::DocumentFormat;
    let bytes: Arc<[u8]> = Arc::from(b"%PDF-1.4 fake".to_vec());
    let messages = conversation_messages(
        &[
            LlmInputItem::UserText("here's the report".to_string()),
            LlmInputItem::Document {
                media_type: "application/pdf".to_string(),
                name: "Q4 forecast.pdf".to_string(),
                bytes: bytes.clone(),
            },
        ],
        CacheRetention::None,
    )
    .expect("build messages");

    // User text + document coalesce into a single user message with
    // two content blocks so the Converse API sees them as one turn.
    assert_eq!(messages.len(), 1);
    assert_eq!(*messages[0].role(), ConversationRole::User);
    let content = messages[0].content();
    assert_eq!(content.len(), 2);
    assert!(matches!(&content[0], ContentBlock::Text(text) if text == "here's the report"));
    let ContentBlock::Document(document) = &content[1] else {
        panic!(
            "expected ContentBlock::Document for document input, got {:?}",
            content[1]
        );
    };
    assert_eq!(document.format(), &DocumentFormat::Pdf);
    assert_eq!(document.name(), "Q4 forecast-pdf");
    let source = document.source().expect("document source");
    let blob = source.as_bytes().expect("Bytes source");
    assert_eq!(blob.as_ref(), bytes.as_ref());
}

#[test]
fn conversation_messages_reject_unknown_document_mime() {
    let bytes: Arc<[u8]> = Arc::from(vec![1u8, 2]);
    let err = conversation_messages(
        &[LlmInputItem::Document {
            media_type: "application/octet-stream".to_string(),
            name: "binary".to_string(),
            bytes,
        }],
        CacheRetention::None,
    )
    .expect_err("unsupported document MIME must surface an explicit ProviderRequest error");
    assert!(
        err.to_string().contains("application/octet-stream"),
        "error must mention the unsupported MIME: {err}",
    );
}

#[test]
fn bedrock_image_block_rejects_images_over_3_75_mib() {
    // Bedrock rejects Claude images larger than 3.75 MB with
    // `ValidationException` and an opaque message. The local guard
    // surfaces a structured error pointing the operator at the
    // offending image instead of letting the AWS SDK error propagate.
    let oversized: Arc<[u8]> = Arc::from(vec![0u8; 3_932_161]);
    let err = super::bedrock_image_block("image/png", &oversized)
        .expect_err("oversized image must surface an explicit ProviderRequest error");
    let message = err.to_string();
    assert!(
        message.contains("3932160"),
        "error must mention the byte limit; got `{message}`",
    );
    assert!(
        message.contains("3932161"),
        "error must mention the actual byte size; got `{message}`",
    );
}

#[test]
fn bedrock_image_block_accepts_image_at_3_75_mib_boundary() {
    // The boundary itself is allowed — `>` not `>=` — so an image that
    // is exactly the documented cap still ships.
    let on_boundary: Arc<[u8]> = Arc::from(vec![0u8; 3_932_160]);
    super::bedrock_image_block("image/png", &on_boundary)
        .expect("boundary-size image must build cleanly");
}

#[test]
fn handle_bedrock_event_replaces_signature_deltas() {
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDelta as DeltaEnum, ContentBlockDeltaEvent, ReasoningContentBlockDelta,
    };
    // Multiple `Signature` deltas in a single block must replace
    // (not concat) so the next-turn replay can present the
    // upstream-canonical signature unchanged. Anthropic's reasoning
    // signature is a full opaque base64 token, not a streaming buffer.
    let mut state = BedrockStreamState::default();
    let first = ContentBlockDeltaEvent::builder()
        .content_block_index(0)
        .delta(DeltaEnum::ReasoningContent(
            ReasoningContentBlockDelta::Signature("first-signature".to_string()),
        ))
        .build()
        .expect("build first signature event");
    let events = handle_bedrock_event(ConverseStreamOutput::ContentBlockDelta(first), &mut state)
        .expect("handle first signature");
    assert!(events.is_empty());

    let second = ContentBlockDeltaEvent::builder()
        .content_block_index(0)
        .delta(DeltaEnum::ReasoningContent(
            ReasoningContentBlockDelta::Signature("authoritative-signature".to_string()),
        ))
        .build()
        .expect("build second signature event");
    let events = handle_bedrock_event(ConverseStreamOutput::ContentBlockDelta(second), &mut state)
        .expect("handle second signature");
    assert!(events.is_empty());

    let block = state
        .reasoning_blocks
        .get(&0)
        .expect("reasoning block must persist across deltas");
    assert_eq!(
        block.signature.as_deref(),
        Some("authoritative-signature"),
        "second Signature delta must replace, not concat: got {:?}",
        block.signature,
    );
}

/// M-18: the 4-slot allocator hands out four markers in
/// invalidation-priority order (tools -> system -> messages) and
/// then drops+warns on every subsequent consumer. The cap matches
/// Bedrock's documented limit so a future per-skill policy that
/// emits a 5th breakpoint degrades to a warning instead of a
/// `ValidationException` on every turn.
#[test]
fn breakpoint_budget_caps_at_four_and_warns_on_overflow() {
    let mut budget = BreakpointBudget::new();
    assert!(budget.consume("tools"));
    assert!(budget.consume("system"));
    assert!(budget.consume("messages"));
    // A fourth slot is still available so a future caller marker
    // (skill layer, per-message breakpoint) lands inside the cap.
    assert!(budget.consume("future-marker"));
    // Cap reached; any further consumer is dropped.
    assert!(!budget.consume("overflow-1"));
    assert!(!budget.consume("overflow-2"));
    assert_eq!(budget.dropped, 2);
}

/// M-18 regression guard: the auto policy emits exactly three
/// markers today (tools tail / system tail / latest user block).
/// The combined system_blocks + conversation_messages +
/// tool_configuration helpers must still stamp all three when the
/// budget allows.
#[test]
fn auto_caching_emits_three_breakpoints_within_budget() {
    let specs: Vec<Arc<LlmToolSpec>> = vec![
        LlmToolSpec {
            name: "search".to_string(),
            description: "Web search".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
        }
        .into(),
    ];
    // System breakpoint.
    let system = system_blocks("be helpful", CacheRetention::Short).expect("system");
    let system_cache_points = system
        .iter()
        .filter(|b| matches!(b, SystemContentBlock::CachePoint(_)))
        .count();
    assert_eq!(
        system_cache_points, 1,
        "system tail must carry exactly one cachePoint when retention is enabled",
    );
    // Last-user breakpoint.
    let messages = conversation_messages(
        &[
            LlmInputItem::UserText("first".to_string()),
            LlmInputItem::AssistantText("ack".to_string()),
            LlmInputItem::UserText("second".to_string()),
        ],
        CacheRetention::Short,
    )
    .expect("messages");
    let message_cache_points: usize = messages
        .iter()
        .map(|m| {
            m.content()
                .iter()
                .filter(|b| matches!(b, ContentBlock::CachePoint(_)))
                .count()
        })
        .sum();
    assert_eq!(
        message_cache_points, 1,
        "exactly one cachePoint must land on the last user message",
    );
    // Tools tail breakpoint.
    let tools = tool_configuration(&specs, CacheRetention::Short, None)
        .expect("ok")
        .expect("present");
    let tool_cache_points = tools
        .tools()
        .iter()
        .filter(|t| matches!(t, aws_sdk_bedrockruntime::types::Tool::CachePoint(_)))
        .count();
    assert_eq!(
        tool_cache_points, 1,
        "tools tail must carry exactly one cachePoint when retention is enabled",
    );
}
