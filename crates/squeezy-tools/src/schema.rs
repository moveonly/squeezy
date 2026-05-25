//! Tool-schema sanitization and lossy compaction.
//!
//! Tool parameter schemas advertised to the model can grow arbitrarily large —
//! especially for external MCP servers that ship verbose JSON Schemas with
//! deeply nested `$defs`, long descriptions, and unreachable definitions.
//! Every byte of that schema costs tokens on every turn the tool is loaded.
//!
//! [`compact_tool_parameters`] runs a three-stage pipeline:
//! 1. **Sanitize** — coerce boolean schemas to permissive object schemas,
//!    infer missing `type` keywords from sibling hints, normalize child
//!    tables.
//! 2. **Prune** — remove `$defs` / `definitions` entries unreachable from any
//!    `$ref`.
//! 3. **Compact** — when the sanitized + pruned schema still serializes above
//!    `MAX_TOOL_SCHEMA_BYTES`, apply increasingly lossy passes until it fits:
//!    strip descriptions, drop definitions (rewriting `$ref` → `{}`), then
//!    collapse objects deeper than `MAX_TOOL_SCHEMA_DEPTH`.

use serde_json::{Map, Value};
use std::collections::BTreeSet;

pub(crate) const MAX_TOOL_SCHEMA_BYTES: usize = 4_000;
pub(crate) const MAX_TOOL_SCHEMA_DEPTH: usize = 2;

/// Run the sanitize → prune → compact pipeline in-place.
pub(crate) fn compact_tool_parameters(value: &mut Value) {
    sanitize_json_schema(value);
    prune_unreachable_definitions(value);
    if fits_budget(value) {
        return;
    }
    strip_schema_descriptions(value);
    if fits_budget(value) {
        return;
    }
    drop_schema_definitions(value);
    if fits_budget(value) {
        return;
    }
    collapse_deep_schema_objects(value, 0);
}

fn fits_budget(value: &Value) -> bool {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() <= MAX_TOOL_SCHEMA_BYTES)
        .unwrap_or(true)
}

fn sanitize_json_schema(value: &mut Value) {
    match value {
        Value::Bool(true) => {
            *value = Value::Object(Map::new());
        }
        Value::Bool(false) => {
            // A `false` schema accepts nothing — preserve as a forbidden shape
            // but encode as an explicit empty enum so downstream JSON readers
            // never have to handle a bool here.
            let mut map = Map::new();
            map.insert("not".to_string(), Value::Object(Map::new()));
            *value = Value::Object(map);
        }
        Value::Object(map) => {
            sanitize_object_schema(map);
        }
        _ => {}
    }
}

fn sanitize_object_schema(map: &mut Map<String, Value>) {
    // Coerce `const` into single-element `enum` so consumers only deal with
    // the enum case.
    if let Some(constant) = map.remove("const") {
        map.entry("enum")
            .or_insert_with(|| Value::Array(vec![constant]));
    }

    // Infer missing `type` from sibling hints. JSON Schema does not require
    // `type`, but advertising it makes downstream models more reliable.
    if !map.contains_key("type") {
        let inferred = if map.contains_key("properties")
            || map.contains_key("required")
            || map.contains_key("additionalProperties")
        {
            Some("object")
        } else if map.contains_key("items") {
            Some("array")
        } else {
            None
        };
        if let Some(ty) = inferred {
            map.insert("type".to_string(), Value::String(ty.to_string()));
        }
    }

    // Recurse into properties (each value is itself a schema).
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (_, child) in properties.iter_mut() {
            sanitize_json_schema(child);
        }
    }

    // Recurse into items (single schema or tuple form).
    if let Some(items) = map.get_mut("items") {
        match items {
            Value::Array(children) => {
                for child in children.iter_mut() {
                    sanitize_json_schema(child);
                }
            }
            other => sanitize_json_schema(other),
        }
    }

    // Composition keywords.
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(children)) = map.get_mut(key) {
            for child in children.iter_mut() {
                sanitize_json_schema(child);
            }
        }
    }
    if let Some(not) = map.get_mut("not") {
        sanitize_json_schema(not);
    }

    // Definition tables.
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = map.get_mut(key) {
            for (_, child) in defs.iter_mut() {
                sanitize_json_schema(child);
            }
        }
    }
}

