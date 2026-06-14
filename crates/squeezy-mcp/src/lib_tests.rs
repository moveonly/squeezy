use super::*;
use squeezy_store::SqueezyStore;
use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc};

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
        discovery_timeout_ms: None,
        tool_call_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: Vec::new(),
        env: BTreeMap::new(),
        permissions: Default::default(),
        bearer_token_env_var: None,
        http_headers: BTreeMap::new(),
        env_http_headers: BTreeMap::new(),
        cwd: None,
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

#[cfg(unix)]
fn fixture_client_handler(server_name: &str) -> SqueezyMcpClientHandler {
    SqueezyMcpClientHandler {
        server_name: server_name.to_string(),
        elicitation_handler: Arc::new(Mutex::new(None)),
        elicitation_policy: Arc::new(Mutex::new(PermissionMode::Ask)),
        elicitation_audit: Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(256))),
        pause_state: ElicitationPauseState::default(),
        resource_reads: Arc::new(Mutex::new(BTreeMap::new())),
        resource_declarations: Arc::new(Mutex::new(BTreeMap::new())),
        tool_list_changed: Arc::new(Notify::new()),
    }
}

fn rmcp_tool(name: &'static str) -> RmcpTool {
    RmcpTool::new(name, format!("{name} description"), JsonObject::new())
}

#[test]
fn preserve_stderr_excerpt_replaces_and_clears_stale_lines() {
    let excerpts = Arc::new(Mutex::new(BTreeMap::new()));

    preserve_stderr_excerpt(&excerpts, "docs", vec!["first".to_string()]);
    assert_eq!(
        excerpts.lock().expect("excerpt lock").get("docs").cloned(),
        Some(vec!["first".to_string()])
    );

    preserve_stderr_excerpt(&excerpts, "docs", Vec::new());
    assert!(
        !excerpts.lock().expect("excerpt lock").contains_key("docs"),
        "empty snapshots must clear stale stderr excerpts"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failed_stdio_start_preserves_stderr_excerpt() {
    let mut server = fixture_server(true, Some("/bin/sh"));
    server.args = vec![
        "-c".to_string(),
        "printf 'boot failure\\n' >&2; exit 1".to_string(),
    ];
    let excerpts = Arc::new(Mutex::new(BTreeMap::new()));

    let result = start_stdio_service(
        "docs",
        &server,
        fixture_client_handler("docs"),
        excerpts.clone(),
    )
    .await;

    assert!(matches!(result, Err(McpError::Transport { .. })));
    let lines = excerpts
        .lock()
        .expect("excerpt lock")
        .get("docs")
        .cloned()
        .unwrap_or_default();
    assert!(
        lines.iter().any(|line| line.contains("boot failure")),
        "startup stderr must survive failed handshakes: {lines:?}"
    );
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

    let outcome = registry.refresh_tools(CancellationToken::new()).await;
    assert!(
        !outcome.errors.is_empty(),
        "missing-command discovery must error"
    );
    assert!(
        registry.tool("mcp__docs__lookup").is_some(),
        "prior cached tool must survive a transient discovery failure"
    );
    let status = outcome
        .status
        .per_server
        .get("docs")
        .expect("server status");
    assert!(
        matches!(
            status,
            McpServerStatus::Stale {
                tools_count: 1,
                outcome: McpStaleOutcome::Failed { error },
            } if error.contains("missing command")
        ),
        "missing-command refresh should publish a stale cached per-server status: {status:?}"
    );
}

#[tokio::test]
async fn refresh_drops_cached_tools_for_disabled_servers() {
    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), fixture_server(false, None));
    let registry = McpClientRegistry::new(servers);
    registry.insert_cached_tool_for_test(fixture_tool("docs", "lookup"));
    let outcome = registry.refresh_tools(CancellationToken::new()).await;
    assert!(outcome.errors.is_empty());
    assert!(
        registry.tool("mcp__docs__lookup").is_none(),
        "disabled servers should not retain cached tools"
    );
}

#[tokio::test]
async fn set_server_enabled_toggles_and_publishes_status() {
    // Disabled → enabled flips the live map. Discovery will fail
    // synchronously (no command), so we only check the map mutation
    // and the failed-status row that the refresh publishes.
    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), fixture_server(false, None));
    let registry = McpClientRegistry::new(servers);
    assert!(registry.has_no_enabled_servers());

    let outcome = registry
        .set_server_enabled("docs", true, CancellationToken::new())
        .await
        .expect("known server toggles");
    assert!(!registry.has_no_enabled_servers(), "enabled flag must flip");
    assert!(
        registry.servers().get("docs").is_some_and(|s| s.enabled),
        "registry's live map must reflect the toggle"
    );
    // Discovery failed (no command) so we get a Failed status row.
    let status = outcome
        .status
        .per_server
        .get("docs")
        .expect("status published");
    assert!(
        matches!(status, McpServerStatus::Failed { .. }),
        "missing-command refresh should publish failed: {status:?}"
    );

    // Toggle back → the live map updates and the server stops
    // appearing in the published per-server status.
    let outcome = registry
        .set_server_enabled("docs", false, CancellationToken::new())
        .await
        .expect("known server toggles");
    assert!(registry.has_no_enabled_servers());
    assert!(
        !registry
            .servers()
            .get("docs")
            .map(|s| s.enabled)
            .unwrap_or(true)
    );
    assert!(
        outcome.status.per_server.is_empty(),
        "disabling the last server clears the status snapshot"
    );
}

#[tokio::test]
async fn set_server_enabled_rejects_unknown_server() {
    let registry = McpClientRegistry::new(BTreeMap::new());
    let err = registry
        .set_server_enabled("ghost", true, CancellationToken::new())
        .await
        .expect_err("unknown server must error");
    assert!(
        matches!(err, McpError::UnknownServer { ref server } if server == "ghost"),
        "expected UnknownServer, got {err:?}"
    );
}

#[tokio::test]
async fn restart_server_invalidates_session_and_runs_refresh() {
    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), fixture_server(true, None));
    let registry = McpClientRegistry::new(servers);

    // No live session yet; restart should not error and the
    // refresh should publish a Failed row (no command).
    let outcome = registry
        .restart_server("docs", CancellationToken::new())
        .await
        .expect("known server restarts");
    let status = outcome
        .status
        .per_server
        .get("docs")
        .expect("status published");
    assert!(matches!(status, McpServerStatus::Failed { .. }));

    // Restarting an unknown server is a typed error rather than a
    // silent no-op so the /mcp page can surface it.
    let err = registry
        .restart_server("ghost", CancellationToken::new())
        .await
        .expect_err("unknown server must error");
    assert!(matches!(err, McpError::UnknownServer { .. }));
}

