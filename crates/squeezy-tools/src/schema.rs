//! Tool-schema sanitization and lossy compaction.
//!
//! Tool parameter schemas advertised to the model can grow arbitrarily large —
//! especially for external MCP servers that ship verbose JSON Schemas with
//! deeply nested `$defs`, long descriptions, and unreachable definitions.
//! Every byte of that schema costs tokens on every turn the tool is loaded.
//!
//! The pipeline runs against a typed [`JsonSchema`] (not raw [`serde_json::Value`])
//! so the parameter shape is validated once on entry and once on exit. Any
//! schema fragment that survives compaction must round-trip through the typed
//! enum, so a tool emitting an unsupported `"type"` value or a stray
//! `additionalProperties: 42` is normalized in one place rather than at every
//! downstream provider translator.
//!
//! [`compact_tool_parameters`] runs three logical stages on the value:
//! 1. **Sanitize** — coerce boolean schemas to permissive object schemas,
//!    infer missing `type` keywords from sibling hints, normalize child
//!    tables, coerce `const` into single-element `enum`.
//! 2. **Prune** — remove `$defs` / `definitions` entries unreachable from any
//!    `$ref`.
//! 3. **Compact** — when the sanitized + pruned schema still serializes above
//!    [`MAX_TOOL_SCHEMA_BYTES`], apply increasingly lossy passes until it fits:
//!    strip descriptions, drop definitions (rewriting `$ref` to `{}`), then
//!    collapse objects deeper than [`MAX_TOOL_SCHEMA_DEPTH`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const MAX_TOOL_SCHEMA_BYTES: usize = 4_000;
pub(crate) const MAX_TOOL_SCHEMA_DEPTH: usize = 2;

/// Primitive JSON Schema type names we support in tool parameter schemas.
///
/// This mirrors the OpenAI Structured Outputs subset for JSON Schema `type`:
/// string, number, boolean, integer, object, array, and null. Anything else
/// is treated as missing and dropped during sanitization, so downstream
/// consumers never see an unrecognized type string.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JsonSchemaPrimitiveType {
    String,
    Number,
    Boolean,
    Integer,
    Object,
    Array,
    Null,
}

/// JSON Schema `type` keyword: either a single type name or a union.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum JsonSchemaType {
    Single(JsonSchemaPrimitiveType),
    Multiple(Vec<JsonSchemaPrimitiveType>),
}

/// `additionalProperties` keyword: boolean toggle or a nested schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AdditionalProperties {
    Boolean(bool),
    Schema(Box<JsonSchema>),
}

/// Typed JSON-Schema subset used for tool parameter schemas.
///
/// Round-tripping a `Value` through this struct is the drift-prevention
/// invariant: any field not listed here is dropped, any field with an
/// incompatible shape fails deserialization. The pipeline catches both and
/// degrades gracefully (see [`compact_tool_parameters`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JsonSchema {
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub schema_ref: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub schema_type: Option<JsonSchemaType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<JsonSchema>>,
    #[serde(rename = "prefixItems", skip_serializing_if = "Option::is_none")]
    pub prefix_items: Option<Vec<JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<BTreeMap<String, JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    #[serde(
        rename = "additionalProperties",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_properties: Option<AdditionalProperties>,
    #[serde(rename = "anyOf", skip_serializing_if = "Option::is_none")]
    pub any_of: Option<Vec<JsonSchema>>,
    #[serde(rename = "oneOf", skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<JsonSchema>>,
    #[serde(rename = "allOf", skip_serializing_if = "Option::is_none")]
    pub all_of: Option<Vec<JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not: Option<Box<JsonSchema>>,
    #[serde(rename = "$defs", skip_serializing_if = "Option::is_none")]
    pub defs: Option<BTreeMap<String, JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definitions: Option<BTreeMap<String, JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<serde_json::Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<serde_json::Number>,
}