fn prune_unreachable_definitions(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    let has_defs = map.contains_key("$defs") || map.contains_key("definitions");
    if !has_defs {
        return;
    }

    let mut reachable: BTreeSet<String> = BTreeSet::new();
    collect_refs_outside_defs(map, &mut reachable);

    // Follow refs into definitions until reachable set stabilizes.
    let mut frontier: Vec<String> = reachable.iter().cloned().collect();
    while let Some(name) = frontier.pop() {
        for key in ["$defs", "definitions"] {
            let Some(Value::Object(defs)) = map.get(key) else {
                continue;
            };
            let Some(def) = defs.get(&name) else {
                continue;
            };
            let mut nested = BTreeSet::new();
            collect_refs(def, &mut nested);
            for r in nested {
                if reachable.insert(r.clone()) {
                    frontier.push(r);
                }
            }
        }
    }

    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = map.get_mut(key) {
            defs.retain(|name, _| reachable.contains(name));
            if defs.is_empty() {
                map.remove(key);
            }
        }
    }
}

fn collect_refs_outside_defs(map: &Map<String, Value>, out: &mut BTreeSet<String>) {
    for (key, value) in map.iter() {
        if key == "$defs" || key == "definitions" {
            continue;
        }
        collect_refs(value, out);
    }
}

fn collect_refs(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && let Some(name) = ref_target_name(reference)
            {
                out.insert(name.to_string());
            }
            for (_, child) in map.iter() {
                collect_refs(child, out);
            }
        }
        Value::Array(children) => {
            for child in children {
                collect_refs(child, out);
            }
        }
        _ => {}
    }
}

fn ref_target_name(reference: &str) -> Option<&str> {
    for prefix in ["#/$defs/", "#/definitions/"] {
        if let Some(rest) = reference.strip_prefix(prefix) {
            // Refs may include nested paths; take the top-level name.
            return Some(rest.split('/').next().unwrap_or(rest));
        }
    }
    None
}

fn strip_schema_descriptions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("description");
            map.remove("title");
            for (_, child) in map.iter_mut() {
                strip_schema_descriptions(child);
            }
        }
        Value::Array(children) => {
            for child in children {
                strip_schema_descriptions(child);
            }
        }
        _ => {}
    }
}

fn drop_schema_definitions(value: &mut Value) {
    rewrite_refs_to_empty(value);
    if let Value::Object(map) = value {
        map.remove("$defs");
        map.remove("definitions");
    }
}

fn rewrite_refs_to_empty(value: &mut Value) {
    if let Value::Object(map) = value {
        if map.contains_key("$ref") {
            map.clear();
            return;
        }
        for (_, child) in map.iter_mut() {
            rewrite_refs_to_empty(child);
        }
    } else if let Value::Array(children) = value {
        for child in children {
            rewrite_refs_to_empty(child);
        }
    }
}

fn collapse_deep_schema_objects(value: &mut Value, depth: usize) {
    if depth >= MAX_TOOL_SCHEMA_DEPTH
        && let Value::Object(map) = value
    {
        let complex = map.contains_key("properties")
            || map.contains_key("items")
            || map.contains_key("anyOf")
            || map.contains_key("oneOf")
            || map.contains_key("allOf");
        if complex {
            *value = Value::Object(Map::new());
            return;
        }
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let next_depth = match key.as_str() {
                    "properties" | "items" | "anyOf" | "oneOf" | "allOf" | "$defs"
                    | "definitions" | "prefixItems" => depth + 1,
                    _ => depth,
                };
                collapse_deep_schema_objects(child, next_depth);
            }
        }
        Value::Array(children) => {
            for child in children {
                collapse_deep_schema_objects(child, depth);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