#[test]
fn starting_status_snapshot_replaces_old_enabled_statuses() {
    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), fixture_server(true, Some("docs-mcp")));
    servers.insert("off".to_string(), fixture_server(false, Some("off-mcp")));

    let mut prior = BTreeMap::new();
    prior.insert(
        "docs".to_string(),
        McpServerStatus::Ready {
            tools_count: 3,
            cached: false,
        },
    );
    prior.insert(
        "off".to_string(),
        McpServerStatus::Failed {
            error: "old failure".to_string(),
        },
    );
    prior.insert(
        "removed".to_string(),
        McpServerStatus::Ready {
            tools_count: 1,
            cached: false,
        },
    );

    let snapshot = starting_status_snapshot(&servers, prior);
    assert_eq!(snapshot.get("docs"), Some(&McpServerStatus::Starting));
    assert!(
        !snapshot.contains_key("off"),
        "disabled servers should not preserve stale visible status"
    );
    assert!(
        !snapshot.contains_key("removed"),
        "removed servers should not preserve stale visible status"
    );
}

#[tokio::test]
async fn replace_servers_swaps_map_and_drops_cached_tools_for_removed() {
    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), fixture_server(true, None));
    let registry = McpClientRegistry::new(servers);
    registry.insert_cached_tool_for_test(fixture_tool("docs", "lookup"));
    assert!(registry.tool("mcp__docs__lookup").is_some());

    // Replace with a totally different server. Tools cached against
    // the dropped server must not survive.
    let mut next = BTreeMap::new();
    next.insert("api".to_string(), fixture_server(true, None));
    let outcome = registry
        .replace_servers(next.clone(), CancellationToken::new())
        .await;
    assert_eq!(
        registry.servers().keys().cloned().collect::<Vec<_>>(),
        vec!["api".to_string()],
        "live map should match the replacement"
    );
    assert!(
        registry.tool("mcp__docs__lookup").is_none(),
        "stale cached tool from the dropped server must be evicted"
    );
    assert!(
        outcome.status.per_server.contains_key("api"),
        "status snapshot tracks the new server"
    );
}

#[test]
fn tool_filter_applies_enabled_allowlist_before_disabled_blocklist() {
    let mut server = fixture_server(true, Some("unused"));
    server.enabled_tools = Some(vec!["read".to_string(), "delete".to_string()]);
    server.disabled_tools = vec!["delete".to_string()];

    let tools = convert_tools(
        "docs",
        &server,
        vec![rmcp_tool("read"), rmcp_tool("delete"), rmcp_tool("search")],
    );

    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].raw_name, "read");
}

#[test]
fn normalized_palette_hashes_collisions_and_fits_model_name_limit() {
    let first = fixture_tool("Same Server!", "read");
    let second = fixture_tool("Same Server?", "read");
    let long = fixture_tool(&"server".repeat(20), &"tool".repeat(20));

    let palette = normalize_palette(vec![first, second, long]);

    assert_eq!(palette.len(), 3);
    assert!(palette.keys().all(|name| name.len() <= 64));
    assert!(
        palette
            .keys()
            .filter(|name| name.starts_with("mcp__same_server__read_"))
            .count()
            == 2,
        "colliding sanitized names should be hashed into distinct model names: {palette:?}"
    );
}

#[test]
fn strip_untrusted_meta_removes_nested_meta_keys() {
    let value = json!({
        "content": [
            {
                "text": "ok",
                "_meta": {"system_prompt_override": "ignore user"},
                "nested": {"meta": {"leak": true}, "value": 1}
            }
        ],
        "meta": {"top": true}
    });

    let stripped = strip_untrusted_meta(value);

    assert_eq!(stripped["content"][0]["text"], "ok");
    assert!(stripped.get("meta").is_none());
    assert!(stripped["content"][0].get("_meta").is_none());
    assert!(stripped["content"][0]["nested"].get("meta").is_none());
}

#[test]
fn uri_templates_match_declared_segments() {
    assert!(uri_matches_template(
        "docs://api/v3/repos/openai/codex",
        "docs://api/v3/repos/{owner}/{repo}"
    ));
    assert!(uri_matches_template("db://users/rows", "db://{table}/rows"));
    assert!(uri_matches_template(
        "file:///tmp/project/a.txt",
        "file:///{path}"
    ));
    assert!(uri_matches_template(
        "file:///tmp/project/a.txt",
        "file:///{path}.txt"
    ));
    assert!(!uri_matches_template(
        "file:///etc/passwd",
        "docs://api/v3/repos/{owner}/{repo}"
    ));
    assert!(!uri_matches_template("db://users", "db://{table}/rows"));
    assert!(!uri_matches_template(
        "db://users/columns",
        "db://{table}/rows"
    ));
    assert!(!uri_matches_template(
        "db://users/rows/extra",
        "db://{table}/rows"
    ));
    assert!(!uri_matches_template("db://users/rows", "db://{table}"));
    assert!(!uri_matches_template(
        "file:///tmp/project/a.rs",
        "file:///{path}.txt"
    ));
    assert!(!uri_matches_template(
        "file:///tmp/project/a.txt?raw=1",
        "file:///{path}"
    ));
}

#[test]
fn separate_startup_and_tool_timeouts_apply() {
    // Unset → both paths fall back to the shared timeout_ms.
    let mut server = fixture_server(true, Some("unused"));
    server.timeout_ms = Some(7_500);
    assert_eq!(discovery_timeout_ms(&server), 7_500);
    assert_eq!(tool_call_timeout_ms(&server), 7_500);

    // Override only discovery → tool calls still use shared timeout_ms.
    server.discovery_timeout_ms = Some(45_000);
    assert_eq!(discovery_timeout_ms(&server), 45_000);
    assert_eq!(tool_call_timeout_ms(&server), 7_500);

    // Override only tool calls → discovery still uses its override.
    server.tool_call_timeout_ms = Some(120_000);
    assert_eq!(discovery_timeout_ms(&server), 45_000);
    assert_eq!(tool_call_timeout_ms(&server), 120_000);

    // With timeout_ms cleared the new knobs still apply; only the side
    // without an override falls back to the crate default.
    server.timeout_ms = None;
    server.discovery_timeout_ms = None;
    assert_eq!(discovery_timeout_ms(&server), DEFAULT_MCP_TIMEOUT_MS);
    assert_eq!(tool_call_timeout_ms(&server), 120_000);
}

