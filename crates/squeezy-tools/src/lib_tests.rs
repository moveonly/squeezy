use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::json;
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

#[test]
fn tool_specs_are_sorted_by_name() {
    let root = temp_workspace("tool_specs");
    let registry = ToolRegistry::new(&root).expect("registry");

    let names = registry
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["grep", "read_file", "shell", "write_file"]);

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
