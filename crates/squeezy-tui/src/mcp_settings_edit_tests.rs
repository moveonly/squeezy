//! Unit tests for the `[mcp.servers]` TOML editor invoked by the
//! `/mcp` config page when persisting toggle/add/remove actions
//! (`crates/squeezy-tui/src/mcp_settings_edit.rs`). The tests drive
//! the editor directly against temp-file inputs so they cover both
//! fresh and existing settings files without standing up a
//! `ConfigScreenState`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{mcp_server_table, mcp_settings_edit, tier_defines_mcp_server};

/// Generate a unique temp path so concurrent test runs do not clash.
/// We rely on the process id plus a monotonic counter rather than
/// pulling `tempfile` into the workspace just for these tests.
fn unique_temp_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("squeezy-mcp-edit-{label}-{pid}-{n}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn mcp_settings_edit_creates_parent_dir_and_inserts_table() {
    let dir = unique_temp_dir("insert");
    // Path points at a nested directory that does NOT exist yet —
    // mirrors the first-write scenario when the Repo or Local tier
    // file has never been written.
    let path = dir.join("nested/etc/squeezy/settings.toml");
    mcp_settings_edit(&path, |servers| {
        let mut table = toml_edit::Table::new();
        table.insert(
            "enabled",
            toml_edit::Item::Value(toml_edit::Value::from(true)),
        );
        table.insert(
            "transport",
            toml_edit::Item::Value(toml_edit::Value::from("stdio")),
        );
        table.insert(
            "command",
            toml_edit::Item::Value(toml_edit::Value::from("docs-mcp")),
        );
        servers.insert("docs", toml_edit::Item::Table(table));
        Ok(())
    })
    .expect("edit succeeds");

    let text = fs::read_to_string(&path).expect("file written");
    assert!(
        text.contains("[mcp.servers.docs]"),
        "table header must be present: {text}"
    );
    assert!(text.contains("enabled = true"), "enabled persists: {text}");
    assert!(
        text.contains("command = \"docs-mcp\""),
        "command persists: {text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn mcp_settings_edit_toggles_existing_enabled_flag() {
    let dir = unique_temp_dir("toggle");
    let path = dir.join("settings.toml");
    fs::write(
        &path,
        "[mcp.servers.docs]\nenabled = true\ntransport = \"stdio\"\ncommand = \"docs-mcp\"\n",
    )
    .expect("seed file");

    mcp_settings_edit(&path, |servers| {
        let entry = servers
            .entry("docs")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let table = entry.as_table_mut().expect("table");
        table.insert(
            "enabled",
            toml_edit::Item::Value(toml_edit::Value::from(false)),
        );
        Ok(())
    })
    .expect("toggle persists");

    let text = fs::read_to_string(&path).expect("file readable");
    assert!(text.contains("enabled = false"), "toggle wrote: {text}");
    assert!(
        text.contains("command = \"docs-mcp\""),
        "sibling keys must survive: {text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn mcp_settings_edit_removes_table_entry() {
    let dir = unique_temp_dir("remove");
    let path = dir.join("settings.toml");
    fs::write(
        &path,
        "[mcp.servers.keep]\nenabled = true\ncommand = \"keep\"\n\n\
         [mcp.servers.drop]\nenabled = true\ncommand = \"drop\"\n",
    )
    .expect("seed file");

    mcp_settings_edit(&path, |servers| {
        servers.remove("drop");
        Ok(())
    })
    .expect("remove persists");

    let text = fs::read_to_string(&path).expect("file readable");
    assert!(
        !text.contains("[mcp.servers.drop]"),
        "removed entry must vanish: {text}"
    );
    assert!(
        text.contains("[mcp.servers.keep]"),
        "siblings survive removal: {text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Regression for the toggle-persist corruption: writing just
/// `enabled = false` to a higher-precedence tier used to default
/// the missing `transport` to `Stdio` during merge, silently
/// downgrading an inherited HTTP/SSE server. The toggle path now
/// serializes the **full** server config; this test pins the
/// resulting table so a future refactor cannot drop `transport`,
/// `url`, `bearer_token_env_var`, etc.
#[test]
fn mcp_server_table_preserves_http_identity() {
    let mut env = BTreeMap::new();
    env.insert("DOCS_API_KEY".to_string(), "secret-ref".to_string());
    let mut headers = BTreeMap::new();
    headers.insert("X-Origin".to_string(), "squeezy".to_string());
    let server = squeezy_core::McpServerConfig {
        enabled: false,
        transport: squeezy_core::McpTransport::Http,
        command: None,
        args: Vec::new(),
        url: Some("https://docs.example/mcp".to_string()),
        timeout_ms: Some(7_500),
        discovery_timeout_ms: None,
        tool_call_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: Vec::new(),
        env,
        permissions: squeezy_core::McpPermissionConfig::default(),
        bearer_token_env_var: Some("DOCS_BEARER".to_string()),
        http_headers: headers,
        env_http_headers: BTreeMap::new(),
    };

    let table = mcp_server_table(&server);

    // Critical invariant: the written table includes `transport =
    // "http"` so the merge layer cannot replace an inherited HTTP
    // server's transport with the default `Stdio`.
    let transport = table
        .get("transport")
        .and_then(|v| v.as_value())
        .and_then(|v| v.as_str());
    assert_eq!(transport, Some("http"));
    let enabled = table
        .get("enabled")
        .and_then(|v| v.as_value())
        .and_then(|v| v.as_bool());
    assert_eq!(enabled, Some(false));
    let url = table
        .get("url")
        .and_then(|v| v.as_value())
        .and_then(|v| v.as_str());
    assert_eq!(url, Some("https://docs.example/mcp"));
    // `command` is None on HTTP servers — the serializer must NOT
    // synthesize a placeholder, otherwise `[mcp.servers.docs]` would
    // claim to be stdio after a toggle.
    assert!(table.get("command").is_none());
    let bearer = table
        .get("bearer_token_env_var")
        .and_then(|v| v.as_value())
        .and_then(|v| v.as_str());
    assert_eq!(bearer, Some("DOCS_BEARER"));
}

/// `mcp_server_table` must round-trip the server's
/// `[mcp.servers.<name>.permissions]` block so a normal persisted
/// toggle does not silently strip the user's MCP guardrails
/// (`default` policy + per-tool rules).
#[test]
fn mcp_server_table_preserves_permissions_block() {
    use squeezy_core::{
        McpPermissionConfig, McpServerConfig, McpTransport, PermissionAction, PermissionMode,
        PermissionRule, PermissionRuleSource,
    };

    let permissions = McpPermissionConfig {
        default: Some(PermissionMode::Ask),
        rules: vec![
            PermissionRule::new(
                "mcp",
                "docs/query:*".to_string(),
                PermissionAction::Allow,
                PermissionRuleSource::User,
                Some("low-risk reads".to_string()),
            ),
            PermissionRule::new(
                "mcp",
                "docs/exec:*".to_string(),
                PermissionAction::Deny,
                PermissionRuleSource::Project,
                None,
            )
            .with_silent(true),
        ],
        ..McpPermissionConfig::default()
    };

    let server = McpServerConfig {
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("docs-mcp".to_string()),
        args: Vec::new(),
        url: None,
        timeout_ms: None,
        discovery_timeout_ms: None,
        tool_call_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: Vec::new(),
        env: BTreeMap::new(),
        permissions,
        bearer_token_env_var: None,
        http_headers: BTreeMap::new(),
        env_http_headers: BTreeMap::new(),
    };

    let table = mcp_server_table(&server);
    let perms = table
        .get("permissions")
        .and_then(|i| i.as_table())
        .expect("permissions sub-table must be present");
    let default = perms
        .get("default")
        .and_then(|i| i.as_value())
        .and_then(|v| v.as_str());
    assert_eq!(default, Some("ask"));

    let rules = perms
        .get("rules")
        .and_then(|i| i.as_array_of_tables())
        .expect("rules array of tables must be present");
    assert_eq!(rules.len(), 2);

    let first = rules.get(0).unwrap();
    assert_eq!(
        first
            .get("target")
            .and_then(|v| v.as_value())
            .and_then(|v| v.as_str()),
        Some("docs/query:*")
    );
    assert_eq!(
        first
            .get("action")
            .and_then(|v| v.as_value())
            .and_then(|v| v.as_str()),
        Some("allow")
    );
    // Source `User` differs from the project-default loader assumption,
    // so it must be emitted explicitly.
    assert_eq!(
        first
            .get("source")
            .and_then(|v| v.as_value())
            .and_then(|v| v.as_str()),
        Some("user")
    );
    assert_eq!(
        first
            .get("reason")
            .and_then(|v| v.as_value())
            .and_then(|v| v.as_str()),
        Some("low-risk reads")
    );
    assert!(
        first.get("silent").is_none(),
        "silent must be omitted unless the rule sets it"
    );

    let second = rules.get(1).unwrap();
    assert_eq!(
        second
            .get("silent")
            .and_then(|v| v.as_value())
            .and_then(|v| v.as_bool()),
        Some(true),
        "silent = true must be persisted on Deny rules"
    );
    assert!(
        second.get("source").is_none(),
        "default source (`project`) is implicit in the loader so we omit it"
    );
}

/// `mcp_settings_edit` must write through the hardened atomic
/// writer so freshly created tier files land with `0o600` on
/// Unix. The MCP settings block can hold inline secrets (env
/// values, HTTP headers, bearer-token env-var names) so an
/// umask-derived `0o644` would expose them to anyone in the user's
/// group.
#[cfg(unix)]
#[test]
fn mcp_settings_edit_writes_files_with_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = unique_temp_dir("chmod");
    let path = dir.join("settings.toml");
    mcp_settings_edit(&path, |servers| {
        servers.insert(
            "docs",
            toml_edit::Item::Table(mcp_server_table(&fixture_stdio_server("docs"))),
        );
        Ok(())
    })
    .expect("edit succeeds");
    let mode = fs::metadata(&path)
        .expect("file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "new MCP settings files must be owner-only; got {mode:o}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Construct a minimal stdio server fixture so the chmod test
/// doesn't depend on `mcp_server_table_preserves_http_identity`'s
/// HTTP-shaped fixture.
fn fixture_stdio_server(name: &str) -> squeezy_core::McpServerConfig {
    squeezy_core::McpServerConfig {
        enabled: true,
        transport: squeezy_core::McpTransport::Stdio,
        command: Some(format!("{name}-mcp")),
        args: Vec::new(),
        url: None,
        timeout_ms: None,
        discovery_timeout_ms: None,
        tool_call_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: Vec::new(),
        env: BTreeMap::new(),
        permissions: squeezy_core::McpPermissionConfig::default(),
        bearer_token_env_var: None,
        http_headers: BTreeMap::new(),
        env_http_headers: BTreeMap::new(),
    }
}

/// `tier_defines_mcp_server` is the read-only sniff that lets
/// `persist_mcp_remove` flag inherited definitions after a scoped
/// remove. It must accept settings files that don't have an
/// `[mcp]` block, ignore missing files, and only return `true`
/// when the named server is explicitly declared under `[mcp.servers]`.
#[test]
fn tier_defines_mcp_server_detects_explicit_declarations_only() {
    let dir = unique_temp_dir("tier-defines");
    let defines = dir.join("defines.toml");
    let other = dir.join("other.toml");
    let no_mcp = dir.join("no-mcp.toml");
    let missing = dir.join("missing.toml");

    fs::write(
        &defines,
        "[mcp.servers.docs]\nenabled = true\ntransport = \"stdio\"\ncommand = \"docs\"\n\n\
         [mcp.servers.other]\nenabled = true\ncommand = \"other\"\n",
    )
    .expect("seed");
    fs::write(
        &other,
        "[mcp.servers.unrelated]\nenabled = true\ncommand = \"x\"\n",
    )
    .expect("seed");
    fs::write(&no_mcp, "[tui]\ntheme = \"dark\"\n").expect("seed");

    assert!(tier_defines_mcp_server(&defines, "docs"));
    assert!(tier_defines_mcp_server(&defines, "other"));
    assert!(!tier_defines_mcp_server(&other, "docs"));
    assert!(
        !tier_defines_mcp_server(&no_mcp, "docs"),
        "files without [mcp] must report not-defined"
    );
    assert!(
        !tier_defines_mcp_server(&missing, "docs"),
        "missing files must report not-defined (no panic)"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `persist_mcp_remove` is now scoped to the active `/config` tab —
/// editing a higher tier must NOT silently rewrite a lower tier's
/// settings file. This pins the underlying editor property that
/// the scoped helper relies on: removing from `path` mutates only
/// `path` and never reaches into sibling files even when they
/// define the same server.
#[test]
fn scoped_remove_only_touches_the_file_it_is_called_against() {
    let dir = unique_temp_dir("scoped-remove");
    let active = dir.join("project.toml");
    let other = dir.join("user.toml");
    fs::write(
        &active,
        "[mcp.servers.docs]\nenabled = true\ntransport = \"stdio\"\ncommand = \"docs-mcp\"\n",
    )
    .expect("seed active scope");
    fs::write(
        &other,
        "[mcp.servers.docs]\nenabled = true\ntransport = \"http\"\nurl = \"https://docs.example/mcp\"\n",
    )
    .expect("seed user tier");

    let mut removed_from_active = false;
    mcp_settings_edit(&active, |servers| {
        removed_from_active = servers.remove("docs").is_some();
        Ok(())
    })
    .expect("active edit succeeds");
    assert!(removed_from_active);

    let active_text = fs::read_to_string(&active).unwrap();
    let other_text = fs::read_to_string(&other).unwrap();
    assert!(
        !active_text.contains("[mcp.servers.docs]"),
        "active scope drops the entry: {active_text}"
    );
    assert!(
        other_text.contains("[mcp.servers.docs]"),
        "non-active tier must survive a scoped remove: {other_text}"
    );
    let _ = fs::remove_dir_all(&dir);
}