#[tokio::test]
async fn timeout_pause_does_not_affect_other_servers() {
    let pause_state = ElicitationPauseState::default();
    let _pause = pause_state.enter("docs");
    let error = with_timeout(
        "other",
        20,
        CancellationToken::new(),
        pause_state,
        std::future::pending::<McpResult<()>>(),
    )
    .await
    .expect_err("other server must still time out");

    assert!(matches!(error, McpError::Timeout { server, .. } if server == "other"));
}

#[tokio::test]
async fn timeout_pause_suspends_same_server_until_guard_drops() {
    let pause_state = ElicitationPauseState::default();
    let pause = pause_state.enter("docs");
    let task_state = pause_state.clone();
    let handle = tokio::spawn(async move {
        with_timeout(
            "docs",
            20,
            CancellationToken::new(),
            task_state,
            std::future::pending::<McpResult<()>>(),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        !handle.is_finished(),
        "same-server pause must suspend timeout"
    );
    drop(pause);

    let error = tokio::time::timeout(Duration::from_millis(100), handle)
        .await
        .expect("timeout should resume after pause")
        .expect("task should not panic")
        .expect_err("pending future must time out");
    assert!(matches!(error, McpError::Timeout { server, .. } if server == "docs"));
}

#[tokio::test]
async fn timeout_pause_preserves_elapsed_budget() {
    let pause_state = ElicitationPauseState::default();
    let task_state = pause_state.clone();
    let handle = tokio::spawn(async move {
        with_timeout(
            "docs",
            80,
            CancellationToken::new(),
            task_state,
            std::future::pending::<McpResult<()>>(),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(30)).await;
    let pause = pause_state.enter("docs");
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(pause);

    let error = tokio::time::timeout(Duration::from_millis(70), handle)
        .await
        .expect("timeout should use remaining budget, not restart")
        .expect("task should not panic")
        .expect_err("pending future must time out");
    assert!(matches!(error, McpError::Timeout { server, .. } if server == "docs"));
}

#[test]
fn tool_cache_key_changes_when_timeout_split_changes() {
    let mut server = fixture_server(true, Some("unused"));
    let base = tool_cache_key("docs", &server);
    server.discovery_timeout_ms = Some(60_000);
    let discovery_changed = tool_cache_key("docs", &server);
    assert_ne!(base, discovery_changed);
    server.tool_call_timeout_ms = Some(90_000);
    assert_ne!(discovery_changed, tool_cache_key("docs", &server));
}

#[test]
fn tool_cache_key_changes_when_palette_filters_change() {
    let mut server = fixture_server(true, Some("unused"));
    let base = tool_cache_key("docs", &server);
    server.disabled_tools = vec!["search".to_string()];
    assert_ne!(base, tool_cache_key("docs", &server));
}

#[test]
fn tool_cache_key_changes_when_env_value_changes() {
    let mut server = fixture_server(true, Some("unused"));
    server
        .env
        .insert("MY_VAR".to_string(), "value-one".to_string());
    let base = tool_cache_key("docs", &server);
    server
        .env
        .insert("MY_VAR".to_string(), "value-two".to_string());
    assert_ne!(
        base,
        tool_cache_key("docs", &server),
        "changing an env value must invalidate the cache key"
    );
}

#[test]
fn tool_cache_key_changes_when_bearer_token_env_var_changes() {
    let mut server = fixture_server(true, Some("unused"));
    server.bearer_token_env_var = Some("OLD_TOKEN_VAR".to_string());
    let base = tool_cache_key("docs", &server);
    server.bearer_token_env_var = Some("NEW_TOKEN_VAR".to_string());
    assert_ne!(
        base,
        tool_cache_key("docs", &server),
        "changing bearer_token_env_var name must invalidate the cache key"
    );
}

#[test]
fn tool_cache_key_changes_when_http_header_value_changes() {
    let mut server = fixture_server(true, Some("unused"));
    server
        .http_headers
        .insert("X-Api-Key".to_string(), "secret-one".to_string());
    let base = tool_cache_key("docs", &server);
    server
        .http_headers
        .insert("X-Api-Key".to_string(), "secret-two".to_string());
    assert_ne!(
        base,
        tool_cache_key("docs", &server),
        "changing a static HTTP header value must invalidate the cache key"
    );
}

#[test]
fn tool_cache_key_changes_when_env_http_header_env_var_name_changes() {
    let mut server = fixture_server(true, Some("unused"));
    server
        .env_http_headers
        .insert("X-Auth".to_string(), "OLD_TOKEN_ENV".to_string());
    let base = tool_cache_key("docs", &server);
    // Renaming the backing env-var changes which credential is loaded at
    // session start — the cache must be evicted.
    server
        .env_http_headers
        .insert("X-Auth".to_string(), "NEW_TOKEN_ENV".to_string());
    assert_ne!(
        base,
        tool_cache_key("docs", &server),
        "changing the env-var name in env_http_headers must invalidate the cache key"
    );
}

#[test]
fn registry_loads_cached_tools_from_store_on_startup() {
    let root = temp_root("mcp-tool-cache");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("open store"));
    let server = fixture_server(true, Some("unused"));
    let key = tool_cache_key("docs", &server);
    store
        .put_mcp_tool_cache(
            &key,
            &McpToolCacheRecord {
                schema_version: MCP_TOOL_CACHE_SCHEMA_VERSION,
                fetched_unix_millis: unix_millis(),
                tools: vec![fixture_tool("docs", "lookup")],
            },
        )
        .expect("write mcp cache");

    let mut servers = BTreeMap::new();
    servers.insert("docs".to_string(), server);
    let registry = McpClientRegistry::new_with_store(servers, Some(store));

    assert!(registry.tool("mcp__docs__lookup").is_some());
    let snapshot = registry.status_snapshot();
    assert!(
        matches!(
            snapshot.per_server.get("docs"),
            Some(McpServerStatus::Ready {
                tools_count: 1,
                cached: true,
            })
        ),
        "expected cached ready status, got {snapshot:?}"
    );
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("squeezy-mcp-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
}

fn http_fixture_server() -> McpServerConfig {
    McpServerConfig {
        enabled: true,
        transport: McpTransport::Http,
        command: None,
        args: Vec::new(),
        url: Some("http://localhost:0/mcp".to_string()),
        timeout_ms: Some(500),
        discovery_timeout_ms: None,
        tool_call_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: Vec::new(),
        env: BTreeMap::new(),
        permissions: Default::default(),
        bearer_token_env_var: None,
        http_headers: BTreeMap::new(),
        env_http_headers: BTreeMap::new(),
        cwd: None,
    }
}

#[test]
fn build_streamable_http_config_attaches_bearer_token_when_env_var_resolves() {
    let mut server = http_fixture_server();
    server.bearer_token_env_var = Some("FAKE_TOKEN_VAR".to_string());

    let config = build_streamable_http_config(
        "slack",
        "https://example.test/mcp".to_string(),
        &server,
        |name| (name == "FAKE_TOKEN_VAR").then(|| "secret".to_string()),
    );

    // `auth_header` carries the raw token; rmcp's reqwest client applies
    // `bearer_auth` which prepends `Bearer ` to produce the final header.
    assert_eq!(config.auth_header.as_deref(), Some("secret"));
    assert!(config.custom_headers.is_empty());
}

#[test]
fn build_streamable_http_config_translates_static_headers() {
    let mut server = http_fixture_server();
    server
        .http_headers
        .insert("X-Foo".to_string(), "bar".to_string());

    let config = build_streamable_http_config(
        "slack",
        "https://example.test/mcp".to_string(),
        &server,
        |_| None,
    );

    let name = HeaderName::from_static("x-foo");
    let value = config
        .custom_headers
        .get(&name)
        .expect("X-Foo header must be present");
    assert_eq!(value.to_str().unwrap(), "bar");
    assert!(config.auth_header.is_none());
}

#[test]
fn build_streamable_http_config_skips_token_when_env_var_missing() {
    let mut server = http_fixture_server();
    server.bearer_token_env_var = Some("DEFINITELY_NOT_SET_VAR_NAME_FOR_TEST".to_string());
    server
        .http_headers
        .insert("X-Trace".to_string(), "abc".to_string());

    let config = build_streamable_http_config(
        "slack",
        "https://example.test/mcp".to_string(),
        &server,
        |_| None,
    );

    // Missing env var must NOT panic and must NOT set the auth header. Other
    // headers still need to come through so the server can connect anonymously
    // if its policy allows it.
    assert!(config.auth_header.is_none());
    let name = HeaderName::from_static("x-trace");
    assert_eq!(
        config
            .custom_headers
            .get(&name)
            .map(|v| v.to_str().unwrap()),
        Some("abc")
    );
}

#[test]
fn build_streamable_http_config_env_headers_override_static_headers() {
    let mut server = http_fixture_server();
    server
        .http_headers
        .insert("X-Trace".to_string(), "static".to_string());
    server
        .env_http_headers
        .insert("X-Trace".to_string(), "FAKE_TRACE_VAR".to_string());

    let config = build_streamable_http_config(
        "slack",
        "https://example.test/mcp".to_string(),
        &server,
        |name| (name == "FAKE_TRACE_VAR").then(|| "from-env".to_string()),
    );

    let name = HeaderName::from_static("x-trace");
    assert_eq!(
        config
            .custom_headers
            .get(&name)
            .map(|v| v.to_str().unwrap()),
        Some("from-env"),
        "env_http_headers must win over http_headers when both target the same header"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sse_transport_parses_event_data_lines_and_posts_to_advertised_endpoint() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // Drive a real legacy MCP HTTP+SSE handshake against an in-process
    // listener. The server replies to the worker's GET with a chunked
    // `text/event-stream` body that carries:
    //   * `event: endpoint` advertising the POST URL,
    //   * `event: message` carrying an initialize response so rmcp's
    //     `serve_client` can complete its handshake.
    // We then capture the worker's subsequent POST to the advertised
    // endpoint and assert it carries the configured headers — proving the
    // SSE arm uses its own transport with `event:` / `data:` parsing rather
    // than falling through to the streamable HTTP client.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (post_tx, post_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    let (get_tx, get_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();

    let accept = tokio::spawn(async move {
        // First connection: the GET that opens the SSE stream.
        let (mut sse_socket, _) = listener.accept().await.expect("accept sse");
        let mut buf = vec![0u8; 4096];
        let n = sse_socket.read(&mut buf).await.unwrap_or(0);
        buf.truncate(n);
        let _ = get_tx.send(buf);

        // Reply with an SSE stream: status + headers, then the endpoint
        // event and an initialize response framed as `event: message`.
        let endpoint_path = "/sse-messages?sid=session-1";
        let init_response = r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fixture","version":"0.0.0"}}}"#;
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let _ = sse_socket.write_all(headers.as_bytes()).await;
        // First SSE frame: endpoint URL the worker should POST to.
        let endpoint_frame = format!("event: endpoint\r\ndata: {endpoint_path}\r\n\r\n");
        write_sse_chunk(&mut sse_socket, endpoint_frame.as_bytes()).await;
        // Second SSE frame: the initialize response. Without this rmcp's
        // `serve` future never completes the handshake.
        let message_frame = format!("event: message\r\ndata: {init_response}\r\n\r\n");
        write_sse_chunk(&mut sse_socket, message_frame.as_bytes()).await;

        // Second connection: the POST the worker sends with the
        // `initialized` notification once the handshake completes.
        let (mut post_socket, _) = listener.accept().await.expect("accept post");
        let mut buf = vec![0u8; 8192];
        let n = post_socket.read(&mut buf).await.unwrap_or(0);
        buf.truncate(n);
        let _ = post_socket
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        let _ = post_socket.shutdown().await;
        let _ = post_tx.send(buf);

        // Hold the SSE socket open just long enough for the worker to
        // dispatch the POST; the drop closes the connection.
        drop(sse_socket);
    });

    let mut server = http_fixture_server();
    server.transport = McpTransport::Sse;
    server
        .http_headers
        .insert("X-Squeezy-Sse-Test".to_string(), "yes".to_string());
    server.bearer_token_env_var = Some("SQUEEZY_TEST_SSE_TOKEN".to_string());
    let sse_url = format!("http://{addr}/sse");
    server.url = Some(sse_url.clone());

    let handler = SqueezyMcpClientHandler {
        server_name: "sse-server".to_string(),
        elicitation_handler: Arc::new(Mutex::new(None)),
        pause_state: ElicitationPauseState::default(),
        elicitation_policy: Arc::new(Mutex::new(PermissionMode::Ask)),
        elicitation_audit: Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(256))),
        resource_reads: Arc::new(Mutex::new(BTreeMap::new())),
        resource_declarations: Arc::new(Mutex::new(BTreeMap::new())),
        tool_list_changed: Arc::new(tokio::sync::Notify::new()),
    };
    let (auth_header, custom_headers) =
        resolve_http_auth_and_headers("sse-server", &server, |name| match name {
            "SQUEEZY_TEST_SSE_TOKEN" => Some("fixture-secret".to_string()),
            _ => None,
        });
    let worker =
        crate::sse::build_sse_worker(sse_url, auth_header, custom_headers).expect("worker built");

    // Serve completes once the handshake exchange finishes; then we drop
    // the resulting service so its transport-close logic tears down.
    let serve = tokio::time::timeout(std::time::Duration::from_secs(5), handler.serve(worker));
    let _service = serve.await.expect("serve future timed out");
    accept.await.expect("listener task");

    let get_raw = tokio::time::timeout(std::time::Duration::from_secs(2), get_rx)
        .await
        .expect("no GET captured")
        .expect("get channel closed");
    let post_raw = tokio::time::timeout(std::time::Duration::from_secs(2), post_rx)
        .await
        .expect("no POST captured")
        .expect("post channel closed");

    let get_request = String::from_utf8_lossy(&get_raw);
    let post_request = String::from_utf8_lossy(&post_raw);

    assert!(
        get_request.starts_with("GET /sse "),
        "expected GET against /sse opening the SSE stream; got:\n{get_request}"
    );
    assert!(
        get_request.lines().any(|line| line
            .to_ascii_lowercase()
            .contains("accept: text/event-stream")),
        "GET must advertise text/event-stream so the server speaks SSE; got:\n{get_request}"
    );
    assert!(
        post_request.starts_with("POST /sse-messages?sid=session-1 "),
        "POST must target the URL advertised via `event: endpoint`; got:\n{post_request}"
    );
    assert!(
        post_request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("x-squeezy-sse-test: yes")),
        "POST must carry the static custom header; got:\n{post_request}"
    );
    assert!(
        post_request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer fixture-secret")),
        "POST must carry the resolved bearer token; got:\n{post_request}"
    );
}

async fn write_sse_chunk<W: tokio::io::AsyncWriteExt + Unpin>(writer: &mut W, body: &[u8]) {
    // Minimal chunked-encoding frame: `<size>\r\n<body>\r\n`. We never send a
    // terminating `0\r\n\r\n` because we want the stream to stay open across
    // multiple frames for the duration of the test.
    let header = format!("{:X}\r\n", body.len());
    let _ = writer.write_all(header.as_bytes()).await;
    let _ = writer.write_all(body).await;
    let _ = writer.write_all(b"\r\n").await;
    let _ = writer.flush().await;
}

#[tokio::test(flavor = "current_thread")]
async fn streamable_http_transport_sends_authorization_bearer_header() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Bind ephemeral port and capture the first inbound POST so we can assert
    // it carries the resolved bearer token. We hang up after the request so
    // the transport tears down quickly rather than negotiating a full session.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();

    let accept = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        buf.truncate(n);
        // Reply with an immediate 400 so the worker exits the initialize
        // handshake instead of waiting for an SSE stream.
        let body = b"stop";
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.write_all(body).await;
        let _ = socket.shutdown().await;
        let _ = tx.send(buf);
    });

    let mut server = http_fixture_server();
    server.bearer_token_env_var = Some("SQUEEZY_TEST_BEARER_TOKEN".to_string());
    server
        .http_headers
        .insert("X-Squeezy-Test".to_string(), "yes".to_string());
    let url = format!("http://{addr}/mcp");

    let config = build_streamable_http_config("slack", url, &server, |name| match name {
        "SQUEEZY_TEST_BEARER_TOKEN" => Some("fixture-secret".to_string()),
        _ => None,
    });
    let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);
    let handler = SqueezyMcpClientHandler {
        server_name: "slack".to_string(),
        elicitation_handler: Arc::new(Mutex::new(None)),
        elicitation_policy: Arc::new(Mutex::new(PermissionMode::Ask)),
        elicitation_audit: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        pause_state: ElicitationPauseState::default(),
        resource_reads: Arc::new(Mutex::new(BTreeMap::new())),
        resource_declarations: Arc::new(Mutex::new(BTreeMap::new())),
        tool_list_changed: Arc::new(tokio::sync::Notify::new()),
    };
    // The serve call will fail because we hang up after one round trip — that
    // is fine, we only need it to issue the initialize POST so the listener
    // captures the headers.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handler.serve(transport)).await;
    accept.await.expect("listener task");

    let raw = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("listener captured no request")
        .expect("listener channel closed");
    let request = String::from_utf8_lossy(&raw);

    assert!(
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer fixture-secret")),
        "outgoing request must carry the resolved bearer token; got:\n{request}"
    );
    assert!(
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("x-squeezy-test: yes")),
        "outgoing request must carry the static custom header; got:\n{request}"
    );
}

