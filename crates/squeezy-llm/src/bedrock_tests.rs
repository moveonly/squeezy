use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, SystemContentBlock, ToolInputSchema,
};
use aws_smithy_types::{Document, Number};
use serde_json::json;

use super::{conversation_messages, json_to_document, system_blocks, tool_configuration};
use crate::{LlmInputItem, LlmToolSpec};

#[test]
fn system_blocks_skip_blank_instructions() {
    assert!(system_blocks("").is_empty());
    assert!(system_blocks("   \n  ").is_empty());
}

#[test]
fn system_blocks_emit_single_text_block() {
    let blocks = system_blocks("be helpful");
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        SystemContentBlock::Text(text) => assert_eq!(text, "be helpful"),
        _ => panic!("expected Text system block"),
    }
}

#[test]
fn conversation_messages_merge_consecutive_user_turns() {
    let messages = conversation_messages(&[
        LlmInputItem::UserText("hello".to_string()),
        LlmInputItem::UserText("again".to_string()),
        LlmInputItem::AssistantText("hi".to_string()),
    ])
    .expect("build messages");

    assert_eq!(messages.len(), 2);
    assert_eq!(*messages[0].role(), ConversationRole::User);
    assert_eq!(messages[0].content().len(), 2);
    assert_eq!(*messages[1].role(), ConversationRole::Assistant);
}

#[test]
fn conversation_messages_round_trip_tool_call_and_result() {
    let messages = conversation_messages(&[
        LlmInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "search".to_string(),
            arguments: json!({"query": "rust"}),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_1".to_string(),
            output: "ok".to_string(),
        },
    ])
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
fn tool_configuration_round_trips_json_schema() {
    let specs = vec![LlmToolSpec {
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
    }];
    let config = tool_configuration(&specs).expect("ok").expect("present");
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
fn tool_configuration_returns_none_when_empty() {
    assert!(tool_configuration(&[]).expect("ok").is_none());
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