impl JsonSchema {
    /// True when the schema is a complex composite (object / array / union /
    /// ref). The deep-collapse pass uses this to decide whether a node carries
    /// enough structural signal to be worth replacing with `{}` past the depth
    /// budget — scalar leaves with only `type`/`description` are left alone.
    fn is_complex(&self) -> bool {
        self.properties.is_some()
            || self.items.is_some()
            || self.any_of.is_some()
            || self.one_of.is_some()
            || self.all_of.is_some()
            || self.additional_properties.is_some()
            || self.schema_ref.is_some()
    }
}

/// Run the sanitize → prune → compact pipeline in-place on a raw
/// `serde_json::Value`. Internally the value is parsed into a [`JsonSchema`]
/// so type drift is caught at the boundary; the result is re-serialized back
/// into `value`.
pub(crate) fn compact_tool_parameters(value: &mut Value) {
    sanitize_value(value);
    let mut schema = value_to_schema(value);
    prune_unreachable_definitions(&mut schema);

    if fits_budget(&schema) {
        write_schema_back(value, &schema);
        return;
    }
    strip_schema_descriptions(&mut schema);
    if fits_budget(&schema) {
        write_schema_back(value, &schema);
        return;
    }
    drop_schema_definitions(&mut schema);
    if fits_budget(&schema) {
        write_schema_back(value, &schema);
        return;
    }
    collapse_deep_schema_objects(&mut schema, 0);
    write_schema_back(value, &schema);
}

/// Parse `value` into a [`JsonSchema`]. Any field outside the typed surface is
/// discarded, and incompatible shapes degrade to default — the pipeline still
/// produces a well-typed schema rather than refusing the tool altogether.
fn value_to_schema(value: &Value) -> JsonSchema {
    serde_json::from_value::<JsonSchema>(value.clone()).unwrap_or_default()
}

/// Re-serialize the typed schema back into the in-place `Value`. Falls back
/// to `{}` if serialization somehow fails so the caller never sees a stale
/// value.
fn write_schema_back(value: &mut Value, schema: &JsonSchema) {
    *value = serde_json::to_value(schema).unwrap_or(Value::Object(Map::new()));
}

fn fits_budget(schema: &JsonSchema) -> bool {
    serde_json::to_vec(schema)
        .map(|bytes| bytes.len() <= MAX_TOOL_SCHEMA_BYTES)
        .unwrap_or(true)
}

/// Pre-pass that normalizes a raw `Value` *before* it round-trips through the
/// typed schema. Handles JSON-Schema-isms that do not deserialize cleanly into
/// our typed surface (boolean schemas, missing `type`, `const`) so the typed
/// schema sees a uniform shape.
fn sanitize_value(value: &mut Value) {
    match value {
        Value::Bool(true) => {
            *value = Value::Object(Map::new());
        }
        Value::Bool(false) => {
            // A `false` schema accepts nothing — preserve as `{ "not": {} }`
            // so downstream JSON consumers never see a bare bool.
            let mut map = Map::new();
            map.insert("not".to_string(), Value::Object(Map::new()));
            *value = Value::Object(map);
        }
        Value::Object(map) => {
            sanitize_object(map);
        }
        _ => {}
    }
}

fn sanitize_object(map: &mut Map<String, Value>) {
    // `const` -> single-element `enum` (typed schema only models `enum`).
    if let Some(constant) = map.remove("const") {
        map.entry("enum")
            .or_insert_with(|| Value::Array(vec![constant]));
    }

    // Infer missing `type` from sibling hints.
    if !map.contains_key("type") {
        let inferred = if map.contains_key("properties")
            || map.contains_key("required")
            || map.contains_key("additionalProperties")
        {
            Some("object")
        } else if map.contains_key("items") || map.contains_key("prefixItems") {
            Some("array")
        } else {
            None
        };
        if let Some(ty) = inferred {
            map.insert("type".to_string(), Value::String(ty.to_string()));
        }
    }

    // Recurse into nested schemas before the typed conversion swallows them.
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (_, child) in properties.iter_mut() {
            sanitize_value(child);
        }
    }
    if let Some(items) = map.get_mut("items") {
        match items {
            Value::Array(children) => {
                for child in children.iter_mut() {
                    sanitize_value(child);
                }
            }
            other => sanitize_value(other),
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(children)) = map.get_mut(key) {
            for child in children.iter_mut() {
                sanitize_value(child);
            }
        }
    }
    if let Some(not) = map.get_mut("not") {
        sanitize_value(not);
    }
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = map.get_mut(key) {
            for (_, child) in defs.iter_mut() {
                sanitize_value(child);
            }
        }
    }
    if let Some(additional) = map.get_mut("additionalProperties") {
        if !matches!(additional, Value::Bool(_)) {
            sanitize_value(additional);
        }
    }
}