fn empty_form_elicitation() -> CreateElicitationRequestParams {
    CreateElicitationRequestParams::FormElicitationParams {
        meta: None,
        message: "confirm".to_string(),
        requested_schema: rmcp::model::ElicitationSchema::new(std::collections::BTreeMap::new()),
    }
}

fn required_form_elicitation() -> CreateElicitationRequestParams {
    let mut schema = rmcp::model::ElicitationSchema::new(std::collections::BTreeMap::new());
    schema.required = Some(vec!["name".to_string()]);
    CreateElicitationRequestParams::FormElicitationParams {
        meta: None,
        message: "name?".to_string(),
        requested_schema: schema,
    }
}

fn url_elicitation() -> CreateElicitationRequestParams {
    CreateElicitationRequestParams::UrlElicitationParams {
        meta: None,
        message: "open?".to_string(),
        url: "https://example.test/auth".to_string(),
        elicitation_id: "e1".to_string(),
    }
}

#[test]
fn classify_elicitation_under_ask_forwards_every_request() {
    // The default `Ask` policy must never silently accept a server-driven
    // elicitation — even one with no required fields — so the user retains
    // visibility into what each MCP server is asking for.
    let decision = classify_elicitation(PermissionMode::Ask, &empty_form_elicitation());
    assert_eq!(decision, AutoElicitationDecision::Forward);
    let decision = classify_elicitation(PermissionMode::Ask, &url_elicitation());
    assert_eq!(decision, AutoElicitationDecision::Forward);
}

