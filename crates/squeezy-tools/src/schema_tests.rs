use super::*;
use serde_json::json;

fn byte_len(value: &Value) -> usize {
    serde_json::to_vec(value).expect("serialize schema").len()
}

#[test]
fn compact_keeps_small_schemas_unchanged() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "user name"},
            "age": {"type": "integer"}
        },
        "required": ["name"]
    });
    let original = schema.clone();
    compact_tool_parameters(&mut schema);
    // Sanitize is a no-op here; descriptions and structure survive.
    assert_eq!(schema, original);
    assert!(byte_len(&schema) <= MAX_TOOL_SCHEMA_BYTES);
}

#[test]
fn compact_strips_descriptions_when_oversized() {
    let big_desc = "x".repeat(MAX_TOOL_SCHEMA_BYTES);
    let mut schema = json!({
        "type": "object",
        "description": big_desc,
        "properties": {
            "name": {"type": "string", "description": "a property"}
        }
    });
    compact_tool_parameters(&mut schema);
    // Top-level properties survive after description strip.
    assert!(schema.get("properties").is_some());
    assert!(schema.get("description").is_none());
    assert!(schema["properties"]["name"].get("description").is_none());
    assert!(byte_len(&schema) <= MAX_TOOL_SCHEMA_BYTES);
}

#[test]
fn compact_drops_definitions_when_needed() {
    // Build a $defs entry that overflows the budget even after descriptions
    // are stripped, so the pipeline must reach drop-definitions.
    let mut big_props = serde_json::Map::new();
    for i in 0..400 {
        big_props.insert(format!("field_{i:04}"), json!({"type": "string"}));
    }
    let mut schema = json!({
        "type": "object",
        "properties": {
            "user": {"$ref": "#/$defs/User"},
            "name": {"type": "string"}
        },
        "$defs": {
            "User": {
                "type": "object",
                "properties": big_props,
            }
        }
    });
    assert!(byte_len(&schema) > MAX_TOOL_SCHEMA_BYTES);
    compact_tool_parameters(&mut schema);
    assert!(byte_len(&schema) <= MAX_TOOL_SCHEMA_BYTES);
    // After drop-definitions: $defs gone and $ref rewritten to empty object.
    assert!(
        schema.get("$defs").is_none(),
        "drop_definitions did not run; schema still has $defs: {schema:?}"
    );
    let user = &schema["properties"]["user"];
    assert!(user.as_object().map(|m| m.is_empty()).unwrap_or(false));
    // The non-ref property is preserved.
    assert_eq!(schema["properties"]["name"]["type"], json!("string"));
}

#[test]
fn compact_collapses_deep_objects() {
    // Manually build a deeply nested schema where the final compaction pass
    // is the only thing that can fit it.
    let mut schema = json!({"type": "object", "properties": {}});
    {
        let mut node = &mut schema["properties"];
        for i in 0..6 {
            let key = format!("level_{i}");
            node[&key] = json!({
                "type": "object",
                "description": "z".repeat(800),
                "properties": {}
            });
            node = &mut node[&key]["properties"];
        }
    }
    compact_tool_parameters(&mut schema);
    assert!(byte_len(&schema) <= MAX_TOOL_SCHEMA_BYTES);
}

#[test]
fn prune_drops_unreachable_definitions() {
    let mut schema = json!({
        "type": "object",
        "properties": {"user": {"$ref": "#/$defs/User"}},
        "$defs": {
            "User": {"type": "object", "properties": {"id": {"type": "string"}}},
            "Orphan": {"type": "object", "properties": {"x": {"type": "string"}}}
        }
    });
    compact_tool_parameters(&mut schema);
    let defs = schema.get("$defs");
    if let Some(Value::Object(defs)) = defs {
        assert!(defs.contains_key("User"));
        assert!(!defs.contains_key("Orphan"));
    }
}

#[test]
fn sanitize_coerces_boolean_schemas() {
    let mut schema = json!({"type": "object", "properties": {"x": true}});
    compact_tool_parameters(&mut schema);
    assert_eq!(schema["properties"]["x"], json!({}));
}

#[test]
fn sanitize_infers_object_type_from_properties() {
    let mut schema = json!({
        "properties": {"x": {"type": "string"}}
    });
    compact_tool_parameters(&mut schema);
    assert_eq!(schema["type"], json!("object"));
}

#[test]
fn sanitize_coerces_const_to_enum() {
    let mut schema = json!({
        "type": "object",
        "properties": {"mode": {"const": "fast"}}
    });
    compact_tool_parameters(&mut schema);
    assert_eq!(schema["properties"]["mode"]["enum"], json!(["fast"]));
    assert!(schema["properties"]["mode"].get("const").is_none());
}

#[test]
fn compact_keeps_top_level_properties_after_oversized_mcp_schema() {
    // 30 KB schema with long descriptions and nested $defs (simulates a real
    // MCP server schema).
    let big_desc = "d".repeat(2000);
    let mut props = serde_json::Map::new();
    for i in 0..40 {
        props.insert(
            format!("prop_{i}"),
            json!({"type": "string", "description": big_desc}),
        );
    }
    let mut schema = Value::Object(serde_json::Map::from_iter([
        ("type".to_string(), json!("object")),
        ("properties".to_string(), Value::Object(props)),
    ]));
    assert!(byte_len(&schema) >= 30_000);
    compact_tool_parameters(&mut schema);
    assert!(byte_len(&schema) <= MAX_TOOL_SCHEMA_BYTES);
    // Top-level properties survive (just stripped of descriptions).
    assert!(schema.get("properties").is_some());
}
