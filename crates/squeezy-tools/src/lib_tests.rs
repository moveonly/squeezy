use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;

#[tokio::test]
async fn grep_respects_gitignore_by_default_and_can_include_ignored() {
    let root = temp_workspace("grep_ignore");
    fs::write(root.join(".gitignore"), "ignored.txt\n").expect("write gitignore");
    fs::write(root.join("visible.txt"), "needle\n").expect("write visible");
    fs::write(root.join("ignored.txt"), "needle\n").expect("write ignored");
    let registry = ToolRegistry::new(&root).expect("registry");

    let default = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle"}),
            },
            CancellationToken::new(),
        )
        .await;
    let paths = match_paths(&default);
    assert_eq!(paths, vec!["visible.txt"]);

    let with_ignored = registry
        .execute(
            ToolCall {
                call_id: "call_2".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle", "include_ignored": true}),
            },
            CancellationToken::new(),
        )
        .await;
    let mut paths = match_paths(&with_ignored);
    paths.sort();
    assert_eq!(paths, vec!["ignored.txt", "visible.txt"]);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn glob_lists_paths_without_reading_content_and_respects_ignore() {
    let root = temp_workspace("glob_ignore");
    fs::write(root.join(".gitignore"), "ignored.rs\n").expect("write gitignore");
    fs::write(root.join("visible.rs"), "needle\n").expect("write visible");
    fs::write(root.join("ignored.rs"), "needle\n").expect("write ignored");
    let registry = ToolRegistry::new(&root).expect("registry");

    let default = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "glob".to_string(),
                arguments: json!({"pattern": "*.rs"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(default.status, ToolStatus::Success);
    assert_eq!(default.content["paths"], json!(["visible.rs"]));
    assert_eq!(default.cost_hint.bytes_read, 0);

    let with_ignored = registry
        .execute(
            ToolCall {
                call_id: "call_2".to_string(),
                name: "glob".to_string(),
                arguments: json!({"pattern": "*.rs", "include_ignored": true}),
            },
            CancellationToken::new(),
        )
        .await;
    let mut paths = with_ignored.content["paths"]
        .as_array()
        .expect("paths")
        .iter()
        .map(|value| value.as_str().expect("path").to_string())
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(paths, vec!["ignored.rs", "visible.rs"]);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_count_mode_returns_count_without_line_content() {
    let root = temp_workspace("grep_count");
    fs::write(root.join("one.txt"), "needle\nneedle\n").expect("write one");
    fs::write(root.join("two.txt"), "needle\n").expect("write two");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle", "output_mode": "count"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["count"], 3);
    assert!(result.content.get("matches").is_none());
    assert_eq!(result.content["metadata"]["output_mode"], "count");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_files_with_matches_mode_returns_unique_paths() {
    let root = temp_workspace("grep_files");
    fs::write(root.join("one.txt"), "needle\nneedle\n").expect("write one");
    fs::write(root.join("two.txt"), "needle\n").expect("write two");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle", "output_mode": "files_with_matches"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["paths"], json!(["one.txt", "two.txt"]));
    assert!(result.content.get("matches").is_none());
    assert_eq!(result.cost_hint.matches_returned, 2);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_keeps_scanning_after_large_truncated_file() {
    let root = temp_workspace("grep_large_first");
    fs::write(
        root.join("a_large.txt"),
        format!("{}needle", "x".repeat(256)),
    )
    .expect("write large");
    fs::write(root.join("z_match.txt"), "needle\n").expect("write match");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({
                    "pattern": "needle",
                    "output_mode": "files_with_matches",
                    "max_bytes_per_file": 8,
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["paths"], json!(["z_match.txt"]));
    assert!(result.cost_hint.truncated);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_returns_bounded_content_and_hash() {
    let root = temp_workspace("read_file");
    fs::write(root.join("sample.txt"), "abcdef").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "offset": 2, "limit": 3}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["content"], "cde");
    assert_eq!(
        result.content["sha256"],
        sha256_hex("abcdef".as_bytes()).as_str()
    );
    assert_eq!(
        result.receipt.content_sha256,
        Some(sha256_hex("abcdef".as_bytes()))
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn secret_name_checks_use_workspace_relative_paths() {
    let root = temp_workspace("secret_parent");
    fs::write(root.join("plain.txt"), "visible").expect("write plain");
    fs::write(root.join("secret.txt"), "hidden").expect("write secret");
    let registry = ToolRegistry::new(&root).expect("registry");

    let plain = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "plain.txt"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(plain.status, ToolStatus::Success);
    assert_eq!(plain.content["content"], "visible");

    let secret = registry
        .execute(
            ToolCall {
                call_id: "call_2".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "secret.txt"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(secret.status, ToolStatus::Denied);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn large_tool_output_spills_to_handle_and_can_be_read_back() {
    let root = temp_workspace("spill");
    let large = "a".repeat(30_000);
    fs::write(root.join("large.txt"), &large).expect("write large");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "large.txt", "limit": 40_000}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["spilled"], true);
    assert!(result.cost_hint.truncated);
    assert!(
        result
            .model_output()
            .len()
            .lt(&DEFAULT_TOOL_SPILL_THRESHOLD_BYTES)
    );

    let handle = result.content["handle"].as_str().expect("handle");
    let fetched = registry
        .execute(
            ToolCall {
                call_id: "call_2".to_string(),
                name: "read_tool_output".to_string(),
                arguments: json!({"handle": handle, "offset": 0, "limit": 256}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(fetched.status, ToolStatus::Success);
    assert_eq!(fetched.content["offset"], 0);
    assert_eq!(fetched.content["bytes_returned"], 256);
    assert_eq!(fetched.content["truncated"], true);
    assert!(
        fetched.content["content"]
            .as_str()
            .expect("content")
            .contains("\"tool_name\":\"read_file\"")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn output_spill_uses_registry_config() {
    let root = temp_workspace("spill_config");
    fs::write(root.join("sample.txt"), "x".repeat(200)).expect("write sample");
    let registry = ToolRegistry::new_with_output_config(
        &root,
        ToolOutputConfig {
            spill_threshold_bytes: 100,
            preview_bytes: 17,
            retention_days: 1,
        },
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "limit": 500}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["spilled"], true);
    assert_eq!(result.content["preview_bytes"], 17);
    assert!(
        result.content["handle"]
            .as_str()
            .is_some_and(|handle| handle.len() == 64)
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn write_file_rejects_stale_expected_hash() {
    let root = temp_workspace("write_file");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "after",
                    "expected_sha256": sha256_hex("other".as_bytes()),
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Stale);
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "before"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_returns_bounded_output_and_exit_code() {
    let root = temp_workspace("shell");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf abc",
                    "description": "check shell tool"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["stdout"], "abc");
    assert_eq!(result.content["exit_code"], 0);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_output_cap_is_enforced_while_command_runs() {
    let root = temp_workspace("shell_cap");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "yes x | head -c 200000",
                    "output_byte_cap": 1024,
                    "description": "large output"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert!(result.content["stdout"].as_str().expect("stdout").len() <= 1024);
    assert_eq!(result.content["truncated"], true);
    let stdout_len = result.content["stdout"].as_str().expect("stdout").len();
    let stderr_len = result.content["stderr"].as_str().expect("stderr").len();
    assert!(stdout_len + stderr_len <= 1024);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn shell_call_description_includes_actual_command() {
    let root = temp_workspace("shell_description");
    let registry = ToolRegistry::new(&root).expect("registry");
    let call = ToolCall {
        call_id: "call_1".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": "rm -rf target",
            "description": "list files"
        }),
    };

    let description = registry.describe_call(&call);

    assert!(description.contains("list files"));
    assert!(description.contains("rm -rf target"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn websearch_parser_accepts_json_and_sse_mcp_responses() {
    let payload = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"search results"}]}}"#;

    assert_eq!(
        parse_mcp_websearch_response(payload),
        Some("search results".to_string())
    );
    assert_eq!(
        parse_mcp_websearch_response(&format!(
            "event: message\ndata: {payload}\n\ndata: [DONE]\n\n"
        )),
        Some("search results".to_string())
    );
}

#[test]
fn web_tool_config_normalizes_blank_values() {
    let config = WebToolConfig {
        exa_mcp_url: "  ".to_string(),
        exa_api_key: Some("  secret-token  ".to_string()),
    }
    .normalized();

    assert_eq!(config.exa_mcp_url, DEFAULT_EXA_MCP_URL);
    assert_eq!(config.exa_api_key.as_deref(), Some("secret-token"));
}

#[test]
fn web_helpers_extract_hosts_and_classify_text_content() {
    assert_eq!(
        web_url_host("https://example.com/docs").expect("host"),
        "example.com"
    );
    assert!(is_textual_content_type("application/json; charset=utf-8"));
    assert!(is_textual_content_type("application/problem+json"));
    assert!(is_textual_content_type("image/svg+xml"));
    assert!(!is_textual_content_type("application/octet-stream"));
}

#[test]
fn web_call_descriptions_include_host_and_query() {
    let root = temp_workspace("web_descriptions");
    let registry = ToolRegistry::new(&root).expect("registry");

    assert_eq!(
        registry.describe_call(&ToolCall {
            call_id: "call_1".to_string(),
            name: "webfetch".to_string(),
            arguments: json!({"url": "https://example.com/docs"}),
        }),
        "webfetch host=\"example.com\""
    );
    assert_eq!(
        registry.describe_call(&ToolCall {
            call_id: "call_2".to_string(),
            name: "websearch".to_string(),
            arguments: json!({"query": "rust release"}),
        }),
        "websearch query=\"rust release\""
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn html_block_stripping_handles_unclosed_blocks() {
    assert_eq!(html_to_text("<main>before<script>ignored"), "before");
}

#[tokio::test]
async fn websearch_sends_exa_mcp_request_and_returns_text() {
    let root = temp_workspace("websearch");
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"search results"}]}}"#;
    let http = Arc::new(MockWebHttpClient::default());
    http.push_post_response(ok_response("application/json", body.as_bytes()));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig {
            exa_mcp_url: "https://search.example/mcp".to_string(),
            exa_api_key: Some("secret-token".to_string()),
        },
        http.clone(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "websearch".to_string(),
                arguments: json!({
                    "query": "rust async",
                    "num_results": 3,
                    "search_type": "fast",
                    "livecrawl": "preferred",
                    "context_max_characters": 1200,
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["provider"], "exa");
    assert_eq!(result.content["query"], "rust async");
    assert_eq!(result.content["result"], "search results");
    let requests = http.post_requests.lock().expect("post requests");
    assert_eq!(requests[0].url, "https://search.example/mcp");
    assert!(
        requests[0]
            .headers
            .contains(&("x-api-key".to_string(), "secret-token".to_string()))
    );
    assert_eq!(requests[0].body["params"]["name"], "web_search_exa");
    assert_eq!(
        requests[0].body["params"]["arguments"]["query"],
        "rust async"
    );
    assert_eq!(requests[0].body["params"]["arguments"]["numResults"], 3);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn websearch_rejects_invalid_arguments() {
    let root = temp_workspace("websearch_invalid_args");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "websearch".to_string(),
                arguments: json!({"num_results": 3}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("invalid tool arguments")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn websearch_rejects_empty_queries_without_http_request() {
    let root = temp_workspace("websearch_empty");
    let http = Arc::new(MockWebHttpClient::default());
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http.clone(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "websearch".to_string(),
                arguments: json!({"query": "  "}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("query must not be empty")
    );
    assert!(http.post_requests.lock().expect("post requests").is_empty());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn websearch_sends_deep_search_requests() {
    let root = temp_workspace("websearch_deep");
    let body =
        r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"deep results"}]}}"#;
    let http = Arc::new(MockWebHttpClient::default());
    http.push_post_response(ok_response("application/json", body.as_bytes()));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http.clone(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "websearch".to_string(),
                arguments: json!({"query": "rust", "search_type": "deep"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["metadata"]["search_type"], "deep");
    assert_eq!(
        http.post_requests.lock().expect("post requests")[0].body["params"]["arguments"]["type"],
        "deep"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn websearch_reports_provider_http_errors() {
    let root = temp_workspace("websearch_http_error");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_post_response(web_response(
        503,
        vec![("content-type", "application/json")],
        br#"{"error":"unavailable"}"#,
    ));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "websearch".to_string(),
                arguments: json!({"query": "rust"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("HTTP 503")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn websearch_reports_missing_text_content() {
    let root = temp_workspace("websearch_no_text");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_post_response(ok_response(
        "application/json",
        br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"   "}]}}"#,
    ));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "websearch".to_string(),
                arguments: json!({"query": "rust"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("no text content")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn websearch_reports_http_client_errors() {
    let root = temp_workspace("websearch_client_error");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_post_error("network unavailable");
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "websearch".to_string(),
                arguments: json!({"query": "rust"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("network unavailable")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_rejects_invalid_arguments() {
    let root = temp_workspace("webfetch_invalid_args");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("invalid tool arguments")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_strips_html_scripts_and_styles() {
    let root = temp_workspace("webfetch_html");
    let html = "<html><head><style>.x{}</style><script>alert(1)</script></head><body>Hello <b>world</b> &amp; docs</body></html>";
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(ok_response("text/html", html.as_bytes()));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http.clone(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/docs", "format": "text"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["format"], "text");
    assert_eq!(result.content["content"], "Hello world & docs");
    let requests = http.get_requests.lock().expect("get requests");
    assert_eq!(*requests, vec!["https://example.com/docs".to_string()]);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_html_format_returns_raw_html() {
    let root = temp_workspace("webfetch_html_format");
    let html = "<html><body>Hello <b>world</b></body></html>";
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(ok_response("text/html", html.as_bytes()));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/docs", "format": "html"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["format"], "html");
    assert_eq!(result.content["content"], html);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_reports_http_status_errors() {
    let root = temp_workspace("webfetch_http_error");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(web_response(
        404,
        vec![("content-type", "text/plain")],
        b"missing",
    ));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/missing"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("HTTP status 404")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_rejects_unsupported_content_types() {
    let root = temp_workspace("webfetch_binary");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(web_response(
        200,
        vec![("content-type", "application/octet-stream")],
        b"\x00\x01\x02",
    ));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/blob"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("unsupported content type")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_reports_cross_host_redirect_without_following() {
    let root = temp_workspace("webfetch_redirect");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(redirect_response("https://example.net/next"));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http.clone(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/start"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(result.content["redirect_url"], "https://example.net/next");
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("redirect to another host")
    );
    let requests = http.get_requests.lock().expect("get requests");
    assert_eq!(*requests, vec!["https://example.com/start".to_string()]);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_reports_redirects_without_location() {
    let root = temp_workspace("webfetch_redirect_no_location");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(web_response(302, Vec::new(), b""));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/start"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("did not include a location")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_follows_same_host_redirects() {
    let root = temp_workspace("webfetch_same_host_redirect");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(redirect_response("/next"));
    http.push_get_response(ok_response("text/plain", b"redirected body"));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http.clone(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/start"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["url"], "https://example.com/next");
    assert_eq!(result.content["content"], "redirected body");
    let requests = http.get_requests.lock().expect("get requests");
    assert_eq!(
        *requests,
        vec![
            "https://example.com/start".to_string(),
            "https://example.com/next".to_string()
        ]
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_reports_too_many_redirects() {
    let root = temp_workspace("webfetch_redirect_loop");
    let http = Arc::new(MockWebHttpClient::default());
    for index in 0..=MAX_WEB_REDIRECTS {
        http.push_get_response(redirect_response(&format!("/next-{index}")));
    }
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http.clone(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/start"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("too many redirects")
    );
    assert_eq!(
        http.get_requests.lock().expect("get requests").len(),
        MAX_WEB_REDIRECTS + 1
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_rejects_non_http_urls() {
    let root = temp_workspace("webfetch_scheme");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "file:///tmp/secret"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("http:// or https://")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_reports_http_client_errors() {
    let root = temp_workspace("webfetch_client_error");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_error("offline");
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/page"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("offline")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn large_webfetch_output_spills_to_handle() {
    let root = temp_workspace("webfetch_spill");
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(ok_response("text/plain", "w".repeat(30_000).as_bytes()));
    let registry = ToolRegistry::new_with_http_client(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        http,
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({"url": "https://example.com/large", "output_byte_cap": 40_000}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["spilled"], true);
    assert!(
        result.content["handle"]
            .as_str()
            .is_some_and(|handle| handle.len() == 64)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn tool_specs_are_sorted_by_name() {
    let root = temp_workspace("tool_specs");
    let registry = ToolRegistry::new(&root).expect("registry");

    let names = registry
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "glob",
            "grep",
            "read_file",
            "read_tool_output",
            "shell",
            "webfetch",
            "websearch",
            "write_file"
        ]
    );

    let _ = fs::remove_dir_all(root);
}

fn match_paths(result: &ToolResult) -> Vec<String> {
    result.content["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|value| value["path"].as_str().expect("path").to_string())
        .collect()
}

fn temp_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy_{name}_{nonce}"));
    fs::create_dir_all(&root).expect("create temp workspace");
    root
}

#[derive(Debug, Clone)]
struct MockPostRequest {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

#[derive(Debug, Default)]
struct MockWebHttpClient {
    post_requests: Mutex<Vec<MockPostRequest>>,
    get_requests: Mutex<Vec<String>>,
    post_responses: Mutex<VecDeque<std::result::Result<WebHttpResponse, String>>>,
    get_responses: Mutex<VecDeque<std::result::Result<WebHttpResponse, String>>>,
}

impl MockWebHttpClient {
    fn push_post_response(&self, response: WebHttpResponse) {
        self.post_responses
            .lock()
            .expect("post responses")
            .push_back(Ok(response));
    }

    fn push_post_error(&self, error: &str) {
        self.post_responses
            .lock()
            .expect("post responses")
            .push_back(Err(error.to_string()));
    }

    fn push_get_response(&self, response: WebHttpResponse) {
        self.get_responses
            .lock()
            .expect("get responses")
            .push_back(Ok(response));
    }

    fn push_get_error(&self, error: &str) {
        self.get_responses
            .lock()
            .expect("get responses")
            .push_back(Err(error.to_string()));
    }
}

impl WebHttpClient for MockWebHttpClient {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: Value,
        _max_response_bytes: usize,
    ) -> WebHttpFuture<'a> {
        let result = {
            self.post_requests
                .lock()
                .expect("post requests")
                .push(MockPostRequest {
                    url: url.to_string(),
                    headers,
                    body,
                });
            self.post_responses
                .lock()
                .expect("post responses")
                .pop_front()
                .unwrap_or_else(|| Err("unexpected websearch request".to_string()))
        };
        Box::pin(async move { result })
    }

    fn get<'a>(&'a self, url: Url, _max_response_bytes: usize) -> WebHttpFuture<'a> {
        let result = {
            self.get_requests
                .lock()
                .expect("get requests")
                .push(url.to_string());
            self.get_responses
                .lock()
                .expect("get responses")
                .pop_front()
                .unwrap_or_else(|| Err("unexpected webfetch request".to_string()))
        };
        Box::pin(async move { result })
    }
}

fn ok_response(content_type: &str, body: &[u8]) -> WebHttpResponse {
    web_response(200, vec![("content-type", content_type)], body)
}

fn redirect_response(location: &str) -> WebHttpResponse {
    web_response(302, vec![("location", location)], b"")
}

fn web_response(status: u16, headers: Vec<(&str, &str)>, body: &[u8]) -> WebHttpResponse {
    WebHttpResponse {
        status,
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
            .collect(),
        body: body.to_vec(),
    }
}