#[test]
fn classify_elicitation_under_allow_auto_accepts_only_empty_forms() {
    assert_eq!(
        classify_elicitation(PermissionMode::Allow, &empty_form_elicitation()),
        AutoElicitationDecision::AutoAccept,
    );
    // A form that needs the user to supply values cannot be silently filled.
    assert_eq!(
        classify_elicitation(PermissionMode::Allow, &required_form_elicitation()),
        AutoElicitationDecision::Forward,
    );
    // URL elicitations always need user attention; "Allow" does not blanket-trust them.
    assert_eq!(
        classify_elicitation(PermissionMode::Allow, &url_elicitation()),
        AutoElicitationDecision::Forward,
    );
}

#[test]
fn classify_elicitation_under_deny_short_circuits_to_decline() {
    assert_eq!(
        classify_elicitation(PermissionMode::Deny, &empty_form_elicitation()),
        AutoElicitationDecision::AutoDecline,
    );
    assert_eq!(
        classify_elicitation(PermissionMode::Deny, &url_elicitation()),
        AutoElicitationDecision::AutoDecline,
    );
}

#[test]
fn registry_default_elicitation_policy_is_ask() {
    let registry = McpClientRegistry::new(BTreeMap::new());
    assert_eq!(registry.elicitation_policy(), PermissionMode::Ask);
}

