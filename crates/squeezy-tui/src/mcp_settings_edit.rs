//! In-place TOML editor for the `[mcp.servers]` block.
//!
//! Called by the `/mcp` config page when persisting toggle / add /
//! remove actions. The editor function is pure (input + edit closure
//! → file on disk) so unit tests can drive it without standing up a
//! `ConfigScreenState`, and the call sites in `lib.rs` stay focused
//! on the host/agent plumbing. Empty / missing files are handled by
//! starting from an empty document, and missing parent directories
//! are created so the first edit at the Repo or Local tier never
//! fails on `ENOENT`.
//!
//! [`mcp_server_table`] lives here too because it's the serializer
//! shared by the toggle, add, and (future) edit persistence paths.
//! Keeping it next to the editor that consumes it means the
//! round-trip invariant (`from_table(mcp_server_table(c)) == c`) is
//! easy to test in one place.

use std::path::Path;

/// Run an in-place edit of `[mcp.servers]` in the TOML file at
/// `path`. Creates parent directories `0o700` and the file `0o600`
/// via the hardened atomic writer in `squeezy-core` so the first
/// write at the Repo or Local tier "just works" without exposing
/// inline secrets (provider API keys, MCP env / HTTP headers) to
/// the umask-derived default mode.
pub(crate) fn mcp_settings_edit(
    path: &Path,
    edit: impl FnOnce(&mut toml_edit::Table) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    let mcp = doc
        .as_table_mut()
        .entry("mcp")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "[mcp] is not a table".to_string(),
            )
        })?;
    let servers = mcp
        .entry("servers")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "[mcp.servers] is not a table".to_string(),
            )
        })?;
    edit(servers)?;
    squeezy_core::settings_writer::write_settings_atomic(path, doc.to_string().as_bytes())
}

/// Serialize an [`squeezy_core::McpServerConfig`] to a
/// `toml_edit::Table` containing every set field. Skips empty
/// optional values so the on-disk file stays tidy; defaults
/// round-trip through `McpServerConfig::from_table` without changing
/// observable behaviour.
///
/// Writing the **full** table — not just the field the user edited
/// — is load-bearing for the toggle path: `McpServerConfig::merge`
/// in `squeezy-core` unconditionally overwrites `transport` from the
/// higher-precedence tier, so a partial `[mcp.servers.X]` block in
/// the active tier would default the missing transport to `Stdio`
/// during merge and silently downgrade an inherited HTTP/SSE server.
pub(crate) fn mcp_server_table(server: &squeezy_core::McpServerConfig) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.insert(
        "enabled",
        toml_edit::Item::Value(toml_edit::Value::from(server.enabled)),
    );
    table.insert(
        "transport",
        toml_edit::Item::Value(toml_edit::Value::from(server.transport.as_str())),
    );
    if let Some(command) = &server.command {
        table.insert(
            "command",
            toml_edit::Item::Value(toml_edit::Value::from(command.as_str())),
        );
    }
    if !server.args.is_empty() {
        let mut array = toml_edit::Array::default();
        for arg in &server.args {
            array.push(arg.as_str());
        }
        table.insert(
            "args",
            toml_edit::Item::Value(toml_edit::Value::Array(array)),
        );
    }
    if let Some(url) = &server.url {
        table.insert(
            "url",
            toml_edit::Item::Value(toml_edit::Value::from(url.as_str())),
        );
    }
    if let Some(timeout_ms) = server.timeout_ms {
        table.insert(
            "timeout_ms",
            toml_edit::Item::Value(toml_edit::Value::from(timeout_ms as i64)),
        );
    }
    if let Some(timeout_ms) = server.discovery_timeout_ms {
        table.insert(
            "discovery_timeout_ms",
            toml_edit::Item::Value(toml_edit::Value::from(timeout_ms as i64)),
        );
    }
    if let Some(timeout_ms) = server.tool_call_timeout_ms {
        table.insert(
            "tool_call_timeout_ms",
            toml_edit::Item::Value(toml_edit::Value::from(timeout_ms as i64)),
        );
    }
    if let Some(enabled_tools) = &server.enabled_tools {
        let mut array = toml_edit::Array::default();
        for tool in enabled_tools {
            array.push(tool.as_str());
        }
        table.insert(
            "enabled_tools",
            toml_edit::Item::Value(toml_edit::Value::Array(array)),
        );
    }
    if !server.disabled_tools.is_empty() {
        let mut array = toml_edit::Array::default();
        for tool in &server.disabled_tools {
            array.push(tool.as_str());
        }
        table.insert(
            "disabled_tools",
            toml_edit::Item::Value(toml_edit::Value::Array(array)),
        );
    }
    if !server.env.is_empty() {
        let mut env = toml_edit::InlineTable::default();
        for (k, v) in &server.env {
            env.insert(k, toml_edit::Value::from(v.as_str()));
        }
        table.insert(
            "env",
            toml_edit::Item::Value(toml_edit::Value::InlineTable(env)),
        );
    }
    if let Some(env_var) = &server.bearer_token_env_var {
        table.insert(
            "bearer_token_env_var",
            toml_edit::Item::Value(toml_edit::Value::from(env_var.as_str())),
        );
    }
    if !server.http_headers.is_empty() {
        let mut headers = toml_edit::InlineTable::default();
        for (k, v) in &server.http_headers {
            headers.insert(k, toml_edit::Value::from(v.as_str()));
        }
        table.insert(
            "http_headers",
            toml_edit::Item::Value(toml_edit::Value::InlineTable(headers)),
        );
    }
    if !server.env_http_headers.is_empty() {
        let mut headers = toml_edit::InlineTable::default();
        for (k, v) in &server.env_http_headers {
            headers.insert(k, toml_edit::Value::from(v.as_str()));
        }
        table.insert(
            "env_http_headers",
            toml_edit::Item::Value(toml_edit::Value::InlineTable(headers)),
        );
    }
    // Always serialize `[mcp.servers.<name>.permissions]` when the
    // running config carries any default policy or rules. Forgetting
    // this is load-bearing: a normal persisted toggle would otherwise
    // overwrite the on-disk server table with a fresh body that
    // lacks the user's `[mcp.servers.<name>.permissions]` block,
    // silently dropping MCP guardrails (allow/ask/deny defaults and
    // per-tool rules) the user explicitly set.
    if server.permissions.default.is_some() || !server.permissions.rules.is_empty() {
        table.insert(
            "permissions",
            toml_edit::Item::Table(mcp_permissions_table(&server.permissions)),
        );
    }
    table
}