fn strip_schema_descriptions(schema: &mut JsonSchema) {
    schema.description = None;
    schema.title = None;
    if let Some(properties) = schema.properties.as_mut() {
        for (_, child) in properties.iter_mut() {
            strip_schema_descriptions(child);
        }
    }
    if let Some(items) = schema.items.as_mut() {
        strip_schema_descriptions(items.as_mut());
    }
    if let Some(prefix) = schema.prefix_items.as_mut() {
        for child in prefix.iter_mut() {
            strip_schema_descriptions(child);
        }
    }
    for variants in [
        schema.any_of.as_mut(),
        schema.one_of.as_mut(),
        schema.all_of.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        for child in variants.iter_mut() {
            strip_schema_descriptions(child);
        }
    }
    if let Some(not) = schema.not.as_mut() {
        strip_schema_descriptions(not.as_mut());
    }
    if let Some(AdditionalProperties::Schema(child)) = schema.additional_properties.as_mut() {
        strip_schema_descriptions(child.as_mut());
    }
    for table in [schema.defs.as_mut(), schema.definitions.as_mut()]
        .into_iter()
        .flatten()
    {
        for (_, child) in table.iter_mut() {
            strip_schema_descriptions(child);
        }
    }
}

fn prune_unreachable_definitions(schema: &mut JsonSchema) {
    if schema.defs.is_none() && schema.definitions.is_none() {
        return;
    }

    let mut reachable: BTreeSet<String> = BTreeSet::new();
    collect_refs_outside_defs(schema, &mut reachable);

    // Follow refs into definitions until the reachable set stabilizes.
    let mut frontier: Vec<String> = reachable.iter().cloned().collect();
    while let Some(name) = frontier.pop() {
        for table in [schema.defs.as_ref(), schema.definitions.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(def) = table.get(&name) {
                let mut nested = BTreeSet::new();
                collect_refs(def, &mut nested);
                for r in nested {
                    if reachable.insert(r.clone()) {
                        frontier.push(r);
                    }
                }
            }
        }
    }

    for table in [schema.defs.as_mut(), schema.definitions.as_mut()]
        .into_iter()
        .flatten()
    {
        table.retain(|name, _| reachable.contains(name));
    }
    if schema.defs.as_ref().is_some_and(|d| d.is_empty()) {
        schema.defs = None;
    }
    if schema.definitions.as_ref().is_some_and(|d| d.is_empty()) {
        schema.definitions = None;
    }
}

fn collect_refs_outside_defs(schema: &JsonSchema, out: &mut BTreeSet<String>) {
    if let Some(reference) = schema.schema_ref.as_deref() {
        if let Some(name) = ref_target_name(reference) {
            out.insert(name.to_string());
        }
    }
    if let Some(properties) = schema.properties.as_ref() {
        for (_, child) in properties.iter() {
            collect_refs(child, out);
        }
    }
    if let Some(items) = schema.items.as_ref() {
        collect_refs(items, out);
    }
    if let Some(prefix) = schema.prefix_items.as_ref() {
        for child in prefix {
            collect_refs(child, out);
        }
    }
    for variants in [
        schema.any_of.as_ref(),
        schema.one_of.as_ref(),
        schema.all_of.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        for child in variants {
            collect_refs(child, out);
        }
    }
    if let Some(not) = schema.not.as_ref() {
        collect_refs(not, out);
    }
    if let Some(AdditionalProperties::Schema(child)) = schema.additional_properties.as_ref() {
        collect_refs(child, out);
    }
}