#[test]
fn set_elicitation_policy_persists_and_is_readable() {
    let registry = McpClientRegistry::new(BTreeMap::new());
    registry.set_elicitation_policy(PermissionMode::Allow);
    assert_eq!(registry.elicitation_policy(), PermissionMode::Allow);
    registry.set_elicitation_policy(PermissionMode::Deny);
    assert_eq!(registry.elicitation_policy(), PermissionMode::Deny);
}

#[test]
fn auto_accept_emit_audit_event() {
    // Acceptance test for MCP auto-accept audit: every auto-accept must leave an audit
    // record so a host can observe whether a malicious server has been
    // silently confirming prompts. We exercise the helper that both the
    // ClientHandler path and operators use to push records, since the rmcp
    // `RequestContext<RoleClient>` cannot be constructed in a unit test.
    let log = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let request = empty_form_elicitation();
    let policy = PermissionMode::Allow;
    let decision = classify_elicitation(policy, &request);
    assert_eq!(
        decision,
        AutoElicitationDecision::AutoAccept,
        "Allow + empty form must auto-accept"
    );

    push_elicitation_audit(
        &log,
        McpElicitationAuditEvent {
            server: "docs".to_string(),
            request_id: "req-1".to_string(),
            kind: elicitation_kind(&request),
            policy,
            outcome: McpElicitationAuditOutcome::AutoAccepted,
            unix_millis: 0,
        },
    );

    let entries: Vec<McpElicitationAuditEvent> = log
        .lock()
        .map(|log| log.iter().cloned().collect())
        .unwrap_or_default();
    assert_eq!(entries.len(), 1, "auto-accept must record one audit entry");
    assert_eq!(entries[0].server, "docs");
    assert_eq!(entries[0].policy, PermissionMode::Allow);
    assert_eq!(entries[0].kind, McpElicitationKind::Form);
    assert_eq!(entries[0].outcome, McpElicitationAuditOutcome::AutoAccepted);
}

#[test]
fn audit_log_is_capacity_bounded_fifo() {
    // A misbehaving server could spam elicitations; the audit ring must drop
    // the oldest entry once the cap is hit so a flood cannot pin memory.
    let log = Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(
        MCP_AUDIT_LOG_CAPACITY,
    )));
    for index in 0..(MCP_AUDIT_LOG_CAPACITY + 16) {
        push_elicitation_audit(
            &log,
            McpElicitationAuditEvent {
                server: format!("s-{index}"),
                request_id: format!("req-{index}"),
                kind: McpElicitationKind::Form,
                policy: PermissionMode::Allow,
                outcome: McpElicitationAuditOutcome::AutoAccepted,
                unix_millis: index as u128,
            },
        );
    }
    let entries: Vec<McpElicitationAuditEvent> = log
        .lock()
        .map(|log| log.iter().cloned().collect())
        .unwrap_or_default();
    assert_eq!(entries.len(), MCP_AUDIT_LOG_CAPACITY);
    // Oldest entries are evicted first; the surviving range starts where the
    // overflow began (`+16`) so the newest record reflects the last push.
    assert_eq!(entries.first().unwrap().server, "s-16");
    assert_eq!(
        entries.last().unwrap().server,
        format!("s-{}", MCP_AUDIT_LOG_CAPACITY + 15)
    );
}

#[test]
fn registry_elicitation_audit_log_starts_empty() {
    let registry = McpClientRegistry::new(BTreeMap::new());
    assert!(registry.elicitation_audit_log().is_empty());
}

