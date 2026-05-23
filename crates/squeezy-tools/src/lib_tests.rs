use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use squeezy_core::{GraphConfig, Redactor, SkillsConfig};
use tokio_util::sync::CancellationToken;

use super::*;

static WORKSPACE_NONCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn shell_permission_metadata_detects_destructive_and_compiler_commands() {
    let root = temp_workspace("permission_metadata");
    let registry = ToolRegistry::new(&root).expect("registry");

    let destructive = registry.permission_request(&ToolCall {
        call_id: "rm".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": "rm -rf target",
            "description": "clean"
        }),
    });
    assert_eq!(destructive.capability, PermissionCapability::Destructive);
    assert_eq!(destructive.risk, PermissionRisk::Critical);
    assert_eq!(destructive.target, "rm:*");

    let compiler = registry.permission_request(&ToolCall {
        call_id: "test".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": "cargo test --workspace",
            "description": "run tests"
        }),
    });
    assert_eq!(compiler.capability, PermissionCapability::Compiler);
    assert_eq!(compiler.target, "cargo test:*");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn write_file_permission_request_target_matches_suggested_rule_target() {
    let root = temp_workspace("permission_write_target");
    let registry = ToolRegistry::new(&root).expect("registry");

    let request = registry.permission_request(&ToolCall {
        call_id: "write".to_string(),
        name: "write_file".to_string(),
        arguments: json!({
            "path": "src/foo.rs",
            "content": "fn main() {}",
            "expected_sha256": "deadbeef"
        }),
    });
    assert_eq!(request.capability, PermissionCapability::Edit);
    assert_eq!(request.target, "path:src/foo.rs");
    assert_eq!(request.risk, PermissionRisk::High);
    let suggested = request
        .suggested_rules
        .first()
        .expect("write_file should propose a session rule");
    assert_eq!(
        suggested.target, request.target,
        "suggested rule target must match the request target so future calls match the persisted rule",
    );
    assert_eq!(suggested.capability, "edit");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn webfetch_and_websearch_requests_carry_expected_targets() {
    let root = temp_workspace("permission_web_targets");
    let registry = ToolRegistry::new(&root).expect("registry");

    let webfetch = registry.permission_request(&ToolCall {
        call_id: "fetch".to_string(),
        name: "webfetch".to_string(),
        arguments: json!({"url": "https://docs.rs/foo"}),
    });
    assert_eq!(webfetch.capability, PermissionCapability::Network);
    assert_eq!(webfetch.target, "domain:docs.rs");

    let websearch = registry.permission_request(&ToolCall {
        call_id: "search".to_string(),
        name: "websearch".to_string(),
        arguments: json!({"query": "rust async runtime"}),
    });
    assert_eq!(websearch.capability, PermissionCapability::Network);
    assert_eq!(websearch.target, "search:exa");

    let _ = fs::remove_dir_all(root);
}

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
async fn grep_and_glob_apply_squeezy_indexing_policy_by_default() {
    let root = temp_workspace("tool_indexing_policy");
    fs::create_dir_all(root.join("node_modules/pkg")).expect("mkdir node_modules");
    fs::write(root.join("visible.rs"), "needle\n").expect("write visible");
    fs::write(root.join("node_modules/pkg/index.ts"), "needle\n").expect("write ignored");
    let registry = ToolRegistry::new(&root).expect("registry");

    let grep_default = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(match_paths(&grep_default), vec!["visible.rs"]);

    let grep_including_ignored = registry
        .execute(
            ToolCall {
                call_id: "call_2".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle", "include_ignored": true}),
            },
            CancellationToken::new(),
        )
        .await;
    let mut paths = match_paths(&grep_including_ignored);
    paths.sort();
    assert_eq!(paths, vec!["node_modules/pkg/index.ts", "visible.rs"]);

    let glob_default = registry
        .execute(
            ToolCall {
                call_id: "call_3".to_string(),
                name: "glob".to_string(),
                arguments: json!({"pattern": "**/*.ts"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(glob_default.content["paths"], json!([]));

    let glob_including_ignored = registry
        .execute(
            ToolCall {
                call_id: "call_4".to_string(),
                name: "glob".to_string(),
                arguments: json!({"pattern": "**/*.ts", "include_ignored": true}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        glob_including_ignored.content["paths"],
        json!(["node_modules/pkg/index.ts"])
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_reports_policy_ignored_reason_and_permission_scope() {
    let root = temp_workspace("read_ignored_policy");
    fs::create_dir_all(root.join("vendor/lib")).expect("mkdir vendor");
    fs::write(
        root.join("vendor/lib/generated.rs"),
        "pub fn vendored() {}\n",
    )
    .expect("write vendored");
    let registry = ToolRegistry::new(&root).expect("registry");
    let call = ToolCall {
        call_id: "call_1".to_string(),
        name: "read_file".to_string(),
        arguments: json!({"path": "vendor/lib/generated.rs"}),
    };

    assert_eq!(
        registry.permission_scope(&call),
        PermissionScope::IgnoredSearch
    );
    let result = registry.execute(call, CancellationToken::new()).await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["ignored"], true);
    assert_eq!(result.content["ignored_reason"], "vendor");
    assert_eq!(result.content["path"], "vendor/lib/generated.rs");

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
async fn diff_context_reports_changed_file_and_dirty_symbol() {
    let root = temp_workspace("diff_context");
    write_rust_crate(
        &root,
        "pub fn changed() -> usize { 1 }\nfn caller() -> usize { changed() }\n",
    );
    git_init_commit(&root);
    fs::write(
        root.join("src/lib.rs"),
        "pub fn changed() -> usize { 2 }\nfn caller() -> usize { changed() }\n",
    )
    .expect("modify source");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "diff_context".to_string(),
                arguments: json!({"max_symbols_per_file": 10}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["summary"]["files_changed"], 1);
    assert_eq!(result.content["files"][0]["path"], "src/lib.rs");
    let symbols = result.content["graph"]["files"][0]["symbols"]
        .as_array()
        .expect("symbols");
    assert!(symbols.iter().any(|symbol| symbol["name"] == "changed"
        && !symbol["references"].as_array().unwrap().is_empty()));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn diff_only_filters_glob_grep_and_read_file() {
    let root = temp_workspace("diff_only");
    fs::write(root.join("changed.txt"), "needle before\n").expect("write changed");
    fs::write(root.join("clean.txt"), "needle clean\n").expect("write clean");
    git_init_commit(&root);
    fs::write(root.join("changed.txt"), "needle after\n").expect("modify changed");
    let registry = ToolRegistry::new(&root).expect("registry");

    let glob = registry
        .execute(
            ToolCall {
                call_id: "glob".to_string(),
                name: "glob".to_string(),
                arguments: json!({"pattern": "*.txt", "diff_only": true}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(glob.content["paths"], json!(["changed.txt"]));

    let grep = registry
        .execute(
            ToolCall {
                call_id: "grep".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle", "diff_only": true}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(match_paths(&grep), vec!["changed.txt"]);

    let clean_read = registry
        .execute(
            ToolCall {
                call_id: "read_clean".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "clean.txt", "diff_only": true}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(clean_read.status, ToolStatus::Denied);

    let changed_read = registry
        .execute(
            ToolCall {
                call_id: "read_changed".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "changed.txt", "diff_only": true}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(changed_read.status, ToolStatus::Success);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn verify_defaults_to_diff_scope_and_noops_for_non_rust_diff() {
    let root = temp_workspace("verify_noop");
    fs::write(root.join("README.md"), "before\n").expect("write readme");
    git_init_commit(&root);
    fs::write(root.join("README.md"), "after\n").expect("modify readme");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "verify".to_string(),
                name: "verify".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["scope"], "diff");
    assert_eq!(result.content["level"], "quick");
    assert_eq!(result.content["no_op"], true);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn diff_verify_command_uses_package_scoped_cargo_test() {
    let root = temp_workspace("verify_command");
    fs::create_dir_all(root.join("crates/example")).expect("create crate");
    fs::write(
        root.join("crates/example/Cargo.toml"),
        "[package]\nname = \"squeezy-example\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");

    let command = verify_command(
        &root,
        VerifyScope::Diff,
        VerifyLevel::Quick,
        &["crates/example/src/lib.rs".to_string()],
    );

    assert_eq!(command, "cargo test -p squeezy-example");

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
async fn read_file_redacts_secret_looking_content_before_returning() {
    let root = temp_workspace("read_redaction");
    fs::write(
        root.join("plain.txt"),
        "token = ghp_abcdefghijklmnopqrstuvwxyz\n",
    )
    .expect("write plain");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "plain.txt"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let content = result.content["content"].as_str().expect("content");
    assert!(!content.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
    assert!(content.contains("<redacted:"));
    assert!(result.cost_hint.redactions > 0);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn grep_and_shell_outputs_are_redacted() {
    let root = temp_workspace("tool_redaction");
    fs::write(
        root.join("app.log"),
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz\n",
    )
    .expect("write log");
    let registry = ToolRegistry::new(&root).expect("registry");

    let grep = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "Bearer", "include": ["*.log"]}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(grep.status, ToolStatus::Success);
    let grep_text = grep.model_output();
    assert!(!grep_text.contains("abcdefghijklmnopqrstuvwxyz"));
    assert!(grep_text.contains("<redacted:bearer_token"));
    assert!(grep.cost_hint.redactions > 0);

    let shell = registry
        .execute(
            ToolCall {
                call_id: "call_2".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf '%s\\n' 'OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz'",
                    "description": "print test key"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(shell.status, ToolStatus::Success);
    let stdout = shell.content["stdout"].as_str().expect("stdout");
    assert!(!stdout.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(stdout.contains("OPENAI_API_KEY="));
    assert!(shell.cost_hint.redactions > 0);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn spilled_tool_output_file_is_redacted_on_disk() {
    let root = temp_workspace("spill_redaction");
    let mut payload = String::new();
    payload.push_str("token=ghp_abcdefghijklmnopqrstuvwxyz ");
    payload.push_str(&"padding".repeat(2_000));
    fs::write(root.join("payload.txt"), &payload).expect("write payload");
    let registry = ToolRegistry::new_with_output_config(
        &root,
        ToolOutputConfig {
            spill_threshold_bytes: 256,
            preview_bytes: 32,
            retention_days: 1,
            output_dir: None,
        },
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_spill".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "payload.txt", "limit": 200_000}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["spilled"], true);
    let handle = result.content["handle"].as_str().expect("handle");
    let spill_path = root
        .canonicalize()
        .expect("canonical")
        .join(".squeezy")
        .join("tool_outputs")
        .join(format!("{handle}.json"));
    let on_disk = fs::read_to_string(&spill_path).expect("read spill");
    // Do not interpolate `on_disk` into the panic message; we already
    // know it would contain the raw secret if the assertion fires, and
    // CodeQL flags that pattern as cleartext logging.
    assert!(
        !on_disk.contains("ghp_abcdefghijklmnopqrstuvwxyz"),
        "spill file leaked raw secret: {spill_path:?}",
    );
    assert!(
        on_disk.contains("<redacted:"),
        "spill file should contain redaction marker: {spill_path:?}",
    );

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
            output_dir: None,
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
async fn relative_output_dir_resolves_against_workspace_root() {
    let root = temp_workspace("output_dir_rel");
    fs::write(root.join("sample.txt"), "x".repeat(200)).expect("write sample");
    let registry = ToolRegistry::new_with_output_config(
        &root,
        ToolOutputConfig {
            spill_threshold_bytes: 100,
            preview_bytes: 8,
            retention_days: 1,
            output_dir: Some(PathBuf::from("cache/spill")),
        },
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_rel".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "limit": 500}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let canonical_root = root.canonicalize().expect("canonical root");
    let expected_dir = canonical_root.join("cache").join("spill");
    assert!(
        expected_dir.is_dir(),
        "spill dir {expected_dir:?} should exist under the workspace root",
    );
    let handle = result.content["handle"].as_str().expect("handle");
    assert!(expected_dir.join(format!("{handle}.json")).is_file());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn absolute_output_dir_overrides_workspace_root() {
    let root = temp_workspace("output_dir_abs");
    let absolute_dir = std::env::temp_dir().join(format!(
        "squeezy_output_abs_{}_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos(),
        WORKSPACE_NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    fs::write(root.join("sample.txt"), "x".repeat(200)).expect("write sample");
    let registry = ToolRegistry::new_with_output_config(
        &root,
        ToolOutputConfig {
            spill_threshold_bytes: 100,
            preview_bytes: 8,
            retention_days: 1,
            output_dir: Some(absolute_dir.clone()),
        },
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_abs".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "limit": 500}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let handle = result.content["handle"].as_str().expect("handle");
    assert!(absolute_dir.join(format!("{handle}.json")).is_file());

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&absolute_dir);
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
async fn write_file_creates_checkpoint_and_checkpoint_undo_restores_file() {
    let root = temp_workspace("checkpoint_write_undo");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "write".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "after",
                    "expected_sha256": sha256_hex("before".as_bytes()),
                }),
            },
            CancellationToken::new(),
            "turn-1".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["checkpoint"]["group_id"], "turn-1");
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "after"
    );

    let undo = registry
        .execute(
            ToolCall {
                call_id: "undo".to_string(),
                name: "checkpoint_undo".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(undo.status, ToolStatus::Success);
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "before"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_created_file_is_checkpointed_and_deleted_on_undo() {
    let root = temp_workspace("checkpoint_shell_undo");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf created > created.txt",
                    "description": "create file"
                }),
            },
            CancellationToken::new(),
            "turn-2".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["checkpoint"]["group_id"], "turn-2");
    assert!(root.join("created.txt").exists());

    let undo = registry
        .execute(
            ToolCall {
                call_id: "undo".to_string(),
                name: "checkpoint_undo".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(undo.status, ToolStatus::Success);
    assert!(!root.join("created.txt").exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn checkpoint_undo_reports_conflict_and_preserves_dirty_user_change() {
    let root = temp_workspace("checkpoint_conflict");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "write".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "agent",
                    "expected_sha256": sha256_hex("before".as_bytes()),
                }),
            },
            CancellationToken::new(),
            "turn-3".to_string(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    fs::write(root.join("sample.txt"), "user").expect("user edit");

    let undo = registry
        .execute(
            ToolCall {
                call_id: "undo".to_string(),
                name: "checkpoint_undo".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(undo.status, ToolStatus::Stale);
    assert_eq!(
        undo.content["rollback"]["conflicts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(fs::read_to_string(root.join("sample.txt")).unwrap(), "user");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn checkpoint_undo_best_effort_restores_clean_files_while_reporting_conflict() {
    let root = temp_workspace("checkpoint_best_effort");
    fs::write(root.join("a.txt"), "before-a").expect("write a");
    fs::write(root.join("b.txt"), "before-b").expect("write b");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "write".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf agent-a > a.txt && printf agent-b > b.txt",
                    "description": "edit two files",
                }),
            },
            CancellationToken::new(),
            "turn-best-effort".to_string(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    fs::write(root.join("a.txt"), "user-a").expect("user edit");

    let undo = registry
        .execute(
            ToolCall {
                call_id: "undo".to_string(),
                name: "checkpoint_undo".to_string(),
                arguments: json!({"mode": "best_effort"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(undo.status, ToolStatus::Stale);
    assert_eq!(undo.content["rollback"]["mode"], "best_effort");
    assert_eq!(
        undo.content["rollback"]["conflicts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "user-a");
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "before-b");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn checkpoint_revert_group_restores_multiple_actions_in_reverse_order() {
    let root = temp_workspace("checkpoint_group_revert");
    fs::write(root.join("sample.txt"), "one").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    for (call_id, before, after) in [("write1", "one", "two"), ("write2", "two", "three")] {
        let result = registry
            .execute_for_group(
                ToolCall {
                    call_id: call_id.to_string(),
                    name: "write_file".to_string(),
                    arguments: json!({
                        "path": "sample.txt",
                        "content": after,
                        "expected_sha256": sha256_hex(before.as_bytes()),
                    }),
                },
                CancellationToken::new(),
                "turn-group".to_string(),
            )
            .await;
        assert_eq!(result.status, ToolStatus::Success);
    }
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "three"
    );

    let revert = registry
        .execute(
            ToolCall {
                call_id: "revert".to_string(),
                name: "checkpoint_revert".to_string(),
                arguments: json!({"group_id": "turn-group"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(revert.status, ToolStatus::Success);
    assert_eq!(fs::read_to_string(root.join("sample.txt")).unwrap(), "one");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn checkpoint_show_returns_patch_metadata_for_specific_checkpoint() {
    let root = temp_workspace("checkpoint_show");
    fs::write(root.join("sample.txt"), "before\n").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "write".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "after\n",
                    "expected_sha256": sha256_hex("before\n".as_bytes()),
                }),
            },
            CancellationToken::new(),
            "turn-show".to_string(),
        )
        .await;
    let checkpoint_id = result.content["checkpoint"]["id"]
        .as_str()
        .expect("checkpoint id")
        .to_string();

    let shown = registry
        .execute(
            ToolCall {
                call_id: "show".to_string(),
                name: "checkpoint_show".to_string(),
                arguments: json!({"checkpoint_id": checkpoint_id}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(shown.status, ToolStatus::Success);
    assert_eq!(shown.content["checkpoint"]["group_id"], "turn-show");
    assert!(
        shown.content["checkpoint"]["files"][0]["patch"]
            .as_str()
            .is_some_and(|patch| patch.contains("-before") && patch.contains("+after"))
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn suspicious_shell_mutation_reports_checkpoint_coverage_warning() {
    let warnings = shell_coverage_warnings("touch /tmp/squeezy-unprotected-test");

    assert_eq!(warnings.len(), 1);
    assert!(shell_coverage_warnings("printf ok > local.txt").is_empty());
}

#[tokio::test]
async fn shell_checkpoint_surfaces_coverage_warnings_inline() {
    let root = temp_workspace("checkpoint_inline_warnings");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "touch /tmp/squeezy-inline-warning-test && printf local > inside.txt",
                    "description": "edit local file but also touch tmp"
                }),
            },
            CancellationToken::new(),
            "turn-warn".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let warnings = result.content["checkpoint"]["coverage_warnings"]
        .as_array()
        .expect("coverage_warnings array");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|w| w.contains("outside the workspace"))),
        "expected outside-workspace warning, got {warnings:?}"
    );

    let _ = std::fs::remove_file("/tmp/squeezy-inline-warning-test");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn noop_shell_produces_no_checkpoint_so_undo_targets_real_edit() {
    let root = temp_workspace("checkpoint_noop_undo");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let edit = registry
        .execute_for_group(
            ToolCall {
                call_id: "edit".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "content": "after",
                    "expected_sha256": sha256_hex("before".as_bytes()),
                }),
            },
            CancellationToken::new(),
            "turn-edit".to_string(),
        )
        .await;
    assert_eq!(edit.status, ToolStatus::Success);
    let edit_checkpoint_id = edit.content["checkpoint"]["id"]
        .as_str()
        .expect("edit checkpoint id")
        .to_string();

    let noop = registry
        .execute_for_group(
            ToolCall {
                call_id: "noop".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "true",
                    "description": "no-op"
                }),
            },
            CancellationToken::new(),
            "turn-noop".to_string(),
        )
        .await;
    assert_eq!(noop.status, ToolStatus::Success);
    assert!(
        noop.content.get("checkpoint").is_none(),
        "no-op shell must not create a checkpoint, got {:?}",
        noop.content.get("checkpoint")
    );

    let undo = registry
        .execute(
            ToolCall {
                call_id: "undo".to_string(),
                name: "checkpoint_undo".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(undo.status, ToolStatus::Success);
    assert_eq!(
        undo.content["rollback"]["checkpoint_ids"][0], edit_checkpoint_id,
        "undo must roll back the most recent real edit, not a phantom no-op"
    );
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
            "checkpoint_list",
            "checkpoint_revert",
            "checkpoint_show",
            "checkpoint_undo",
            "diff_context",
            "glob",
            "grep",
            "list_skills",
            "load_skill",
            "read_file",
            "read_tool_output",
            "shell",
            "symbol_context",
            "verify",
            "webfetch",
            "websearch",
            "write_file"
        ]
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn skill_tools_list_metadata_and_load_body() {
    let root = temp_workspace("skill_tools");
    write_skill(&root.join(".agents/skills/rust-nav"), "rust-nav");
    let registry = ToolRegistry::new_with_configs_and_skills(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        SkillsConfig {
            user_dir: root.join("user-skills"),
            compat_user_dir: root.join("compat-skills"),
        },
        &GraphConfig::default(),
        Arc::new(Redactor::default()),
    )
    .expect("registry");

    let list = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "list_skills".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(list.status, ToolStatus::Success);
    assert_eq!(list.content["skills"][0]["name"], "rust-nav");
    assert!(list.content.to_string().contains("Rust navigation"));
    assert!(!list.content.to_string().contains("Use graph tools"));

    let loaded = registry
        .execute(
            ToolCall {
                call_id: "call_2".to_string(),
                name: "load_skill".to_string(),
                arguments: json!({"name": "rust-nav"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(loaded.status, ToolStatus::Success);
    assert_eq!(loaded.content["name"], "rust-nav");
    assert!(
        loaded.content["content"]
            .as_str()
            .is_some_and(|content| content.contains("Use graph tools"))
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
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let counter = WORKSPACE_NONCE.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "squeezy_{name}_{pid}_{base}_{counter}",
        pid = std::process::id()
    ));
    fs::create_dir_all(&root).expect("create temp workspace");
    root
}

fn write_rust_crate(root: &Path, source: &str) {
    fs::create_dir_all(root.join("src")).expect("create src");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"case\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");
    fs::write(root.join("src/lib.rs"), source).expect("write source");
}

fn git_init_commit(root: &Path) {
    run_git(root, &["init"]);
    run_git(root, &["config", "user.email", "test@example.com"]);
    run_git(root, &["config", "user.name", "Squeezy Test"]);
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-m", "initial"]);
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_skill(dir: &Path, name: &str) {
    fs::create_dir_all(dir).expect("mkdir skill");
    fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: Rust navigation\ntriggers:\n  - rust symbol\n---\n# Rust Nav\n\nUse graph tools.\n"
        ),
    )
    .expect("write skill");
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
