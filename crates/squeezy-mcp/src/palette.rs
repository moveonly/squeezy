use std::collections::BTreeMap;

use serde_json::json;
use sha2::{Digest, Sha256};
use squeezy_core::McpServerConfig;

use crate::ExternalMcpTool;

pub(crate) const MCP_TOOL_CACHE_SCHEMA_VERSION: u64 = 1;

const MAX_MODEL_TOOL_NAME_BYTES: usize = 64;
const HASH_SUFFIX_BYTES: usize = 12;

#[cfg(test)]
pub(crate) fn external_tool_name(server: &str, tool: &str) -> String {
    external_tool_name_with_prefix(&external_tool_name_prefix(server), tool)
}

pub(crate) fn external_tool_name_prefix(server: &str) -> String {
    format!("mcp__{}__", sanitize_name(server))
}

pub(crate) fn external_tool_name_with_prefix(prefix: &str, tool: &str) -> String {
    format!("{prefix}{}", sanitize_name(tool))
}

fn sanitize_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "tool".to_string()
    } else if trimmed.len() == out.len() {
        out
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn normalize_palette(tools: Vec<ExternalMcpTool>) -> BTreeMap<String, ExternalMcpTool> {
    let mut tools = tools;
    tools.sort_by(|left, right| {
        (&left.server, &left.raw_name, &left.model_name).cmp(&(
            &right.server,
            &right.raw_name,
            &right.model_name,
        ))
    });
    let mut by_base: BTreeMap<String, usize> = BTreeMap::new();
    for tool in &tools {
        *by_base.entry(tool.model_name.clone()).or_default() += 1;
    }
    let mut next = BTreeMap::new();
    for mut tool in tools {
        let force_hash = by_base
            .get(tool.model_name.as_str())
            .copied()
            .unwrap_or_default()
            > 1
            || tool.model_name.len() > MAX_MODEL_TOOL_NAME_BYTES;
        if force_hash || next.contains_key(tool.model_name.as_str()) {
            ensure_unique_model_name(&mut tool, &next, force_hash);
        }
        next.insert(tool.model_name.clone(), tool);
    }
    next
}

fn ensure_unique_model_name(
    tool: &mut ExternalMcpTool,
    existing: &BTreeMap<String, ExternalMcpTool>,
    force_initial_hash: bool,
) {
    let base = tool.model_name.clone();
    let raw_identity = format!("{}\0{}", tool.server, tool.raw_name);
    if force_initial_hash {
        tool.model_name = fit_model_name(&base, &raw_identity, true);
    }
    let mut attempt = 0u32;
    while existing.contains_key(tool.model_name.as_str()) {
        attempt = attempt.saturating_add(1);
        tool.model_name = fit_model_name(&base, &format!("{raw_identity}\0{attempt}"), true);
    }
}

fn fit_model_name(base: &str, raw_identity: &str, force_hash: bool) -> String {
    if !force_hash && base.len() <= MAX_MODEL_TOOL_NAME_BYTES {
        return base.to_string();
    }
    let hash = sha256_hex_prefix(raw_identity.as_bytes(), HASH_SUFFIX_BYTES);
    let max_prefix = MAX_MODEL_TOOL_NAME_BYTES.saturating_sub(1 + hash.len());
    let prefix = truncate_ascii(base, max_prefix);
    let mut out = String::with_capacity(prefix.len() + 1 + hash.len());
    out.push_str(&prefix);
    out.push('_');
    out.push_str(&hash);
    out
}

fn truncate_ascii(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    value.chars().take(max_bytes).collect()
}

pub(crate) fn tool_cache_key(server_name: &str, server: &McpServerConfig) -> String {
    // Hash secret values before including them in the fingerprint so the key
    // is safe to store in the on-disk DB while still invalidating when a
    // credential rotates. Include enough signal to detect auth/header changes
    // without embedding raw secrets.
    let env_value_hashes: Vec<(&str, String)> = server
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), sha256_hex_prefix(v.as_bytes(), 16)))
        .collect();
    let header_value_hashes: Vec<(&str, String)> = server
        .http_headers
        .iter()
        .map(|(k, v)| (k.as_str(), sha256_hex_prefix(v.as_bytes(), 16)))
        .collect();
    let env_http_headers_pairs: Vec<(&str, &str)> = server
        .env_http_headers
        .iter()
        .map(|(header, env_var)| (header.as_str(), env_var.as_str()))
        .collect();
    let fingerprint = json!({
        "schema": MCP_TOOL_CACHE_SCHEMA_VERSION,
        "server": server_name,
        "transport": server.transport.as_str(),
        "command": &server.command,
        "args": &server.args,
        "url": &server.url,
        "cwd": &server.cwd,
        "timeout_ms": server.timeout_ms,
        "discovery_timeout_ms": server.discovery_timeout_ms,
        "tool_call_timeout_ms": server.tool_call_timeout_ms,
        "env_value_hashes": env_value_hashes,
        "enabled_tools": &server.enabled_tools,
        "disabled_tools": &server.disabled_tools,
        "bearer_token_env_var": &server.bearer_token_env_var,
        "header_value_hashes": header_value_hashes,
        "env_http_headers": env_http_headers_pairs,
    });
    format!(
        "{server_name}\0{}",
        sha256_hex(fingerprint.to_string().as_bytes())
    )
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes.as_ref());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn sha256_hex_prefix(bytes: impl AsRef<[u8]>, max_hex_chars: usize) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes.as_ref());
    let mut output = String::with_capacity(max_hex_chars);
    for byte in digest {
        if output.len() >= max_hex_chars {
            break;
        }
        let _ = write!(output, "{byte:02x}");
    }
    output.truncate(max_hex_chars);
    output
}
