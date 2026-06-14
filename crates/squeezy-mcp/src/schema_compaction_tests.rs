use super::*;
use serde_json::{Value, json};

#[test]
fn sanitize_tool_schema_strips_null_and_empty_description_fields() {
    let input = json!({
        "type": "object",
        "description": "",
        "title": null,
        "properties": {
            "name": {
                "type": "string",
                "default": null,
                "description": "user name",
            },
            "tags": {
                "type": "array",
                "description": "   ",
                "items": {"type": "string", "extra": null},
            },
        },
    });

    let sanitized = sanitize_tool_schema(&input);

    let object = sanitized.as_object().expect("object");
    assert!(
        !object.contains_key("description"),
        "empty description removed"
    );
    assert!(!object.contains_key("title"), "null fields removed");
    let name = object["properties"]["name"]
        .as_object()
        .expect("name object");
    assert!(!name.contains_key("default"), "nested nulls removed");
    assert_eq!(name["description"], json!("user name"));
    let tags = object["properties"]["tags"]
        .as_object()
        .expect("tags object");
    assert!(
        !tags.contains_key("description"),
        "whitespace description removed"
    );
    let items = tags["items"].as_object().expect("items object");
    assert!(!items.contains_key("extra"), "nested null in items removed");
}

#[test]
fn compact_tool_schema_shrinks_large_schema_and_drops_unused_defs() {
    let mut properties = serde_json::Map::new();
    let mut defs = serde_json::Map::new();
    for index in 0..50 {
        let prop_name = format!("field_{index:02}");
        properties.insert(
            prop_name,
            json!({
                "type": "string",
                "description": "",
                "default": null,
                "examples": ["x".repeat(40)],
            }),
        );
        // Only the first 5 defs are referenced; the rest are unreachable.
        defs.insert(
            format!("def_{index:02}"),
            json!({
                "type": "object",
                "description": null,
                "properties": {"value": {"type": "string"}},
            }),
        );
    }
    let mut input = serde_json::Map::new();
    input.insert("type".to_string(), json!("object"));
    input.insert("title".to_string(), Value::Null);
    input.insert("properties".to_string(), Value::Object(properties.clone()));
    // Reference only def_00..def_04.
    let mut required_refs = Vec::new();
    for index in 0..5 {
        required_refs.push(json!({"$ref": format!("#/$defs/def_{index:02}")}));
    }
    input.insert("allOf".to_string(), Value::Array(required_refs));
    input.insert("$defs".to_string(), Value::Object(defs));
    let input = Value::Object(input);

    let (compacted, stats) = compact_tool_schema(&input, 4096);

    assert_eq!(stats.original_bytes, input.to_string().len());
    assert_eq!(stats.compacted_bytes, compacted.to_string().len());
    assert!(
        stats.compacted_bytes <= stats.original_bytes,
        "compaction must never expand: {stats:?}"
    );
    assert!(
        stats.compacted_bytes <= (stats.original_bytes * 9) / 10,
        "expected at least 10% shrink, got {stats:?}"
    );
    assert!(
        stats.ratio < 0.91,
        "ratio should reflect shrink: {}",
        stats.ratio
    );
    // Unreferenced defs removed; the 5 referenced ones survive.
    let defs = compacted["$defs"].as_object().expect("defs survive");
    assert_eq!(defs.len(), 5, "only referenced defs are kept: {defs:?}");
    // Properties retain their structural top-level surface.
    assert_eq!(
        compacted["properties"].as_object().map(|map| map.len()),
        Some(50),
        "top-level property surface preserved",
    );
}

#[test]
fn compact_tool_schema_is_idempotent_for_empty_input() {
    let input = json!({});
    let (compacted, stats) = compact_tool_schema(&input, 4096);
    assert_eq!(compacted, input, "empty schema is unchanged");
    assert_eq!(stats.original_bytes, stats.compacted_bytes);

    let (again, second_stats) = compact_tool_schema(&compacted, 4096);
    assert_eq!(again, compacted, "running compactor twice is a fixed point");
    assert_eq!(second_stats.compacted_bytes, stats.compacted_bytes);

    let minimal = json!({"type": "object"});
    let (minimal_compacted, minimal_stats) = compact_tool_schema(&minimal, 4096);
    assert_eq!(minimal_compacted, minimal);
    assert!(minimal_stats.compacted_bytes <= minimal_stats.original_bytes);
}