fn collect_refs(schema: &JsonSchema, out: &mut BTreeSet<String>) {
    if let Some(reference) = schema.schema_ref.as_deref() {
        if let Some(name) = ref_target_name(reference) {
            out.insert(name.to_string());
        }
    }
    if let Some(properties) = schema.properties.as_ref() {
        for (_, child) in properties.iter() {
            collect_refs(child, out);
        }
    }
    if let Some(items) = schema.items.as_ref() {
        collect_refs(items, out);
    }
    if let Some(prefix) = schema.prefix_items.as_ref() {
        for child in prefix {
            collect_refs(child, out);
        }
    }
    for variants in [
        schema.any_of.as_ref(),
        schema.one_of.as_ref(),
        schema.all_of.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        for child in variants {
            collect_refs(child, out);
        }
    }
    if let Some(not) = schema.not.as_ref() {
        collect_refs(not, out);
    }
    if let Some(AdditionalProperties::Schema(child)) = schema.additional_properties.as_ref() {
        collect_refs(child, out);
    }
    for table in [schema.defs.as_ref(), schema.definitions.as_ref()]
        .into_iter()
        .flatten()
    {
        for (_, child) in table.iter() {
            collect_refs(child, out);
        }
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

fn drop_schema_definitions(schema: &mut JsonSchema) {
    rewrite_refs_to_empty(schema);
    schema.defs = None;
    schema.definitions = None;
}

fn rewrite_refs_to_empty(schema: &mut JsonSchema) {
    if schema.schema_ref.is_some() {
        *schema = JsonSchema::default();
        return;
    }
    if let Some(properties) = schema.properties.as_mut() {
        for (_, child) in properties.iter_mut() {
            rewrite_refs_to_empty(child);
        }
    }
    if let Some(items) = schema.items.as_mut() {
        rewrite_refs_to_empty(items.as_mut());
    }
    if let Some(prefix) = schema.prefix_items.as_mut() {
        for child in prefix.iter_mut() {
            rewrite_refs_to_empty(child);
        }
    }
    for variants in [
        schema.any_of.as_mut(),
        schema.one_of.as_mut(),
        schema.all_of.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        for child in variants.iter_mut() {
            rewrite_refs_to_empty(child);
        }
    }
    if let Some(not) = schema.not.as_mut() {
        rewrite_refs_to_empty(not.as_mut());
    }
    if let Some(AdditionalProperties::Schema(child)) = schema.additional_properties.as_mut() {
        rewrite_refs_to_empty(child.as_mut());
    }
}

fn collapse_deep_schema_objects(schema: &mut JsonSchema, depth: usize) {
    if depth >= MAX_TOOL_SCHEMA_DEPTH && schema.is_complex() {
        *schema = JsonSchema::default();
        return;
    }
    if let Some(properties) = schema.properties.as_mut() {
        for (_, child) in properties.iter_mut() {
            collapse_deep_schema_objects(child, depth + 1);
        }
    }
    if let Some(items) = schema.items.as_mut() {
        collapse_deep_schema_objects(items.as_mut(), depth + 1);
    }
    if let Some(prefix) = schema.prefix_items.as_mut() {
        for child in prefix.iter_mut() {
            collapse_deep_schema_objects(child, depth + 1);
        }
    }
    for variants in [
        schema.any_of.as_mut(),
        schema.one_of.as_mut(),
        schema.all_of.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        for child in variants.iter_mut() {
            collapse_deep_schema_objects(child, depth + 1);
        }
    }
    if let Some(not) = schema.not.as_mut() {
        collapse_deep_schema_objects(not.as_mut(), depth + 1);
    }
    if let Some(AdditionalProperties::Schema(child)) = schema.additional_properties.as_mut() {
        collapse_deep_schema_objects(child.as_mut(), depth + 1);
    }
    for table in [schema.defs.as_mut(), schema.definitions.as_mut()]
        .into_iter()
        .flatten()
    {
        for (_, child) in table.iter_mut() {
            collapse_deep_schema_objects(child, depth + 1);
        }
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
