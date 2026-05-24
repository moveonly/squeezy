use super::*;
use std::collections::BTreeMap;

#[test]
fn external_tool_names_are_sanitized_and_stable() {
    assert_eq!(
        external_tool_name("GitHub Docs", "list/repos"),
        "mcp__github_docs__list_repos"
    );
}

#[test]
fn arguments_must_be_json_objects() {
    assert!(arguments_object("tool", json!({"ok": true})).is_ok());
    assert!(arguments_object("tool", Value::Null).is_ok());
    assert!(arguments_object("tool", json!("bad")).is_err());
}

fn fixture_server(enabled: bool, command: Option<&str>) -> McpServerConfig {
    McpServerConfig {
        enabled,
        transport: McpTransport::Stdio,
        command: command.map(str::to_string),
        args: Vec::new(),
        url: None,
        timeout_ms: Some(500),
        env: BTreeMap::new(),
        permissions: Default::default(),
    }
}

fn fixture_tool(server: &str, raw: &str) -> ExternalMcpTool {
    ExternalMcpTool {
        server: server.to_string(),
        raw_name: raw.to_string(),
        model_name: external_tool_name(server, raw),
        description: "stale".to_string(),
        parameters: json!({"type": "object"}),
        transport: McpTransport::Stdio,
    }
}

#[test]
fn registry_reports_no_enabled_servers_when_all_disabled() {
    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), fixture_server(false, None));
    let registry = McpClientRegistry::new(servers);
    assert!(registry.has_no_enabled_servers());
}

#[tokio::test]
async fn refresh_preserves_cached_tools_when_enabled_server_discovery_fails() {
    // The server is enabled but missing a command, so stdio start fails
    // synchronously. The prior cache entry must survive the refresh.
    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), fixture_server(true, None));
    let registry = McpClientRegistry::new(servers);
    registry.insert_cached_tool_for_test(fixture_tool("docs", "lookup"));

    let errors = registry.refresh_tools(CancellationToken::new()).await;
    assert!(!errors.is_empty(), "missing-command discovery must error");
    assert!(
        registry.tool("mcp__docs__lookup").is_some(),
        "prior cached tool must survive a transient discovery failure"
    );
}

#[tokio::test]
async fn refresh_drops_cached_tools_for_disabled_servers() {
    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), fixture_server(false, None));
    let registry = McpClientRegistry::new(servers);
    registry.insert_cached_tool_for_test(fixture_tool("docs", "lookup"));
    let errors = registry.refresh_tools(CancellationToken::new()).await;
    assert!(errors.is_empty());
    assert!(
        registry.tool("mcp__docs__lookup").is_none(),
        "disabled servers should not retain cached tools"
    );
}
