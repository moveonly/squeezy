//! Tool-schema sanitization and lossy compaction.
//!
//! Tool parameter schemas advertised to the model can grow arbitrarily large —
//! especially for external MCP servers that ship verbose JSON Schemas with
//! deeply nested `$defs`, long descriptions, and unreachable definitions.
//! Every byte of that schema costs tokens on every turn the tool is loaded.
//!
//! `ToolSpec::parameters` holds a typed [`JsonSchema`] (not a raw
//! [`serde_json::Value`]), so the parameter shape is validated once on entry
//! and once on exit. Internal specs route through
//! [`parse_strict_tool_parameters`] which uses
//! `#[serde(deny_unknown_fields)]`: a misspelled JSON-Schema keyword in a
//! first-party spec (such as `"propeties"` instead of `"properties"`) makes
//! `serde_json::from_value` fail and the spec constructor panics at process
//! startup, surfacing the drift at registration time instead of silently
//! shipping an empty schema to the model. External MCP schemas come from
//! untrusted servers and route through [`parse_lossy_tool_parameters`],
//! which first strips fields outside our typed surface so unknown
//! JSON-Schema-isms degrade gracefully to the modeled subset.
//!
//! [`compact_typed_tool_parameters`] runs three logical passes on the
//! typed schema:
//! 1. **Prune** — remove `$defs` / `definitions` entries unreachable from
//!    any `$ref`.
//! 2. **Compact** — when the pruned schema still serializes above
//!    [`MAX_TOOL_SCHEMA_BYTES`], apply increasingly lossy passes until it
//!    fits: strip descriptions, drop definitions (rewriting `$ref` to
//!    `{}`), then collapse objects deeper than [`MAX_TOOL_SCHEMA_DEPTH`].

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
/// `#[serde(deny_unknown_fields)]` is the registration-time guard that
/// keeps first-party specs honest: a misspelled keyword inside a
/// `parse_strict_tool_parameters` call (used by every first-party
/// [`crate::ToolSpec`] constructor in `crate::specs`) makes `from_value`
/// return an error, and the wrapping `tool_schema!` helper panics at
/// startup. Without the typed surface a typo would silently disappear and
/// the model would receive a degraded schema only visible at dispatch
/// time. External MCP schemas (built via
/// [`parse_lossy_tool_parameters`]) are pre-stripped down to the modeled
/// subset before strict deserialization so this guard does not reject
/// arbitrary external JSON Schemas.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
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
    #[serde(rename = "minLength", skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u64>,
    #[serde(rename = "maxLength", skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u64>,
    #[serde(rename = "minItems", skip_serializing_if = "Option::is_none")]
    pub min_items: Option<u64>,
    #[serde(rename = "maxItems", skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u64>,
    /// Default suggestion shown to the model. Arbitrary JSON value — not
    /// recursed into by [`strip_unknown_schema_fields`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
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

/// JSON-Schema keys covered by the typed [`JsonSchema`] surface. Used by
/// [`strip_unknown_schema_fields`] to drop everything else before the
/// strict-parse pass.
const KNOWN_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "type",
    "description",
    "title",
    "enum",
    "format",
    "items",
    "prefixItems",
    "properties",
    "required",
    "additionalProperties",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "$defs",
    "definitions",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "default",
];

/// Sanitize and strict-parse a raw [`Value`] into a [`JsonSchema`].
/// Returns `Err` when the value contains any keyword outside the typed
/// surface — exactly the registration-time guard that catches misspellings
/// in first-party tool specs (see [`crate::specs`] callers).
pub fn parse_strict_tool_parameters(value: Value) -> Result<JsonSchema, String> {
    let mut sanitized = value;
    sanitize_value(&mut sanitized);
    serde_json::from_value::<JsonSchema>(sanitized).map_err(|err| err.to_string())
}

/// Tolerant counterpart for external schemas (MCP servers): first strips
/// every field outside the typed surface so strict deserialization succeeds
/// on arbitrary third-party JSON Schemas, then degrades to
/// `JsonSchema::default()` if even the stripped value fails to parse.
pub fn parse_lossy_tool_parameters(value: Value) -> JsonSchema {
    let mut sanitized = value;
    sanitize_value(&mut sanitized);
    strip_unknown_schema_fields(&mut sanitized);
    serde_json::from_value::<JsonSchema>(sanitized).unwrap_or_default()
}

/// Run prune-then-compact passes on an already-typed schema in place.
/// Equivalent to the original [`compact_tool_parameters`] pipeline but
/// without the round-trip through [`Value`].
pub fn compact_typed_tool_parameters(schema: &mut JsonSchema) {
    prune_unreachable_definitions(schema);
    if fits_budget(schema) {
        return;
    }
    strip_schema_descriptions(schema);
    if fits_budget(schema) {
        return;
    }
    drop_schema_definitions(schema);
    if fits_budget(schema) {
        return;
    }
    collapse_deep_schema_objects(schema, 0);
}

/// Sanitize-parse-compact a raw [`Value`] in place. Backward-compatibility
/// shim retained for the existing tests in `schema_tests` — production
/// callers use the strict / lossy parsers plus
/// [`compact_typed_tool_parameters`] directly so the typed schema never
/// round-trips through `Value` once built.
#[cfg(test)]
pub(crate) fn compact_tool_parameters(value: &mut Value) {
    let mut schema = parse_lossy_tool_parameters(value.clone());
    compact_typed_tool_parameters(&mut schema);
    *value = serde_json::to_value(&schema).unwrap_or(Value::Object(Map::new()));
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
    if let Some(additional) = map.get_mut("additionalProperties")
        && !matches!(additional, Value::Bool(_))
    {
        sanitize_value(additional);
    }
}

/// Recursively drop keys not in [`KNOWN_SCHEMA_KEYS`] from a raw [`Value`]
/// shaped like a JSON Schema. Walks schema-positional locations only —
/// `enum` value arrays and `default` payloads carry arbitrary user JSON
/// and are deliberately left untouched.
fn strip_unknown_schema_fields(value: &mut Value) {
    let Value::Object(obj) = value else {
        return;
    };

    obj.retain(|key, _| KNOWN_SCHEMA_KEYS.contains(&key.as_str()));

    if let Some(Value::Object(properties)) = obj.get_mut("properties") {
        for (_, child) in properties.iter_mut() {
            strip_unknown_schema_fields(child);
        }
    }
    if let Some(items) = obj.get_mut("items") {
        match items {
            Value::Array(children) => {
                for child in children.iter_mut() {
                    strip_unknown_schema_fields(child);
                }
            }
            other => strip_unknown_schema_fields(other),
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(children)) = obj.get_mut(key) {
            for child in children.iter_mut() {
                strip_unknown_schema_fields(child);
            }
        }
    }
    if let Some(not) = obj.get_mut("not") {
        strip_unknown_schema_fields(not);
    }
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = obj.get_mut(key) {
            for (_, child) in defs.iter_mut() {
                strip_unknown_schema_fields(child);
            }
        }
    }
    if let Some(additional) = obj.get_mut("additionalProperties")
        && !matches!(additional, Value::Bool(_))
    {
        strip_unknown_schema_fields(additional);
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
    if let Some(reference) = schema.schema_ref.as_deref()
        && let Some(name) = ref_target_name(reference)
    {
        out.insert(name.to_string());
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
    if let Some(reference) = schema.schema_ref.as_deref()
        && let Some(name) = ref_target_name(reference)
    {
        out.insert(name.to_string());
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