/// Serialize an [`squeezy_core::McpPermissionConfig`] into a
/// `[mcp.servers.<name>.permissions]` sub-table with a `default`
/// leaf and an `[[..rules]]` array of tables. Each rule round-trips
/// through `McpPermissionConfig::from_table` — target stays
/// server-qualified, action / source / reason / silent are emitted
/// in the same shape the loader accepts. `default_source` is
/// runtime-only (`#[serde(skip)]`) and intentionally omitted.
fn mcp_permissions_table(perms: &squeezy_core::McpPermissionConfig) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    if let Some(default) = perms.default {
        table.insert(
            "default",
            toml_edit::Item::Value(toml_edit::Value::from(default.as_str())),
        );
    }
    if !perms.rules.is_empty() {
        let mut rules = toml_edit::ArrayOfTables::new();
        for rule in &perms.rules {
            rules.push(mcp_permission_rule_table(rule));
        }
        table.insert("rules", toml_edit::Item::ArrayOfTables(rules));
    }
    table
}

fn mcp_permission_rule_table(rule: &squeezy_core::PermissionRule) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.insert(
        "target",
        toml_edit::Item::Value(toml_edit::Value::from(rule.target.as_str())),
    );
    table.insert(
        "action",
        toml_edit::Item::Value(toml_edit::Value::from(rule.action.as_str())),
    );
    // Only persist the `source` and `reason` keys when the in-memory
    // values diverge from the loader's defaults. `Project` is the
    // implicit source for unannotated rules in `from_table`; emitting
    // it unconditionally would noisily promote runtime defaults into
    // the file the user maintains by hand.
    if rule.source != squeezy_core::PermissionRuleSource::Project {
        table.insert(
            "source",
            toml_edit::Item::Value(toml_edit::Value::from(rule.source.as_str())),
        );
    }
    if let Some(reason) = &rule.reason {
        table.insert(
            "reason",
            toml_edit::Item::Value(toml_edit::Value::from(reason.as_str())),
        );
    }
    if rule.silent {
        table.insert(
            "silent",
            toml_edit::Item::Value(toml_edit::Value::from(true)),
        );
    }
    table
}

/// Return `true` when the file at `path` parses as a settings TOML
/// containing `[mcp.servers.<name>]`. Used by the scoped
/// `persist_mcp_remove` helper to flag other tiers that still
/// define a server the user just removed from the active scope —
/// the merge layer would otherwise resurrect the entry on the next
/// reload. Parse errors and missing files are treated as "not
/// defined here" because the caller is reporting hints, not
/// enforcing state.
pub(crate) fn tier_defines_mcp_server(path: &Path, name: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc.get("mcp")
        .and_then(|mcp| mcp.as_table())
        .and_then(|mcp| mcp.get("servers"))
        .and_then(|servers| servers.as_table())
        .is_some_and(|servers| servers.contains_key(name))
}

#[cfg(test)]
#[path = "mcp_settings_edit_tests.rs"]
mod tests;