#[test]
fn client_info_advertises_squeezy_identity_and_elicitation_capability() {
    // Servers gating features on `client.capabilities.elicitation` must see
    // Squeezy declare it before sending elicitation requests; servers logging
    // peer identity must see "squeezy-mcp", not rmcp's `CARGO_CRATE_NAME`.
    let handler = SqueezyMcpClientHandler {
        server_name: "id-probe".to_string(),
        elicitation_handler: Arc::new(Mutex::new(None)),
        elicitation_policy: Arc::new(Mutex::new(PermissionMode::Ask)),
        elicitation_audit: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        pause_state: ElicitationPauseState::default(),
        resource_reads: Arc::new(Mutex::new(BTreeMap::new())),
        resource_declarations: Arc::new(Mutex::new(BTreeMap::new())),
        tool_list_changed: Arc::new(tokio::sync::Notify::new()),
    };
    let info = ClientHandler::get_info(&handler);
    assert_eq!(info.client_info.name, env!("CARGO_PKG_NAME"));
    assert_eq!(info.client_info.version, env!("CARGO_PKG_VERSION"));
    let elicitation = info
        .capabilities
        .elicitation
        .as_ref()
        .expect("elicitation capability must be advertised");
    assert!(
        elicitation.form.is_some(),
        "form elicitation must be advertised"
    );
    assert!(
        elicitation.url.is_some(),
        "url elicitation must be advertised so OAuth-style flows are gated correctly"
    );
    let experimental = info
        .capabilities
        .experimental
        .as_ref()
        .expect("experimental capabilities slot must exist for future opt-ins");
    assert!(
        experimental.is_empty(),
        "no experimental flags advertised today; slot is reserved for forward-compat"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn server_capabilities_surfaces_experimental_flags_from_initialize_response() {
    // Acceptance test for the MCP roots-declaration finding: a server declaring
    // `{ experimental: { "squeezy/test": {} } }` in its initialize response
    // must be reachable via `registry.server_capabilities("fixture")`. We
    // drive a legacy SSE handshake (cheaper than spawning a child process
    // and reusing the same pattern as the existing SSE-transport test) so
    // the discovery path actually runs `session_for` end-to-end.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Accept the SSE GET on a background task, then accept every follow-up
    // POST so the worker's `initialize` + `initialized` round-trips can both
    // complete. The shutdown signal closes the SSE channel after the test has
    // captured the live `service`, mirroring the existing SSE-transport test
    // pattern but without prematurely tearing the channel down.
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_signal = shutdown.clone();
    let accept = tokio::spawn(async move {
        let (mut sse_socket, _) = listener.accept().await.expect("accept sse");
        let mut buf = vec![0u8; 4096];
        let _ = sse_socket.read(&mut buf).await.unwrap_or(0);

        // Reply with a declared experimental capability so the registry must
        // surface it to callers asking for `server_capabilities`.
        let endpoint_path = "/sse-messages?sid=capabilities";
        let init_response = r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{"experimental":{"squeezy/test":{}}},"serverInfo":{"name":"fixture","version":"0.0.0"}}}"#;
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let _ = sse_socket.write_all(headers.as_bytes()).await;
        let endpoint_frame = format!("event: endpoint\r\ndata: {endpoint_path}\r\n\r\n");
        write_sse_chunk(&mut sse_socket, endpoint_frame.as_bytes()).await;
        let message_frame = format!("event: message\r\ndata: {init_response}\r\n\r\n");
        write_sse_chunk(&mut sse_socket, message_frame.as_bytes()).await;

        // Drain incoming POSTs (`initialize`, `notifications/initialized`,
        // any keep-alive) and respond 202 until the test signals shutdown,
        // so the worker's `serve` future completes cleanly.
        loop {
            tokio::select! {
                _ = shutdown_signal.notified() => break,
                accepted = listener.accept() => {
                    let Ok((mut post_socket, _)) = accepted else {
                        break;
                    };
                    let mut post_buf = vec![0u8; 8192];
                    let _ = post_socket.read(&mut post_buf).await.unwrap_or(0);
                    let _ = post_socket
                        .write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = post_socket.shutdown().await;
                }
            }
        }
        drop(sse_socket);
    });

    let mut server = http_fixture_server();
    server.transport = McpTransport::Sse;
    let sse_url = format!("http://{addr}/sse");
    server.url = Some(sse_url.clone());

    let handler = SqueezyMcpClientHandler {
        server_name: "fixture".to_string(),
        elicitation_handler: Arc::new(Mutex::new(None)),
        elicitation_policy: Arc::new(Mutex::new(PermissionMode::Ask)),
        elicitation_audit: Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(256))),
        pause_state: ElicitationPauseState::default(),
        resource_reads: Arc::new(Mutex::new(BTreeMap::new())),
        resource_declarations: Arc::new(Mutex::new(BTreeMap::new())),
        tool_list_changed: Arc::new(tokio::sync::Notify::new()),
    };
    let (auth_header, custom_headers) = resolve_http_auth_and_headers("fixture", &server, |_| None);
    let worker =
        crate::sse::build_sse_worker(sse_url, auth_header, custom_headers).expect("worker built");
    let serve = tokio::time::timeout(std::time::Duration::from_secs(5), handler.serve(worker));
    let service = serve
        .await
        .expect("serve future timed out")
        .expect("serve failed");
    // The listener stays alive until we signal so the worker's `initialize`
    // and `initialized` POSTs both get a 202. Closing the channel beforehand
    // surfaces as `TransportError::Closed` from rmcp.
    shutdown.notify_one();
    accept.await.expect("listener task");

    let capabilities = service
        .peer_info()
        .map(|info| info.capabilities.clone())
        .expect("peer_info must be populated after initialize");
    let experimental = capabilities
        .experimental
        .as_ref()
        .expect("server declared experimental block");
    assert!(
        experimental.contains_key("squeezy/test"),
        "experimental flag must round-trip from initialize response: {experimental:?}"
    );

    // Now plumb the captured capabilities into the registry the way
    // `session_for` does, and confirm the public accessor returns them.
    let entry = Arc::new(SessionEntry {
        service: Arc::new(service),
        _process: None,
        server_capabilities: Some(capabilities.clone()),
        stderr_ring: None,
    });
    let registry = McpClientRegistry::new(BTreeMap::new());
    assert_eq!(
        registry.aggregate_capabilities().await,
        (false, false, false),
        "empty registry must not report capabilities"
    );
    registry
        .sessions
        .lock()
        .await
        .insert("fixture".to_string(), entry);
    let observed = registry
        .server_capabilities("fixture")
        .await
        .expect("registry must surface captured capabilities");
    assert_eq!(observed, capabilities);
    let observed_experimental = observed
        .experimental
        .expect("experimental must survive accessor round-trip");
    assert!(observed_experimental.contains_key("squeezy/test"));
    assert_eq!(
        registry.aggregate_capabilities().await,
        (false, true, true),
        "connected sessions advertise client-side elicitation even when server caps do not"
    );

    assert!(
        registry
            .server_capabilities("never-connected")
            .await
            .is_none(),
        "missing server must surface as None, not a default-empty value"
    );
}

