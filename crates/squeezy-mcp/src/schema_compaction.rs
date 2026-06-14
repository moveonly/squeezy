use std::collections::BTreeSet;

use serde_json::{Value, json};

#[cfg(test)]
#[path = "schema_compaction_tests.rs"]
mod tests;

pub(crate) fn schema_object(value: Value) -> Value {
    if value.as_object().is_some() {
        value
    } else {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CompactionStats {
    pub original_bytes: usize,
    pub compacted_bytes: usize,
    pub ratio: f32,
}

/// Strip explicit `null` fields and empty-string `description` entries from a
/// JSON schema, recursively. Returns a new value; the input is left intact.
fn sanitize_tool_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                if matches!(child, Value::Null) {
                    continue;
                }
                if key == "description"
                    && matches!(child, Value::String(text) if text.trim().is_empty())
                {
                    continue;
                }
                out.insert(key.clone(), sanitize_tool_schema(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_tool_schema).collect()),
        _ => value.clone(),
    }
}

/// Run the three-pass compactor:
///   (1) sanitize - strip null / empty-description fields,
///   (2) `$defs` hoist - drop unreachable definitions,
///   (3) minify - handled implicitly by `Value::to_string()` at emission time.
/// Reports the byte cost before and after compaction. The cap is informational
/// for now (used to gate deeper passes in future work); the function always
/// returns a schema whose serialized size is <= the original.
pub(crate) fn compact_tool_schema(value: &Value, _max_bytes: usize) -> (Value, CompactionStats) {
    let original_bytes = value.to_string().len();
    let sanitized = sanitize_tool_schema(value);
    let pruned = prune_unreachable_defs(sanitized);
    let compacted_bytes = pruned.to_string().len();
    let ratio = if original_bytes == 0 {
        1.0
    } else {
        compacted_bytes as f32 / original_bytes as f32
    };
    (
        pruned,
        CompactionStats {
            original_bytes,
            compacted_bytes,
            ratio,
        },
    )
}

/// Drop entries from `$defs` / `definitions` that are not referenced anywhere
/// in the schema (by `"$ref": "#/$defs/<name>"` or `"#/definitions/<name>"`).
fn prune_unreachable_defs(mut value: Value) -> Value {
    let object = match value.as_object_mut() {
        Some(object) => object,
        None => return value,
    };
    for key in ["$defs", "definitions"] {
        let Some(defs) = object.get(key).and_then(Value::as_object) else {
            continue;
        };
        if defs.is_empty() {
            object.remove(key);
            continue;
        }
        // Build the set of refs that appear outside the defs block itself.
        let prefix = ref_prefix(key);
        let mut referenced = BTreeSet::new();
        collect_refs_outside_key(object, key, &prefix, &mut referenced);
        let Some(defs) = object.get_mut(key).and_then(Value::as_object_mut) else {
            continue;
        };
        let defs = std::mem::take(defs);
        // Walk over def bodies too: a referenced def may itself ref another def.
        let mut frontier: Vec<String> = referenced.iter().cloned().collect();
        while let Some(name) = frontier.pop() {
            let Some(body) = defs.get(&name) else {
                continue;
            };
            let mut nested = BTreeSet::new();
            collect_refs_with_prefix(body, &prefix, &mut nested);
            for next in nested {
                if referenced.insert(next.clone()) {
                    frontier.push(next);
                }
            }
        }
        let kept: serde_json::Map<String, Value> = defs
            .into_iter()
            .filter(|(name, _)| referenced.contains(name))
            .collect();
        if kept.is_empty() {
            object.remove(key);
        } else if let Some(defs) = object.get_mut(key) {
            *defs = Value::Object(kept);
        }
    }
    value
}

fn collect_refs_outside_key(
    object: &serde_json::Map<String, Value>,
    skip_key: &str,
    prefix: &str,
    out: &mut BTreeSet<String>,
) {
    for (key, child) in object {
        if key != skip_key {
            collect_ref_field(key, child, prefix, out);
            collect_refs_with_prefix(child, prefix, out);
        }
    }
}

fn collect_refs_with_prefix(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                collect_ref_field(key, child, prefix, out);
                collect_refs_with_prefix(child, prefix, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs_with_prefix(item, prefix, out);
            }
        }
        _ => {}
    }
}

fn collect_ref_field(key: &str, child: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    if key == "$ref"
        && let Some(text) = child.as_str()
        && let Some(name) = text.strip_prefix(prefix)
        && !out.contains(name)
    {
        out.insert(name.to_string());
    }
}

fn ref_prefix(defs_key: &str) -> String {
    format!("#/{defs_key}/")
}