#[test]
fn resource_updated_notification_evicts_cached_read_within_ttl() {
    // A fresh read is cached well inside RESOURCE_READ_CACHE_TTL. Without an
    // eviction path an `on_resource_updated` notification would not drop it, so
    // the next read would serve the stale value until the TTL lapsed.
    let registry = McpClientRegistry::new(BTreeMap::new());
    registry.seed_resource_read_for_test("docs", "file:///a.txt", json!({"contents": "old"}));
    registry.seed_resource_read_for_test("docs", "file:///b.txt", json!({"contents": "keep"}));

    let handler = registry.client_handler_for_test("docs");
    handler.evict_resource_read("file:///a.txt");

    assert!(
        registry
            .cached_resource_read_for_test("docs", "file:///a.txt")
            .is_none(),
        "updated resource must be evicted so the next read re-fetches"
    );
    assert!(
        registry
            .cached_resource_read_for_test("docs", "file:///b.txt")
            .is_some(),
        "an unrelated resource read must stay cached"
    );
}

#[test]
fn resource_read_cache_stays_bounded_and_drops_oldest() {
    let mut cache: BTreeMap<(String, String), CachedResourceRead> = BTreeMap::new();
    let overflow = 17usize;
    let total = RESOURCE_READ_CACHE_CAPACITY + overflow;

    // Insert CAP + N distinct keys, each strictly newer than the last so
    // `fetched_at` ordering is unambiguous.
    let base = Instant::now();
    for i in 0..total {
        let key = ("srv".to_string(), format!("res://{i}"));
        insert_resource_read(
            &mut cache,
            key,
            CachedResourceRead {
                value: json!({ "i": i }),
                fetched_at: base + Duration::from_millis(i as u64),
            },
        );
        assert!(
            cache.len() <= RESOURCE_READ_CACHE_CAPACITY,
            "cache must never exceed capacity (len {} after {} inserts)",
            cache.len(),
            i + 1
        );
    }

    assert_eq!(cache.len(), RESOURCE_READ_CACHE_CAPACITY);
    // The oldest `overflow` keys must have been evicted; the newest CAP survive.
    for i in 0..overflow {
        assert!(
            !cache.contains_key(&("srv".to_string(), format!("res://{i}"))),
            "oldest key res://{i} should have been evicted"
        );
    }
    assert!(
        cache.contains_key(&("srv".to_string(), format!("res://{}", total - 1))),
        "newest key must be retained"
    );
}

#[test]
fn resource_list_changed_notification_evicts_only_that_servers_reads() {
    let registry = McpClientRegistry::new(BTreeMap::new());
    registry.seed_resource_read_for_test("docs", "file:///a.txt", json!({"contents": "a"}));
    registry.seed_resource_read_for_test("docs", "file:///b.txt", json!({"contents": "b"}));
    registry.seed_resource_read_for_test("other", "file:///c.txt", json!({"contents": "c"}));

    registry
        .client_handler_for_test("docs")
        .evict_server_resource_reads();

    assert!(
        registry
            .cached_resource_read_for_test("docs", "file:///a.txt")
            .is_none()
            && registry
                .cached_resource_read_for_test("docs", "file:///b.txt")
                .is_none(),
        "a resource-list change must drop every cached read for that server"
    );
    assert!(
        registry
            .cached_resource_read_for_test("other", "file:///c.txt")
            .is_some(),
        "other servers' cached reads must be untouched"
    );
}

#[tokio::test]
async fn resource_declaration_cache_satisfies_gate_without_enumerating() {
    let registry = McpClientRegistry::new(BTreeMap::new());
    registry.seed_resource_declarations_for_test(
        "docs",
        &["file:///a.txt"],
        &["docs://api/{owner}/{repo}"],
    );
    let server = fixture_server(true, None);

    assert!(
        registry
            .resource_uri_is_declared("docs", &server, "file:///a.txt", CancellationToken::new())
            .await
            .expect("cached exact declaration")
    );
    assert!(
        registry
            .resource_uri_is_declared(
                "docs",
                &server,
                "docs://api/openai/codex",
                CancellationToken::new()
            )
            .await
            .expect("cached template declaration")
    );
    assert!(
        !registry
            .resource_uri_is_declared(
                "docs",
                &server,
                "file:///missing.txt",
                CancellationToken::new()
            )
            .await
            .expect("cached negative declaration")
    );
}

#[test]
fn partial_resource_declaration_cache_does_not_deny_templates() {
    let registry = McpClientRegistry::new(BTreeMap::new());
    let resources = vec![Resource {
        raw: rmcp::model::RawResource {
            uri: "file:///a.txt".to_string(),
            name: "a.txt".to_string(),
            title: None,
            description: None,
            mime_type: None,
            size: None,
            icons: None,
            meta: None,
        },
        annotations: None,
    }];

    registry.store_resource_declarations_partial("docs", Some(&resources), None);

    assert_eq!(
        registry.cached_resource_declarations_match("docs", "file:///a.txt"),
        Some(true),
        "resources-only cache should answer exact positives"
    );
    assert_eq!(
        registry.cached_resource_declarations_match("docs", "docs://api/openai/codex"),
        None,
        "resources-only cache must not become a negative cache for templates"
    );
}

#[test]
fn resource_list_changed_notification_evicts_declaration_cache() {
    let registry = McpClientRegistry::new(BTreeMap::new());
    registry.seed_resource_declarations_for_test("docs", &["file:///a.txt"], &[]);
    registry.seed_resource_declarations_for_test("other", &["file:///b.txt"], &[]);

    registry
        .client_handler_for_test("docs")
        .evict_server_resource_declarations();

    assert!(
        !registry.cached_resource_declarations_for_test("docs"),
        "resource-list change must drop declarations for that server"
    );
    assert!(
        registry.cached_resource_declarations_for_test("other"),
        "other servers' declaration cache must stay intact"
    );
}

#[test]
fn resource_read_cache_prunes_expired_entries() {
    let mut cache: BTreeMap<(String, String), CachedResourceRead> = BTreeMap::new();
    // A long-stale entry whose age exceeds the TTL must be pruned on the next
    // insert, independent of the capacity cap.
    cache.insert(
        ("srv".to_string(), "res://stale".to_string()),
        CachedResourceRead {
            value: json!({ "stale": true }),
            fetched_at: Instant::now() - RESOURCE_READ_CACHE_TTL - Duration::from_secs(1),
        },
    );
    insert_resource_read(
        &mut cache,
        ("srv".to_string(), "res://fresh".to_string()),
        CachedResourceRead {
            value: json!({ "fresh": true }),
            fetched_at: Instant::now(),
        },
    );
    assert!(!cache.contains_key(&("srv".to_string(), "res://stale".to_string())));
    assert!(cache.contains_key(&("srv".to_string(), "res://fresh".to_string())));
}
