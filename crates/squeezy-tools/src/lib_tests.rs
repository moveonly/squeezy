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
use squeezy_core::{GraphConfig, SkillConfigEntry, SkillsConfig};
use squeezy_store::{SqueezyStore, StoredReadSnapshot};
use tokio_util::sync::CancellationToken;

use super::*;

static WORKSPACE_NONCE: AtomicU64 = AtomicU64::new(0);

fn registry_with_shell_sandbox_off(root: &Path) -> ToolRegistry {
    registry_with_shell_sandbox_off_and_output_config(root, ToolOutputConfig::default())
}

fn registry_with_checkpoints(root: &Path) -> ToolRegistry {
    registry_with_runtime_config(
        root,
        ToolRuntimeConfig {
            checkpoints_enabled: true,
            ..ToolRuntimeConfig::default()
        },
    )
}

fn registry_with_shell_sandbox_off_and_checkpoints(root: &Path) -> ToolRegistry {
    registry_with_runtime_config(
        root,
        ToolRuntimeConfig {
            shell_sandbox: squeezy_core::ShellSandboxConfig {
                mode: squeezy_core::ShellSandboxMode::Off,
                ..squeezy_core::ShellSandboxConfig::default()
            },
            checkpoints_enabled: true,
            ..ToolRuntimeConfig::default()
        },
    )
}

fn registry_with_runtime_config(root: &Path, config: ToolRuntimeConfig) -> ToolRegistry {
    ToolRegistry::new_with_configs_skills_and_mcp(
        root,
        config,
        SkillsConfig::default(),
        &GraphConfig::default(),
        ToolRegistryRuntime::default(),
    )
    .expect("registry")
}

fn registry_with_shell_sandbox_off_and_output_config(
    root: &Path,
    output_config: ToolOutputConfig,
) -> ToolRegistry {
    let shell_sandbox = squeezy_core::ShellSandboxConfig {
        mode: squeezy_core::ShellSandboxMode::Off,
        ..squeezy_core::ShellSandboxConfig::default()
    };
    ToolRegistry::new_inner(
        root,
        output_config,
        WebToolConfig::default(),
        shell_sandbox,
        SkillCatalog::empty(),
        CrawlOptions::default(),
        ToolRegistryRuntime::default(),
    )
    .expect("registry")
}

fn registry_with_state_store(root: &Path, store: Arc<SqueezyStore>) -> ToolRegistry {
    ToolRegistry::new_inner(
        root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        squeezy_core::ShellSandboxConfig::default(),
        SkillCatalog::empty(),
        CrawlOptions::default(),
        ToolRegistryRuntime::new(Some(store), Arc::new(Redactor::default())),
    )
    .expect("registry")
}

#[test]
fn shell_permission_metadata_detects_destructive_and_compiler_commands() {
    let root = temp_workspace("permission_metadata");
    let registry = registry_with_shell_sandbox_off(&root);

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
    assert_eq!(destructive.metadata["cwd"], ".");
    assert_eq!(destructive.metadata["destructive"], "true");

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

    let refresh = registry.permission_request(&ToolCall {
        call_id: "facts".to_string(),
        name: "refresh_compiler_facts".to_string(),
        arguments: json!({"diagnostics": true}),
    });
    assert_eq!(refresh.capability, PermissionCapability::Compiler);
    assert_eq!(refresh.target, "cargo facts+check:*");
    assert_eq!(refresh.metadata["diagnostics"], "true");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn shell_permission_metadata_classifies_read_only_listing_commands_as_low_risk_search() {
    let root = temp_workspace("permission_read_only_shell");
    let registry = registry_with_shell_sandbox_off(&root);

    let request = registry.permission_request(&ToolCall {
        call_id: "ls".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": "ls",
            "description": "list workspace files"
        }),
    });

    assert_eq!(request.capability, PermissionCapability::Search);
    assert_eq!(request.risk, PermissionRisk::Low);
    assert_eq!(request.target, "ls:*");
    assert_eq!(request.metadata["destructive"], "false");
    assert_eq!(request.metadata["network"], "none");

    let grep = analyze_shell_command("rg getFoo");
    assert_eq!(grep.capability, PermissionCapability::Search);
    assert_eq!(grep.risk, PermissionRisk::Low);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn shell_permission_metadata_detects_network_commands() {
    let root = temp_workspace("permission_network_metadata");
    let registry = registry_with_shell_sandbox_off(&root);
    fs::create_dir_all(root.join("src")).expect("mkdir src");

    let request = registry.permission_request(&ToolCall {
        call_id: "curl".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": "curl https://example.com",
            "workdir": "src",
            "timeout_ms": 1000,
            "output_byte_cap": 2048,
            "description": "fetch"
        }),
    });

    assert_eq!(request.capability, PermissionCapability::Network);
    assert_eq!(request.risk, PermissionRisk::High);
    assert_eq!(request.target, "shell:curl:example.com");
    assert_eq!(request.metadata["network"], "classified");
    assert_eq!(request.metadata["cwd"], "src");
    assert_eq!(request.metadata["timeout_ms"], "1000");
    assert_eq!(request.metadata["output_byte_cap"], "2048");
    assert!(request.metadata["env"].contains("allowlist"));

    let git_clone = registry.permission_request(&ToolCall {
        call_id: "git".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": "git clone https://example.com/repo.git",
            "description": "clone"
        }),
    });
    assert_eq!(git_clone.capability, PermissionCapability::Network);
    assert_eq!(git_clone.target, "shell:git clone:example.com");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn shell_prefix_analysis_handles_env_assignments_and_bare_shell_wrappers() {
    let safe_env = analyze_shell_command("CI=1 cargo test --workspace");
    assert_eq!(safe_env.capability, PermissionCapability::Compiler);
    assert_eq!(safe_env.rule_target, "cargo test:*");
    assert!(safe_env.parser_backed);

    let secret_env = analyze_shell_command("OPENAI_API_KEY=sk-test cargo test --workspace");
    assert_eq!(secret_env.capability, PermissionCapability::Shell);
    assert_eq!(secret_env.rule_target, "shell:*");

    let bare_shell = analyze_shell_command("bash -lc 'cargo test'");
    assert_eq!(bare_shell.capability, PermissionCapability::Shell);
    assert_eq!(bare_shell.rule_target, "shell:*");

    let destructive_network = analyze_shell_command("git push --force origin main");
    assert_eq!(
        destructive_network.capability,
        PermissionCapability::Destructive
    );
    assert_eq!(destructive_network.risk, PermissionRisk::Critical);
    assert!(destructive_network.network);
}

#[test]
fn shell_parser_respects_quoted_operators_and_marks_dynamic_commands() {
    let quoted_segments = shell_segments("printf 'a;b' && cargo test");
    assert_eq!(quoted_segments, ["printf 'a;b'", "cargo test"]);

    let pipe_segments = shell_segments("rg needle | xargs rm -rf target || printf 'a|b'");
    assert_eq!(
        pipe_segments,
        ["rg needle", "xargs rm -rf target", "printf 'a|b'"]
    );

    let dynamic = analyze_shell_command("echo $(cat file)");
    assert!(dynamic.parser_backed);
    assert!(dynamic.dynamic);
    assert_eq!(dynamic.capability, PermissionCapability::Shell);
    assert_eq!(dynamic.rule_target, "shell:*");

    let destructive_pipeline = analyze_shell_command("rg needle | xargs rm -rf target");
    assert_eq!(
        destructive_pipeline.capability,
        PermissionCapability::Destructive
    );
    assert!(destructive_pipeline.destructive);
}

#[test]
fn heredoc_prefix_is_classified_as_command() {
    let analysis = analyze_shell_command("python3 <<'PY'\nprint('hi')\nPY\n");
    assert!(analysis.parser_backed);
    assert!(!analysis.dynamic);
    assert_eq!(analysis.rule_target, "python3:*");
    assert_eq!(analysis.risk, PermissionRisk::Medium);
}

#[test]
fn shell_environment_policy_preserves_only_safe_names() {
    let allowlist = squeezy_core::ShellSandboxConfig::default().env_allowlist;
    assert!(shell_env_should_preserve("PATH", &allowlist));
    assert!(shell_env_should_preserve("CARGO_HOME", &allowlist));
    assert!(shell_env_should_preserve("LC_ALL", &allowlist));

    assert!(!shell_env_should_preserve("OPENAI_API_KEY", &allowlist));
    assert!(!shell_env_should_preserve(
        "AWS_SECRET_ACCESS_KEY",
        &allowlist
    ));
    assert!(!shell_env_should_preserve("SSH_AUTH_SOCK", &allowlist));
    assert!(!shell_env_should_preserve("GITHUB_TOKEN", &allowlist));
}

#[test]
fn write_file_permission_request_target_matches_suggested_rule_target() {
    let root = temp_workspace("permission_write_target");
    let registry = registry_with_shell_sandbox_off(&root);

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
    let registry = registry_with_shell_sandbox_off(&root);

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
async fn read_file_returns_dedup_stub_when_unchanged_since_last_receipt() {
    let root = temp_workspace("read_file_dedup_unchanged");
    // Use a multi-KB body so the audit's "stub output < full output / 10"
    // ratio is meaningful — the stub is a small fixed-size JSON object.
    let body = "x".repeat(20_000);
    fs::write(root.join("sample.txt"), &body).expect("write sample");

    // First call: no snapshot exists, full payload returned.
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    let registry = registry_with_state_store(&root, Arc::clone(&store));
    let first = registry
        .execute(
            ToolCall {
                call_id: "first_read".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "limit": body.len()}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(first.status, ToolStatus::Success);
    assert!(first.content.get("dedup").is_none());
    assert!(first.cost_hint.output_bytes >= body.len() as u64);

    // Manually persist the snapshot the agent normally would after a read.
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: first.call_id.clone(),
            stable_output_sha256: first.receipt.output_sha256.clone(),
            content_sha256: first.receipt.content_sha256.clone(),
            start_byte: 0,
            end_byte: body.len() as u64,
            content: body.clone(),
            model_output_bytes: first.cost_hint.output_bytes as usize,
            created_unix_millis: 1,
        })
        .expect("put snapshot");

    // Second call with same args: dedup stub.
    let second = registry
        .execute(
            ToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "limit": body.len()}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(second.status, ToolStatus::Success);
    assert_eq!(second.content["dedup"], true);
    assert_eq!(second.content["receipt_stub"], true);
    assert_eq!(second.content["unchanged"], true);
    assert_eq!(second.content["same_as_call_id"], "first_read");
    assert_eq!(second.content["bytes_returned"], 0);
    assert!(
        second.cost_hint.output_bytes * 10 < first.cost_hint.output_bytes,
        "dedup stub output_bytes {} not <10x smaller than full payload {}",
        second.cost_hint.output_bytes,
        first.cost_hint.output_bytes
    );
    assert_eq!(second.receipt.content_sha256, first.receipt.content_sha256);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_does_not_dedup_when_content_changed() {
    let root = temp_workspace("read_file_dedup_changed");
    let prior = "alpha\nbeta\ngamma\n";
    let current = "alpha\nDELTA\ngamma\n";
    fs::write(root.join("sample.txt"), current).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex(prior.as_bytes())),
            start_byte: 0,
            end_byte: prior.len() as u64,
            content: prior.to_string(),
            model_output_bytes: 256,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "limit": current.len()}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert!(result.content.get("dedup").is_none());
    assert_eq!(result.content["content"], current);
    assert_eq!(
        result.receipt.content_sha256,
        Some(sha256_hex(current.as_bytes()))
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_file_does_not_dedup_when_window_differs() {
    let root = temp_workspace("read_file_dedup_window");
    let body = "alpha\nbeta\ngamma\n";
    fs::write(root.join("sample.txt"), body).expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    // Prior snapshot covered bytes [0, 5) only ("alpha").
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex(body.as_bytes())),
            start_byte: 0,
            end_byte: 5,
            content: "alpha".to_string(),
            model_output_bytes: 64,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    // Request a different window: bytes [6, 10) ("beta").
    let result = registry
        .execute(
            ToolCall {
                call_id: "second_read".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "sample.txt", "offset": 6, "limit": 4}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert!(result.content.get("dedup").is_none());
    assert_eq!(result.content["content"], "beta");

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
async fn read_slice_diff_mode_returns_only_changed_worktree_ranges() {
    let root = temp_workspace("read_slice_diff_worktree");
    write_rust_crate(
        &root,
        "pub fn changed() -> usize { 1 }\npub fn same() -> usize { 1 }\n",
    );
    git_init_commit(&root);
    fs::write(
        root.join("src/lib.rs"),
        "pub fn changed() -> usize { 2 }\npub fn same() -> usize { 1 }\n",
    )
    .expect("modify source");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "diff".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "src/lib.rs",
                    "read_mode": "diff",
                    "diff_baseline": "worktree"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["read_mode"], "diff");
    assert_eq!(result.content["baseline_used"], "worktree");
    let ranges = result.content["ranges"].as_array().expect("ranges");
    assert_eq!(ranges.len(), 1);
    assert!(
        ranges[0]["content"]
            .as_str()
            .expect("range content")
            .contains("changed() -> usize { 2 }")
    );
    assert!(
        !ranges[0]["content"]
            .as_str()
            .expect("range content")
            .contains("same()")
    );
    assert_uniform_evidence_packet(&result.content["packets"][0]);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_last_receipt_returns_stub_when_file_is_unchanged() {
    let root = temp_workspace("read_slice_last_receipt_unchanged");
    fs::write(root.join("sample.txt"), "alpha\nbeta\n").expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex("alpha\nbeta\n".as_bytes())),
            start_byte: 0,
            end_byte: 11,
            content: "alpha\nbeta\n".to_string(),
            model_output_bytes: 256,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "diff".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "read_mode": "diff",
                    "diff_baseline": "last_receipt",
                    "limit": 11
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["receipt_stub"], true);
    assert_eq!(result.content["same_as_call_id"], "prior_read");
    assert!(
        result.content["ranges"]
            .as_array()
            .expect("ranges")
            .is_empty()
    );
    assert_uniform_evidence_packet(&result.content["packets"][0]);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_last_receipt_returns_changed_window() {
    let root = temp_workspace("read_slice_last_receipt_changed");
    fs::write(root.join("sample.txt"), "alpha\nzeta\n").expect("write sample");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex("alpha\nbeta\n".as_bytes())),
            start_byte: 0,
            end_byte: 11,
            content: "alpha\nbeta\n".to_string(),
            model_output_bytes: 256,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "diff".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "read_mode": "diff",
                    "diff_baseline": "last_receipt",
                    "limit": 11
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["baseline_used"], "last_receipt");
    let ranges = result.content["ranges"].as_array().expect("ranges");
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0]["content"], "zeta\n");
    assert_uniform_evidence_packet(&result.content["packets"][0]);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_last_receipt_reports_absolute_line_numbers_for_non_zero_offset() {
    // Regression: window-local line numbers used to leak through, so any
    // non-zero offset reported `start_line`/`end_line` off by exactly the
    // count of newlines preceding the window. Stage a four-line file, snapshot
    // only lines 3-4, mutate them, and assert the reported line numbers point
    // at the file's lines 3-4 — not 1-2.
    let root = temp_workspace("read_slice_last_receipt_offset_lines");
    let original = "line1\nline2\nline3\nline4\n";
    let modified = "line1\nline2\nLINE3\nLINE4\n";
    let window_start: u64 = 12; // length of "line1\nline2\n"
    let window_end: u64 = 24; // end of file
    fs::write(root.join("sample.txt"), modified).expect("write modified");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "sample.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some(sha256_hex(original.as_bytes())),
            start_byte: window_start,
            end_byte: window_end,
            content: original[window_start as usize..window_end as usize].to_string(),
            model_output_bytes: 256,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "diff".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "sample.txt",
                    "read_mode": "diff",
                    "diff_baseline": "last_receipt",
                    "offset": window_start,
                    "limit": window_end - window_start,
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["baseline_used"], "last_receipt");
    let ranges = result.content["ranges"].as_array().expect("ranges");
    assert!(!ranges.is_empty(), "expected at least one changed range");
    let first = &ranges[0];
    assert_eq!(
        first["start_line"], 3,
        "start_line must be absolute (file line 3), not window-local: {first}"
    );
    assert!(
        first["end_line"].as_u64().expect("end_line") >= 3,
        "end_line must be absolute: {first}"
    );
    assert!(
        first["start_byte"].as_u64().expect("start_byte") >= window_start,
        "start_byte must be file-absolute: {first}"
    );
    assert_uniform_evidence_packet(&result.content["packets"][0]);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_last_receipt_falls_back_to_worktree_when_window_mismatches() {
    let root = temp_workspace("read_slice_last_receipt_window_mismatch");
    write_rust_crate(&root, "pub fn alpha() -> usize { 1 }\n");
    git_init_commit(&root);
    fs::write(root.join("src/lib.rs"), "pub fn alpha() -> usize { 2 }\n").expect("modify");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    // Snapshot covers a window that does not match the request below
    // (start_byte/end_byte differ) so last_receipt must fall back to
    // `worktree` and surface `last_receipt_window_mismatch`.
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "src/lib.rs".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "prior_read".to_string(),
            stable_output_sha256: "prior-output".to_string(),
            content_sha256: Some("some-other-hash".to_string()),
            start_byte: 5,
            end_byte: 10,
            content: "n alp".to_string(),
            model_output_bytes: 64,
            created_unix_millis: 1,
        })
        .expect("put snapshot");
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "diff".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "src/lib.rs",
                    "read_mode": "diff",
                    "diff_baseline": "last_receipt"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["baseline_requested"], "last_receipt");
    assert_eq!(result.content["baseline_used"], "worktree");
    assert_eq!(
        result.content["baseline_fallback"]["reason"],
        "last_receipt_window_mismatch"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_last_receipt_falls_back_to_worktree_when_snapshot_is_missing() {
    let root = temp_workspace("read_slice_last_receipt_snapshot_missing");
    write_rust_crate(&root, "pub fn alpha() -> usize { 1 }\n");
    git_init_commit(&root);
    fs::write(root.join("src/lib.rs"), "pub fn alpha() -> usize { 2 }\n").expect("modify");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "diff".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "src/lib.rs",
                    "read_mode": "diff",
                    "diff_baseline": "last_receipt"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(
        result.content["baseline_fallback"]["reason"],
        "last_receipt_snapshot_missing"
    );
    assert_eq!(result.content["baseline_used"], "worktree");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_slice_diff_last_receipt_keeps_distinct_windows_for_same_path() {
    // Two snapshots for the same path with non-overlapping windows must
    // coexist under the new `(path, start_byte, end_byte)` keying.
    // Asking for window B's bytes must hit window B's snapshot, not silently
    // fall back because window A's hash matches the current file.
    let root = temp_workspace("read_slice_last_receipt_two_windows");
    // 24 bytes, two halves of 12 bytes each:
    let original = "aaaaaaaaaaaa" // bytes 0..12
        .to_string()
        + "bbbbbbbbbbbb"; // bytes 12..24
    let modified = "aaaaaaaaaaaa".to_string() + "ZZZZbbbbbbbb"; // window B mutated
    fs::write(root.join("blob.txt"), modified.as_bytes()).expect("write blob");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("store"));
    let original_sha = sha256_hex(original.as_bytes());
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "blob.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "window_a".to_string(),
            stable_output_sha256: "window-a-out".to_string(),
            content_sha256: Some(original_sha.clone()),
            start_byte: 0,
            end_byte: 12,
            content: "aaaaaaaaaaaa".to_string(),
            model_output_bytes: 64,
            created_unix_millis: 1,
        })
        .expect("put window A snapshot");
    store
        .put_read_snapshot(&StoredReadSnapshot {
            path: "blob.txt".to_string(),
            tool_name: "read_file".to_string(),
            call_id: "window_b".to_string(),
            stable_output_sha256: "window-b-out".to_string(),
            content_sha256: Some(original_sha),
            start_byte: 12,
            end_byte: 24,
            content: "bbbbbbbbbbbb".to_string(),
            model_output_bytes: 64,
            created_unix_millis: 2,
        })
        .expect("put window B snapshot");
    let registry = registry_with_state_store(&root, store);

    let window_b = registry
        .execute(
            ToolCall {
                call_id: "diff_b".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({
                    "path": "blob.txt",
                    "read_mode": "diff",
                    "diff_baseline": "last_receipt",
                    "offset": 12,
                    "limit": 12
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(window_b.status, ToolStatus::Success);
    assert_eq!(window_b.content["baseline_used"], "last_receipt");
    let ranges = window_b.content["ranges"].as_array().expect("ranges");
    assert!(
        !ranges.is_empty(),
        "expected window B to surface modified bytes: {}",
        window_b.content
    );
    assert!(
        ranges[0]["content"]
            .as_str()
            .expect("content")
            .contains("ZZZZ"),
        "unexpected ranges payload: {}",
        window_b.content
    );

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
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/example\"]\n",
    )
    .expect("write workspace manifest");
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

    assert_eq!(
        command,
        "cargo test -p squeezy-example --message-format=json"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn diff_verify_command_uses_nested_manifest_when_root_has_no_cargo() {
    let root = temp_workspace("verify_nested_manifest");
    fs::create_dir_all(root.join("tools/sonar-arch-graph/src")).expect("create crate");
    fs::write(
        root.join("tools/sonar-arch-graph/Cargo.toml"),
        "[package]\nname = \"sonar-arch-graph\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");

    let plan = verify_command_plan(
        &root,
        VerifyScope::Diff,
        VerifyLevel::Quick,
        &["tools/sonar-arch-graph/src/main.rs".to_string()],
    )
    .expect("verification plan");

    assert_eq!(
        plan.command,
        "cargo test --manifest-path 'tools/sonar-arch-graph/Cargo.toml' --message-format=json"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn verify_reports_not_run_when_no_cargo_manifest_exists() {
    let root = temp_workspace("verify_no_manifest");
    fs::create_dir_all(root.join("src")).expect("create src");
    fs::write(root.join("src/lib.rs"), "pub fn changed() {}\n").expect("write rust");
    git_init_commit(&root);
    fs::write(
        root.join("src/lib.rs"),
        "pub fn changed() -> bool { true }\n",
    )
    .expect("modify rust");
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
    assert_eq!(result.content["no_op"], true);
    assert_eq!(result.content["not_run"], true);
    assert!(
        result.content["reason"]
            .as_str()
            .expect("reason")
            .contains("no Cargo.toml"),
        "{}",
        result.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_patch_reports_graph_impact_and_locality_warning() {
    let root = temp_workspace("plan_patch");
    write_rust_crate(
        &root,
        "pub fn changed() -> usize { 1 }\nfn caller() -> usize { changed() }\n",
    );
    fs::create_dir_all(root.join(".github")).expect("mkdir github");
    fs::write(root.join(".github/CODEOWNERS"), "* @owner\n").expect("write codeowners");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "plan".to_string(),
                name: "plan_patch".to_string(),
                arguments: json!({
                    "objective": "change changed return value",
                    "query": "changed",
                    "kind": "function",
                    "candidate_paths": ["README.md"],
                    "max_symbols": 4,
                    "max_related": 4
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["patch_format"], "search_replace");
    assert!(
        result.content["impact"]["neighborhood_paths"]
            .as_array()
            .expect("neighborhood")
            .iter()
            .any(|path| path == "src/lib.rs")
    );
    assert_eq!(result.content["locality"]["status"], "outside");
    assert!(
        result.content["impact"]["owners"]
            .as_array()
            .expect("owners")
            .iter()
            .any(|owner| owner["owners"][0] == "@owner")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_edits_file_and_checkpoint_undo_restores_it() {
    let root = temp_workspace("apply_patch_undo");
    fs::write(root.join("sample.txt"), "before\n").expect("write sample");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "plan_id": "patch-test",
                    "impact_paths": ["sample.txt"],
                    "patches": [{
                        "path": "sample.txt",
                        "search": "before\n",
                        "replace": "after\n",
                        "expected_sha256": sha256_hex("before\n".as_bytes())
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-patch".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["checkpoint"]["group_id"], "turn-patch");
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "after\n"
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
        "before\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_dry_run_previews_without_writing() {
    let root = temp_workspace("apply_patch_dry_run");
    fs::write(root.join("sample.txt"), "before\n").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "patch".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "dry_run": true,
                    "patches": [{
                        "path": "sample.txt",
                        "search": "before\n",
                        "replace": "after\n",
                        "expected_sha256": sha256_hex("before\n".as_bytes())
                    }]
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["dry_run"], true);
    assert!(result.content.get("checkpoint").is_none());
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "before\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_rejects_stale_hash_without_modifying_file() {
    let root = temp_workspace("apply_patch_stale_hash");
    fs::write(root.join("sample.txt"), "before\n").expect("write sample");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "patch".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": "sample.txt",
                        "search": "before\n",
                        "replace": "after\n",
                        "expected_sha256": sha256_hex("other\n".as_bytes())
                    }]
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Stale);
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "before\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_rejects_multiple_matches_unless_allowed() {
    let root = temp_workspace("apply_patch_multiple");
    fs::write(root.join("sample.txt"), "same same\n").expect("write sample");
    let registry = registry_with_checkpoints(&root);

    let rejected = registry
        .execute(
            ToolCall {
                call_id: "patch1".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": "sample.txt",
                        "search": "same",
                        "replace": "next",
                        "expected_sha256": sha256_hex("same same\n".as_bytes())
                    }]
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(rejected.status, ToolStatus::Stale);
    assert_eq!(rejected.content["matches"], 2);
    assert_eq!(rejected.content["match_contexts"][0]["line"], 1);
    assert!(
        rejected.content["match_contexts"][0]["preview"]
            .as_str()
            .is_some_and(|preview| preview.contains("same same")),
        "{}",
        rejected.content
    );
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "same same\n"
    );

    let accepted = registry
        .execute(
            ToolCall {
                call_id: "patch2".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": "sample.txt",
                        "search": "same",
                        "replace": "next",
                        "expected_sha256": sha256_hex("same same\n".as_bytes()),
                        "allow_multiple": true
                    }]
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(accepted.status, ToolStatus::Success);
    assert!(
        accepted.content["checkpoint"]["files"][0]["patch"]
            .as_str()
            .is_some_and(|patch| patch.contains("-same same") && patch.contains("+next next")),
        "{}",
        accepted.content
    );
    assert_eq!(
        fs::read_to_string(root.join("sample.txt")).unwrap(),
        "next next\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_warns_for_paths_outside_impact_neighborhood() {
    let root = temp_workspace("apply_patch_locality");
    fs::write(root.join("inside.txt"), "inside\n").expect("write inside");
    fs::write(root.join("outside.txt"), "outside\n").expect("write outside");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "patch".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "impact_paths": ["inside.txt"],
                    "patches": [{
                        "path": "outside.txt",
                        "search": "outside\n",
                        "replace": "changed\n",
                        "expected_sha256": sha256_hex("outside\n".as_bytes())
                    }]
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["locality"]["status"], "outside");
    assert!(
        result.content["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning.as_str().unwrap_or("").contains("outside.txt"))
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_partial_failure_records_checkpoint_for_undo() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_workspace("apply_patch_partial_failure");
    fs::write(root.join("first.txt"), "first-before\n").expect("write first");
    fs::write(root.join("second.txt"), "second-before\n").expect("write second");
    let read_only = root.join("second.txt");
    let mut perms = fs::metadata(&read_only).expect("read meta").permissions();
    perms.set_mode(0o444);
    fs::set_permissions(&read_only, perms).expect("set readonly");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_partial".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [
                        {
                            "path": "first.txt",
                            "search": "first-before\n",
                            "replace": "first-after\n",
                            "expected_sha256": sha256_hex("first-before\n".as_bytes())
                        },
                        {
                            "path": "second.txt",
                            "search": "second-before\n",
                            "replace": "second-after\n",
                            "expected_sha256": sha256_hex("second-before\n".as_bytes())
                        }
                    ]
                }),
            },
            CancellationToken::new(),
            "turn-partial".to_string(),
        )
        .await;

    // Restore writable perms so cleanup works regardless of how the platform
    // reacts to the read-only target.
    if let Ok(meta) = fs::metadata(&read_only) {
        let mut perms = meta.permissions();
        perms.set_mode(0o644);
        let _ = fs::set_permissions(&read_only, perms);
    }

    if result.status == ToolStatus::Error {
        assert!(
            result.content.get("checkpoint").is_some(),
            "expected partial-failure result to carry a checkpoint, got: {}",
            result.content
        );
        let applied_delta = result
            .content
            .get("applied_delta")
            .cloned()
            .unwrap_or(Value::Null);
        let ops = applied_delta
            .get("operations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            ops.len(),
            2,
            "applied_delta must contain one entry per requested op, got: {applied_delta}"
        );
        assert_eq!(
            ops[0].get("status").and_then(|v| v.as_str()),
            Some("applied"),
            "first op should be applied, got: {}",
            ops[0]
        );
        assert_eq!(
            ops[1].get("status").and_then(|v| v.as_str()),
            Some("failed"),
            "second op should be failed, got: {}",
            ops[1]
        );
        assert_eq!(
            applied_delta.get("exact").and_then(|v| v.as_bool()),
            Some(false),
            "applied_delta.exact must be false when any op failed"
        );
        assert_eq!(
            fs::read_to_string(root.join("first.txt")).unwrap(),
            "first-after\n",
            "first file should have been written before the second failed"
        );
        let undo = registry
            .execute(
                ToolCall {
                    call_id: "undo_partial".to_string(),
                    name: "checkpoint_undo".to_string(),
                    arguments: json!({}),
                },
                CancellationToken::new(),
            )
            .await;
        assert_eq!(undo.status, ToolStatus::Success);
        assert_eq!(
            fs::read_to_string(root.join("first.txt")).unwrap(),
            "first-before\n",
            "checkpoint_undo should restore the partial mutation"
        );
        assert_eq!(
            fs::read_to_string(root.join("second.txt")).unwrap(),
            "second-before\n",
            "second file should be unchanged after partial failure"
        );
    } else {
        // Some sandboxes (e.g. CI running as root) ignore 0o444, in which case
        // both writes succeed and the assertion above does not apply.
        assert_eq!(result.status, ToolStatus::Success);
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_denies_secret_paths() {
    let root = temp_workspace("apply_patch_secret");
    fs::write(root.join(".env"), "KEY=val\n").expect("write env");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "patch_secret".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": ".env",
                        "search": "KEY=val\n",
                        "replace": "KEY=new\n",
                        "expected_sha256": sha256_hex("KEY=val\n".as_bytes())
                    }]
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Denied);
    assert_eq!(result.content["path"], ".env");
    assert_eq!(result.content["permission_denied"], true);
    assert_eq!(
        fs::read_to_string(root.join(".env")).unwrap(),
        "KEY=val\n",
        ".env must not be modified by a denied patch"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_denies_protected_metadata_paths_before_mutation() {
    let root = temp_workspace("apply_patch_metadata");
    fs::create_dir_all(root.join(".git")).expect("mkdir git");
    fs::write(root.join(".git/config"), "before\n").expect("write git config");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "patch_metadata".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "patches": [{
                        "path": ".git/config",
                        "search": "before\n",
                        "replace": "after\n",
                        "expected_sha256": sha256_hex("before\n".as_bytes())
                    }]
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Denied);
    assert_eq!(result.content["reason"], "protected_metadata_path");
    assert_eq!(result.content["permission_denied"], true);
    assert_eq!(
        fs::read_to_string(root.join(".git/config")).unwrap(),
        "before\n",
        "protected metadata must not be modified by a denied patch",
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_move_collapses_to_single_checkpoint_entry() {
    let root = temp_workspace("apply_patch_move");
    fs::write(root.join("alpha.txt"), "alpha\n").expect("seed alpha");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_move".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "move_file",
                        "from": "alpha.txt",
                        "to": "beta.txt",
                        "expected_sha256": sha256_hex("alpha\n".as_bytes()),
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-move".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert!(!root.join("alpha.txt").exists(), "source should be removed");
    assert_eq!(
        fs::read_to_string(root.join("beta.txt")).unwrap(),
        "alpha\n",
        "destination should hold the source content"
    );
    let checkpoint = result
        .content
        .get("checkpoint")
        .expect("checkpoint emitted");
    let files = checkpoint
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        files.len(),
        1,
        "rename should collapse to a single checkpoint entry, got {files:?}"
    );
    assert_eq!(files[0]["status"], "renamed");
    assert_eq!(files[0]["path"], "beta.txt");
    assert_eq!(files[0]["from_path"], "alpha.txt");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_create_and_delete_in_one_call() {
    let root = temp_workspace("apply_patch_create_delete");
    fs::write(root.join("doomed.txt"), "bye\n").expect("seed doomed");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_create_delete".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [
                        {
                            "kind": "create_file",
                            "path": "fresh.txt",
                            "contents": "hello\n",
                        },
                        {
                            "kind": "delete_file",
                            "path": "doomed.txt",
                            "expected_sha256": sha256_hex("bye\n".as_bytes()),
                        }
                    ]
                }),
            },
            CancellationToken::new(),
            "turn-create-delete".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert_eq!(
        fs::read_to_string(root.join("fresh.txt")).unwrap(),
        "hello\n"
    );
    assert!(!root.join("doomed.txt").exists());
    let delta = result
        .content
        .get("applied_delta")
        .cloned()
        .unwrap_or(Value::Null);
    let ops = delta
        .get("operations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["status"], "applied");
    assert_eq!(ops[0]["kind"], "create_file");
    assert_eq!(ops[1]["status"], "applied");
    assert_eq!(ops[1]["kind"], "delete_file");
    assert_eq!(delta["exact"], true);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_create_file_rejects_existing_target() {
    let root = temp_workspace("apply_patch_create_existing");
    fs::write(root.join("there.txt"), "stay\n").expect("seed there");
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_create_existing".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "create_file",
                        "path": "there.txt",
                        "contents": "stomp\n",
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-create-existing".to_string(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Stale);
    assert_eq!(
        fs::read_to_string(root.join("there.txt")).unwrap(),
        "stay\n",
        "existing file must not be clobbered by create_file"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn write_file_denies_protected_metadata_paths_before_mutation() {
    let root = temp_workspace("write_metadata");
    fs::create_dir_all(root.join(".squeezy")).expect("mkdir metadata");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "write_metadata".to_string(),
                name: "write_file".to_string(),
                arguments: json!({
                    "path": ".squeezy/state.toml",
                    "content": "after\n",
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Denied);
    assert_eq!(result.content["reason"], "protected_metadata_path");
    assert_eq!(result.content["permission_denied"], true);
    assert!(!root.join(".squeezy/state.toml").exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn apply_patch_unified_diff_fallback_applies_via_git_apply_3way() {
    // The fallback's job is to honour a unified-diff body the model places in
    // `search` (e.g. when its literal search string drifted or it knows the
    // change as a hunk, not a contiguous substring). On a clean worktree
    // `git apply --3way` lands the diff and the resulting file lives at the
    // diff's `+` lines.
    let root = temp_workspace("apply_patch_unified_diff");
    let initial = "line one\nline two\nline three\n";
    fs::write(root.join("doc.txt"), initial).expect("seed doc");
    git_init_commit(&root);

    let diff = "--- a/doc.txt\n+++ b/doc.txt\n@@ -1,3 +1,3 @@\n line one\n-line two\n+LINE TWO\n line three\n";
    let on_disk_hash = sha256_hex(initial.as_bytes());
    let registry = registry_with_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "patch_fallback".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "operations": [{
                        "kind": "search_replace",
                        "path": "doc.txt",
                        // `search` here is the unified diff body — the literal
                        // string won't be found in the file, so the fallback
                        // path is the only way this could succeed.
                        "search": diff,
                        "replace": "",
                        "expected_sha256": on_disk_hash,
                        "fallback": "unified_diff",
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-fallback".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    let final_doc = fs::read_to_string(root.join("doc.txt")).expect("read doc");
    assert!(
        final_doc.contains("LINE TWO"),
        "fallback should replace via git apply, got: {final_doc:?}"
    );
    let checkpoint = result
        .content
        .get("checkpoint")
        .expect("fallback should emit checkpoint");
    let files = checkpoint
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        files.iter().any(|f| f["path"] == "doc.txt"),
        "checkpoint should record the touched file"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_patch_binding_succeeds_inside_neighborhood() {
    let root = temp_workspace("plan_binding_inside");
    write_rust_crate(
        &root,
        "pub fn target() -> usize { 1 }\nfn caller() -> usize { target() }\n",
    );
    let registry = registry_with_checkpoints(&root);

    let plan = registry
        .execute(
            ToolCall {
                call_id: "plan".to_string(),
                name: "plan_patch".to_string(),
                arguments: json!({
                    "objective": "tweak target",
                    "query": "target",
                    "kind": "function",
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(plan.status, ToolStatus::Success, "{:?}", plan.content);
    let plan_id = plan.content["plan_id"]
        .as_str()
        .expect("plan_id returned")
        .to_string();

    let actual_hash = sha256_hex(
        fs::read(root.join("src/lib.rs"))
            .expect("read src/lib.rs")
            .as_slice(),
    );
    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "apply_inside".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "plan_id": plan_id,
                    "patches": [{
                        "path": "src/lib.rs",
                        "search": "pub fn target() -> usize { 1 }\n",
                        "replace": "pub fn target() -> usize { 2 }\n",
                        "expected_sha256": actual_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-plan-inside".to_string(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert!(
        fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("pub fn target() -> usize { 2 }")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_patch_binding_rejects_outside_neighborhood() {
    let root = temp_workspace("plan_binding_outside");
    write_rust_crate(
        &root,
        "pub fn target() -> usize { 1 }\nfn caller() -> usize { target() }\n",
    );
    fs::write(root.join("stranger.txt"), "out\n").expect("write stranger");
    let registry = registry_with_checkpoints(&root);

    let plan = registry
        .execute(
            ToolCall {
                call_id: "plan_out".to_string(),
                name: "plan_patch".to_string(),
                arguments: json!({
                    "objective": "tweak target",
                    "query": "target",
                    "kind": "function",
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(plan.status, ToolStatus::Success);
    let plan_id = plan.content["plan_id"]
        .as_str()
        .expect("plan_id")
        .to_string();
    if plan
        .content
        .get("graph_available")
        .and_then(|v| v.as_bool())
        == Some(false)
    {
        // Without the semantic graph the neighborhood is empty and the plan
        // would not bind any path, so the binding check is a no-op. Skip.
        let _ = fs::remove_dir_all(root);
        return;
    }

    let actual_hash = sha256_hex(
        fs::read(root.join("stranger.txt"))
            .expect("read stranger")
            .as_slice(),
    );
    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "apply_outside".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "plan_id": plan_id,
                    "patches": [{
                        "path": "stranger.txt",
                        "search": "out\n",
                        "replace": "in\n",
                        "expected_sha256": actual_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-plan-outside".to_string(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Stale, "{:?}", result.content);
    let err = result.content["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("plan_id"),
        "rejection should mention plan_id, got: {err}"
    );
    assert_eq!(
        fs::read_to_string(root.join("stranger.txt")).unwrap(),
        "out\n",
        "out-of-neighborhood file must not be written"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn plan_patch_binding_can_be_bypassed_with_confirm_outside_plan() {
    let root = temp_workspace("plan_binding_confirm");
    write_rust_crate(
        &root,
        "pub fn target() -> usize { 1 }\nfn caller() -> usize { target() }\n",
    );
    fs::write(root.join("stranger.txt"), "out\n").expect("write stranger");
    let registry = registry_with_checkpoints(&root);

    let plan = registry
        .execute(
            ToolCall {
                call_id: "plan_confirm".to_string(),
                name: "plan_patch".to_string(),
                arguments: json!({
                    "objective": "tweak target",
                    "query": "target",
                    "kind": "function",
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(plan.status, ToolStatus::Success);
    let plan_id = plan.content["plan_id"]
        .as_str()
        .expect("plan_id")
        .to_string();

    let actual_hash = sha256_hex(
        fs::read(root.join("stranger.txt"))
            .expect("read stranger")
            .as_slice(),
    );
    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "apply_confirm".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({
                    "plan_id": plan_id,
                    "confirm_outside_plan": true,
                    "patches": [{
                        "path": "stranger.txt",
                        "search": "out\n",
                        "replace": "in\n",
                        "expected_sha256": actual_hash,
                    }]
                }),
            },
            CancellationToken::new(),
            "turn-plan-confirm".to_string(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert_eq!(
        fs::read_to_string(root.join("stranger.txt")).unwrap(),
        "in\n"
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
    let registry = registry_with_shell_sandbox_off(&root);

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
    let registry = registry_with_checkpoints(&root);

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
async fn checkpointing_is_disabled_by_default_for_mutations() {
    let root = temp_workspace("checkpoint_disabled_default");
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
            "turn-disabled".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert!(result.content.get("checkpoint").is_none());
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
    assert_eq!(undo.status, ToolStatus::Stale);
    assert_eq!(undo.content["enabled"], false);

    let list = registry
        .execute(
            ToolCall {
                call_id: "list".to_string(),
                name: "checkpoint_list".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(list.status, ToolStatus::Success);
    assert_eq!(list.content["enabled"], false);
    assert_eq!(list.content["checkpoints"].as_array().unwrap().len(), 0);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_created_file_is_checkpointed_and_deleted_on_undo() {
    let root = temp_workspace("checkpoint_shell_undo");
    // Disable the OS sandbox so this test focuses on checkpoint behavior;
    // OS sandbox backend coverage lives in shell_sandbox_tests.
    let registry = registry_with_shell_sandbox_off_and_checkpoints(&root);

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
async fn direct_user_shell_skips_checkpoint_and_sandbox() {
    let root = temp_workspace("direct_user_shell_fast_path");
    let shell_sandbox = squeezy_core::ShellSandboxConfig {
        mode: squeezy_core::ShellSandboxMode::Required,
        ..squeezy_core::ShellSandboxConfig::default()
    };
    let registry = ToolRegistry::new_inner(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        shell_sandbox,
        SkillCatalog::empty(),
        CrawlOptions::default(),
        ToolRegistryRuntime::default(),
    )
    .expect("registry");

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "local-shell-test".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf direct > direct.txt",
                    "description": "run an explicit user shell command",
                    "direct_user_shell": true
                }),
            },
            CancellationToken::new(),
            "turn-direct".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert_eq!(result.content["sandbox"]["backend"], "none");
    assert_eq!(result.content["sandbox"]["mode"], "off");
    assert_eq!(result.content["policy"]["direct_user_shell"], true);
    assert!(result.content.get("checkpoint").is_none());
    assert_eq!(
        fs::read_to_string(root.join("direct.txt")).unwrap(),
        "direct"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_only_model_shell_skips_checkpoint() {
    let root = temp_workspace("read_only_shell_fast_path");
    fs::write(root.join("sample.txt"), "sample").expect("write sample");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "readonly".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "ls -la",
                    "description": "list files"
                }),
            },
            CancellationToken::new(),
            "turn-readonly".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert_eq!(result.content["policy"]["capability"], "search");
    assert!(result.content.get("checkpoint").is_none());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_only_git_shell_skips_checkpoint() {
    let root = temp_workspace("read_only_git_shell_fast_path");
    fs::write(root.join("sample.txt"), "sample").expect("write sample");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "git-status".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "git status --short",
                    "description": "inspect git status"
                }),
            },
            CancellationToken::new(),
            "turn-git-status".to_string(),
        )
        .await;

    assert_eq!(result.content["policy"]["capability"], "git");
    assert!(result.content.get("checkpoint").is_none());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn model_shell_cannot_request_direct_user_shell_fast_path() {
    let root = temp_workspace("direct_user_shell_model_guard");
    let registry = registry_with_shell_sandbox_off_and_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "model-call".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf guarded > guarded.txt",
                    "description": "attempt to set hidden direct shell flag",
                    "direct_user_shell": true
                }),
            },
            CancellationToken::new(),
            "turn-model".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert_eq!(result.content["policy"]["direct_user_shell"], false);
    assert_eq!(result.content["checkpoint"]["group_id"], "turn-model");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_checkpoint_ignores_gitignored_target_outputs() {
    let root = temp_workspace("checkpoint_shell_ignored_target");
    fs::write(root.join(".gitignore"), "target\n").expect("write gitignore");
    fs::create_dir(root.join("target")).expect("create target");
    let large = fs::File::create(root.join("target").join("debug.bin")).expect("create large");
    large
        .set_len(3 * 1024 * 1024)
        .expect("write large placeholder");
    let registry = registry_with_shell_sandbox_off_and_checkpoints(&root);

    let result = registry
        .execute_for_group(
            ToolCall {
                call_id: "shell-build".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf ok > built.txt",
                    "description": "simulate a build artifact beside ignored target output"
                }),
            },
            CancellationToken::new(),
            "turn-build".to_string(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert_eq!(result.content["checkpoint"]["group_id"], "turn-build");
    assert!(root.join("built.txt").exists());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn checkpoint_undo_reports_conflict_and_preserves_dirty_user_change() {
    let root = temp_workspace("checkpoint_conflict");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let registry = registry_with_checkpoints(&root);

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
    let registry = registry_with_shell_sandbox_off_and_checkpoints(&root);

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
    let registry = registry_with_checkpoints(&root);

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
    let registry = registry_with_checkpoints(&root);

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
    let registry = registry_with_shell_sandbox_off_and_checkpoints(&root);

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
    let registry = registry_with_shell_sandbox_off_and_checkpoints(&root);

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
    let registry = registry_with_shell_sandbox_off(&root);

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
    assert_eq!(result.content["env"]["policy"], "allowlist");
    assert_eq!(result.content["env"]["values"], "redacted");
    assert_eq!(result.content["sandbox"]["mode"], "off");
    assert!(result.content["policy"]["parser_backed"].as_bool().unwrap());
    let audit = fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log");
    assert!(audit.contains("\"call_id\":\"call_1\""));
    assert!(audit.contains("\"stdout_sha256\""));
    assert!(!audit.contains("\"stdout\":\"abc\""));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_default_sandbox_runs_benign_command() {
    let root = temp_workspace("shell_default_sandbox");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_default_shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf ok",
                    "description": "check default shell sandbox posture"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["stdout"], "ok");
    assert_eq!(result.content["sandbox"]["mode"], "best_effort");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_external_sandbox_mode_preserves_policy_metadata() {
    let root = temp_workspace("shell_external_sandbox");
    let registry = registry_with_runtime_config(
        &root,
        ToolRuntimeConfig {
            shell_sandbox: squeezy_core::ShellSandboxConfig {
                mode: squeezy_core::ShellSandboxMode::External,
                ..squeezy_core::ShellSandboxConfig::default()
            },
            ..ToolRuntimeConfig::default()
        },
    );

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_external_shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf ok",
                    "description": "outer sandbox handles isolation"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["stdout"], "ok");
    assert_eq!(result.content["sandbox"]["backend"], "external");
    assert_eq!(result.content["sandbox"]["mode"], "external");
    assert_eq!(result.content["sandbox"]["network"], "external");
    assert_eq!(result.content["env"]["policy"], "allowlist");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_denies_protected_metadata_write_before_spawn() {
    let root = temp_workspace("shell_metadata_write");
    fs::create_dir_all(root.join(".git")).expect("mkdir git");
    fs::write(root.join(".git/config"), "secret-ish").expect("write git config");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_metadata_shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "touch .git/config",
                    "description": "try metadata write"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Denied);
    assert_eq!(result.content["permission_denied"], true);
    assert!(
        result.content["error"]
            .as_str()
            .is_some_and(|reason| reason.contains("protected metadata directory")),
        "{:?}",
        result.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_workdir_accepts_configured_extra_root() {
    let root = temp_workspace("shell_extra_workdir");
    let extra = temp_workspace("shell_extra_root");
    let extra = fs::canonicalize(&extra).expect("canonical extra root");
    let shell_sandbox = squeezy_core::ShellSandboxConfig {
        mode: squeezy_core::ShellSandboxMode::Off,
        write_roots: vec![extra.clone()],
        ..squeezy_core::ShellSandboxConfig::default()
    };
    let registry = ToolRegistry::new_inner(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        shell_sandbox,
        SkillCatalog::empty(),
        CrawlOptions::default(),
        ToolRegistryRuntime::default(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_extra_workdir".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf ok",
                    "workdir": extra.display().to_string(),
                    "description": "run in configured extra root"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["stdout"], "ok");
    assert_eq!(
        result.content["sandbox"]["write_roots"][0],
        extra.display().to_string()
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(extra);
}

#[tokio::test]
async fn shell_workdir_rejects_unconfigured_outside_root() {
    let root = temp_workspace("shell_outside_workdir");
    let outside = temp_workspace("shell_outside_root");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_outside_workdir".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf no",
                    "workdir": outside.display().to_string(),
                    "description": "run outside workspace"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Denied);
    assert!(
        result.content["error"]
            .as_str()
            .unwrap()
            .contains("configured shell sandbox roots")
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

#[tokio::test]
async fn shell_sensitive_path_reference_is_denied_before_spawn() {
    let root = temp_workspace("shell_sensitive");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_sensitive".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "cat .env",
                    "description": "read env"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Denied);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("sensitive path")
    );
    let audit = fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log");
    assert!(audit.contains("\"outcome\":\"denied\""));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_rejects_empty_command_with_structured_policy_reason() {
    let root = temp_workspace("shell_empty");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "   ",
                    "description": "empty"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Denied);
    assert_eq!(result.content["permission_denied"], true);
    assert_eq!(result.content["policy_denied"], true);
    assert_eq!(result.content["capability"], "shell");
    assert_eq!(result.content["target"], "shell:*");
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("must not be empty")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_rejects_workdir_outside_workspace_with_structured_policy_reason() {
    let root = temp_workspace("shell_workdir_policy");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "pwd",
                    "workdir": "..",
                    "description": "outside"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Denied);
    assert_eq!(result.content["permission_denied"], true);
    assert_eq!(result.content["policy_denied"], true);
    assert_eq!(result.content["capability"], "search");
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("workdir rejected")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_timeout_returns_structured_error_and_kills_process() {
    let root = temp_workspace("shell_timeout");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "sleep 2",
                    "timeout_ms": 25,
                    "description": "exercise timeout"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(result.content["exit_code"], Value::Null);
    assert!(
        result.content["error"]
            .as_str()
            .expect("error")
            .contains("timed out")
    );
    assert_eq!(result.content["truncated"], true);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_output_cap_is_enforced_while_command_runs() {
    let root = temp_workspace("shell_cap");
    let registry = registry_with_shell_sandbox_off(&root);

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

#[tokio::test]
async fn shell_non_tty_closes_stdin_by_default() {
    let root = temp_workspace("shell_stdin_null");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_stdin".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "if read line; then printf got; else printf eof; fi",
                    "output_mode": "raw",
                    "description": "check stdin policy"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["stdout"], "eof");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_tty_attaches_stdout_to_terminal() {
    let root = temp_workspace("shell_tty");
    let registry = registry_with_shell_sandbox_off(&root);

    let pipe = registry
        .execute(
            ToolCall {
                call_id: "call_pipe".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "if test -t 1; then printf tty; else printf pipe; fi",
                    "output_mode": "raw",
                    "description": "check pipe mode"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(pipe.status, ToolStatus::Success);
    assert_eq!(pipe.content["stdout"], "pipe");

    let tty = registry
        .execute(
            ToolCall {
                call_id: "call_tty".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "if test -t 1; then printf tty; else printf pipe; fi",
                    "tty": true,
                    "output_mode": "raw",
                    "description": "check tty mode"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(tty.status, ToolStatus::Success);
    assert_eq!(tty.content["stdout"], "tty");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_drain_timeout_returns_partial_output_from_open_grandchild_pipe() {
    let root = temp_workspace("shell_drain_timeout");
    let registry = registry_with_shell_sandbox_off(&root);
    let started = std::time::Instant::now();

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_drain".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf head; sleep 5 &",
                    "timeout_ms": 10_000,
                    "output_mode": "raw",
                    "description": "check output drain timeout"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "open inherited pipe must be bounded by drain timeout"
    );
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["stdout"], "head");
    assert_eq!(result.content["truncated"], true);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_stream_budget_preserves_stdout_and_stderr() {
    let root = temp_workspace("shell_stream_budget");
    let registry = registry_with_shell_sandbox_off(&root);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_split".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "i=0; while [ $i -lt 300 ]; do printf 'stdout line %04d\\n' \"$i\"; printf 'stderr line %04d\\n' \"$i\" >&2; i=$((i+1)); done",
                    "output_byte_cap": 4096,
                    "output_mode": "raw",
                    "description": "check split stream budget"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let stdout = result.content["stdout"].as_str().expect("stdout");
    let stderr = result.content["stderr"].as_str().expect("stderr");
    assert!(stdout.len() >= 1000, "stdout too small: {}", stdout.len());
    assert!(stderr.len() >= 1000, "stderr too small: {}", stderr.len());
    assert!(stdout.len() + stderr.len() <= 4096);
    assert_eq!(result.content["truncated"], true);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_exposes_in_flight_ask_socket_when_approver_is_present() {
    let root = temp_workspace("shell_ask_socket");
    let registry = registry_with_shell_sandbox_off(&root);
    let approver: ShellAskApprover = Arc::new(|_| Box::pin(async { ShellAskDecision::allow() }));

    let result = registry
        .execute_for_group_with_options(
            ToolCall {
                call_id: "call_ask_socket".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf '%s' \"$SQUEEZY_ASK_SOCKET\"",
                    "output_mode": "raw",
                    "description": "check ask socket env"
                }),
            },
            CancellationToken::new(),
            "test".to_string(),
            ToolExecutionOptions {
                shell_ask_approver: Some(approver),
            },
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let socket = result.content["stdout"].as_str().expect("socket path");
    if socket.is_empty() {
        let _ = fs::remove_dir_all(root);
        return;
    }
    assert!(socket.ends_with(".sock"), "{socket}");
    assert!(
        !Path::new(socket).exists(),
        "ask socket should be removed after shell completion"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn parallel_shell_calls_serialize_per_workdir() {
    let root = temp_workspace("shell_parallel_isolation");
    let registry = Arc::new(registry_with_shell_sandbox_off(&root));
    let command = "printf s >> order.txt; sleep 1; printf e >> order.txt";
    let call = |id: &str| ToolCall {
        call_id: id.to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": command,
            "output_mode": "raw",
            "description": "check per-workdir shell serialization"
        }),
    };

    let left_registry = registry.clone();
    let right_registry = registry.clone();
    let (left, right) = tokio::join!(
        left_registry.execute(call("left"), CancellationToken::new()),
        right_registry.execute(call("right"), CancellationToken::new()),
    );

    assert_eq!(left.status, ToolStatus::Success);
    assert_eq!(right.status, ToolStatus::Success);
    assert_eq!(fs::read_to_string(root.join("order.txt")).unwrap(), "sese");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_shaped_output_drops_noise_but_raw_mode_keeps_it() {
    let root = temp_workspace("shell_shaped_raw");
    let registry = registry_with_shell_sandbox_off(&root);

    let shaped = registry
        .execute(
            ToolCall {
                call_id: "call_shaped".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf 'Compiling crate_a\\nerror: bad\\n'",
                    "description": "shape noisy output"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(shaped.status, ToolStatus::Success);
    assert_eq!(shaped.content["output_shape"]["mode"], "shaped");
    assert!(
        shaped.content["stdout"]
            .as_str()
            .expect("stdout")
            .contains("error: bad")
    );
    assert!(
        !shaped.content["stdout"]
            .as_str()
            .expect("stdout")
            .contains("Compiling crate_a")
    );

    let raw = registry
        .execute(
            ToolCall {
                call_id: "call_raw".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf 'Compiling crate_a\\nerror: bad\\n'",
                    "description": "raw noisy output",
                    "output_mode": "raw"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(raw.status, ToolStatus::Success);
    assert!(raw.content.get("output_shape").is_none());
    assert!(
        raw.content["stdout"]
            .as_str()
            .expect("stdout")
            .contains("Compiling crate_a")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shaped_shell_spill_handle_reads_raw_unshaped_output() {
    let root = temp_workspace("shell_shaped_spill_raw");
    let registry = registry_with_shell_sandbox_off_and_output_config(
        &root,
        ToolOutputConfig {
            spill_threshold_bytes: 100,
            // Preview cap large enough to fit the full shaped-shell JSON
            // wrapper so this test exercises the spill+rehydrate roundtrip
            // rather than preview-truncation behavior (covered separately in
            // truncate tests).
            preview_bytes: 2_048,
            retention_days: 1,
            output_dir: None,
        },
    );

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_spill".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf 'Compiling crate_a\\nCompiling crate_b\\nerror: bad\\n'",
                    "description": "spill shaped output"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["spilled"], true);
    let preview = result.content["preview"].as_str().expect("preview");
    assert!(preview.contains("output_shape"));
    let handle = result.content["handle"].as_str().expect("handle");
    let fetched = registry
        .execute(
            ToolCall {
                call_id: "call_read_spill".to_string(),
                name: "read_tool_output".to_string(),
                arguments: json!({"handle": handle, "offset": 0, "limit": 8_000}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(fetched.status, ToolStatus::Success);
    let raw = fetched.content["content"].as_str().expect("content");
    assert!(raw.contains("Compiling crate_a"));
    assert!(!raw.contains("output_shape"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn shell_call_description_summary_carries_only_description() {
    // The shell `describe_call` summary intentionally surfaces ONLY the
    // model-facing description; the command, cwd, env policy, and other
    // structured fields are emitted via `permission_request().metadata`
    // and rendered by the TUI in the dedicated approval panel. This
    // prevents the same value from appearing twice in the approval UI.
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
    assert!(
        !description.contains("rm -rf target"),
        "summary must not duplicate the command",
    );
    assert!(
        !description.contains("env="),
        "summary must not duplicate env policy",
    );

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
fn cargo_json_output_shape_preserves_warnings_errors_and_summary() {
    let cargo_json = include_str!("../tests/artifacts/tool-output-shaping/cargo-json.txt");

    let shaped = shape_shell_output(
        "cargo test --workspace --message-format=json",
        cargo_json,
        "",
        false,
        Some(101),
    );

    assert_eq!(shaped.family, "cargo");
    assert_eq!(shaped.kind, "structured");
    assert!(shaped.stdout.contains("warning: unused variable"));
    assert!(shaped.stdout.contains("error[E0425]"));
    assert!(shaped.stdout.contains("build-finished success=false"));
    assert!(shaped.stderr.is_empty());
    assert_eq!(shaped.fallback_reason, None);
}

#[test]
fn jest_json_output_shape_preserves_failure_and_summary() {
    let jest_json = include_str!("../tests/artifacts/tool-output-shaping/jest-json.txt");

    let shaped = shape_shell_output("jest --json", jest_json, "", false, Some(1));

    assert_eq!(shaped.family, "jest");
    assert_eq!(shaped.kind, "structured");
    assert!(shaped.stdout.contains("numFailedTests=1"));
    assert!(shaped.stdout.contains("Error: expected true to be false"));
}

#[test]
fn unstructured_shape_drops_noise_and_keeps_signal() {
    let noisy = include_str!("../tests/artifacts/tool-output-shaping/noisy-shell.txt");

    let shaped = shape_shell_output("cargo fmt --check", noisy, "", false, Some(1));

    assert_eq!(shaped.family, "cargo");
    assert!(shaped.stdout.contains("warning: unused variable"));
    assert!(shaped.stdout.contains("error: expected `;`"));
    assert!(shaped.stdout.contains("test result: FAILED"));
    assert!(!shaped.stdout.contains("Compiling crate_a"));
    assert!(shaped.stdout.contains("repeated previous line"));
}

#[test]
fn cargo_json_with_libtest_plain_text_preserves_test_failure() {
    // `cargo test --message-format=json` interleaves JSON cargo events with
    // libtest's plain-text harness output (panics, "test result: FAILED",
    // etc.). The shaped output has to surface those plain-text failure lines
    // or shaped verify runs silently hide test failures.
    let mixed = include_str!("../tests/artifacts/tool-output-shaping/cargo-test-mixed.txt");

    let shaped = shape_shell_output(
        "cargo test --workspace --message-format=json",
        mixed,
        "",
        false,
        Some(101),
    );

    assert_eq!(shaped.family, "cargo");
    assert_eq!(shaped.kind, "structured");
    assert!(shaped.stdout.contains("build-finished success=true"));
    assert!(
        shaped.stdout.contains("test result: FAILED"),
        "expected libtest failure summary in shaped output: {}",
        shaped.stdout
    );
    assert!(
        shaped.stdout.contains("panicked at"),
        "expected panic line preserved: {}",
        shaped.stdout
    );
    assert!(
        shaped.stdout.contains("error: test failed"),
        "expected libtest error tail preserved: {}",
        shaped.stdout
    );
}

#[test]
fn test_report_json_parses_when_stderr_has_non_json_chatter() {
    // npm/jest commonly print warnings to stderr while the structured report
    // lands on stdout. The shaper has to ignore the stderr chatter instead of
    // concatenating both streams into a single malformed document.
    let jest_stdout =
        include_str!("../tests/artifacts/tool-output-shaping/jest-json.txt").to_string();
    let stderr = "npm WARN deprecated foo@1.0.0\nnpm notice using cache\n";

    let shaped = shape_shell_output("jest --json", &jest_stdout, stderr, false, Some(1));

    assert_eq!(shaped.family, "jest");
    assert_eq!(shaped.kind, "structured");
    assert!(shaped.stdout.contains("numFailedTests=1"));
    assert!(shaped.stdout.contains("Error: expected true to be false"));
}

#[test]
fn nextest_json_emits_pass_fail_summary_even_when_all_pass() {
    let pass = include_str!("../tests/artifacts/tool-output-shaping/nextest-pass.txt");

    let shaped = shape_shell_output("cargo nextest run", pass, "", false, Some(0));

    assert_eq!(shaped.family, "nextest");
    assert_eq!(shaped.kind, "structured");
    assert!(
        shaped.stdout.contains("family=nextest"),
        "expected nextest summary line, got {}",
        shaped.stdout
    );
    assert!(
        shaped.stdout.contains("passed=2"),
        "expected pass tally in shaped output: {}",
        shaped.stdout
    );
    assert!(
        shaped.stdout.contains("failed=0"),
        "expected fail tally in shaped output: {}",
        shaped.stdout
    );
}

#[test]
fn unstructured_shape_keeps_head_and_tail_around_long_quiet_runs() {
    // Build an output where the only "signal" line lives at the very end, and
    // a large block of quiet (but non-noise) lines in the middle. The shaper
    // should retain the head, the trailing context, and the signal line.
    let mut output = String::new();
    for i in 0..200 {
        output.push_str(&format!("quiet line {i}\n"));
    }
    output.push_str("error: something blew up at the end\n");

    let shaped = shape_shell_output("/usr/bin/custom-tool", &output, "", false, Some(1));

    assert_eq!(shaped.family, "shell");
    assert!(
        shaped.stdout.contains("quiet line 0"),
        "expected head preserved"
    );
    assert!(
        shaped.stdout.contains("quiet line 199"),
        "expected tail line preserved: {}",
        shaped.stdout
    );
    assert!(
        shaped.stdout.contains("error: something blew up"),
        "expected signal line preserved"
    );
    assert!(
        shaped.stdout.contains("dropped"),
        "expected drop accounting in shaped output: {}",
        shaped.stdout
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
    assert_eq!(
        extract_http_urls("See https://example.com/docs, then https://docs.rs/squeezy."),
        vec![
            "https://docs.rs/squeezy".to_string(),
            "https://example.com/docs".to_string()
        ]
    );
}

#[test]
fn web_cache_receipt_status_marks_stale_entries() {
    let retrieved_at = 1_000_u128;
    let stale_after = web_cache_stale_after_unix_ms(retrieved_at);

    assert_eq!(web_cache_receipt_status(retrieved_at, stale_after), "fresh");
    assert_eq!(
        web_cache_receipt_status(retrieved_at, stale_after + 1),
        "stale"
    );
}

#[test]
fn web_stable_output_sha256_is_deterministic_and_kind_scoped() {
    let request = "request-hash";
    let content = "content-hash";
    let quote = "quote-hash";

    let webfetch = web_stable_output_sha256("webfetch", request, content, quote);
    let websearch = web_stable_output_sha256("websearch", request, content, quote);
    let webfetch_again = web_stable_output_sha256("webfetch", request, content, quote);

    assert_eq!(webfetch, webfetch_again);
    assert_ne!(webfetch, websearch);
    assert_eq!(webfetch.len(), 64);
    assert_eq!(
        web_stable_output_sha256("webfetch", request, "different-content", quote),
        web_stable_output_sha256("webfetch", request, "different-content", quote)
    );
    assert_ne!(
        webfetch,
        web_stable_output_sha256("webfetch", request, "different-content", quote)
    );
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
async fn websearch_returns_citations_cache_receipt_and_redacted_quote() {
    let root = temp_workspace("websearch_citations");
    let text = "Rust docs at https://doc.rust-lang.org/book/. API_TOKEN=super-secret-value";
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"content":[{{"type":"text","text":{}}}]}}}}"#,
        serde_json::to_string(text).expect("quote")
    );
    let http = Arc::new(MockWebHttpClient::default());
    http.push_post_response(ok_response("application/json", body.as_bytes()));
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
                // Cap large enough to fit the full redacted text so this test
                // verifies redaction behavior without depending on whether the
                // marker survives middle-truncation.
                arguments: json!({"query": "rust book", "output_byte_cap": 200}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["evidence"]["kind"], "remote_search");
    assert_eq!(
        result.content["source_urls"][0],
        "https://doc.rust-lang.org/book/"
    );
    assert_eq!(
        result.content["citations"][0]["url"],
        "https://doc.rust-lang.org/book/"
    );
    assert_eq!(result.content["cache_receipt"]["kind"], "websearch");
    assert_eq!(result.content["cache_receipt"]["status"], "fresh");
    assert!(
        result.content["cache_receipt"]["stable_output_sha256"]
            .as_str()
            .is_some_and(|value| value.len() == 64)
    );
    let quote = result.content["result"].as_str().expect("quote");
    assert!(quote.contains("<redacted:"));
    assert!(!quote.contains("super-secret-value"));
    assert!(result.cost_hint.redactions > 0);

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
    assert_eq!(result.content["source_url"], "https://example.com/docs");
    assert_eq!(result.content["evidence"]["kind"], "remote_document");
    assert_eq!(
        result.content["citations"][0]["url"],
        "https://example.com/docs"
    );
    assert_eq!(result.content["cache_receipt"]["kind"], "webfetch");
    assert_eq!(result.content["cache_receipt"]["status"], "fresh");
    let requests = http.get_requests.lock().expect("get requests");
    assert_eq!(*requests, vec!["https://example.com/docs".to_string()]);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_quote_limit_is_enforced_after_redaction() {
    let root = temp_workspace("webfetch_redacted_quote_limit");
    let body = format!("API_TOKEN=super-secret-value {}", "a".repeat(200));
    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(ok_response("text/plain", body.as_bytes()));
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
                arguments: json!({
                    "url": "https://example.com/docs",
                    "output_byte_cap": 64,
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let content = result.content["content"].as_str().expect("content");
    assert!(
        content.len() <= 64,
        "len={} content={content}",
        content.len()
    );
    assert!(!content.contains("super-secret-value"));
    assert_eq!(result.content["quote_limit_bytes"], 64);
    assert!(result.content["quote_truncated"].as_bool().unwrap_or(false));
    assert!(result.cost_hint.redactions > 0);
    assert!(
        result.cost_hint.truncated,
        "cost_hint.truncated must mirror quote_truncated"
    );

    let cache_receipt = result.content["cache_receipt"]
        .as_object()
        .expect("cache_receipt");
    let request_sha = cache_receipt["request_sha256"]
        .as_str()
        .expect("request_sha256");
    let content_sha = cache_receipt["content_sha256"]
        .as_str()
        .expect("content_sha256");
    let quote_sha = cache_receipt["quote_sha256"]
        .as_str()
        .expect("quote_sha256");
    let expected_stable = web_stable_output_sha256("webfetch", request_sha, content_sha, quote_sha);
    assert_eq!(cache_receipt["stable_output_sha256"], expected_stable);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn webfetch_quote_keeps_tail_under_byte_cap() {
    // F02 acceptance: middle-truncate preserves tail signal so the model can
    // still see the end of a fetched document (article summary, error footer,
    // last paragraph) even after a small byte cap.
    let root = temp_workspace("webfetch_keeps_tail");
    let mut body = String::with_capacity(100_000);
    body.push_str("[[HEAD_SIGNAL]] ");
    for _ in 0..2_000 {
        body.push_str("filler ");
    }
    body.push_str(" [[TAIL_SIGNAL]]");
    assert!(body.len() >= 100_000 / 8); // confirm sufficiently large

    let http = Arc::new(MockWebHttpClient::default());
    http.push_get_response(ok_response("text/plain", body.as_bytes()));
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
                call_id: "call_tail".to_string(),
                name: "webfetch".to_string(),
                arguments: json!({
                    "url": "https://example.com/article",
                    "output_byte_cap": 1_024,
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    let content = result.content["content"].as_str().expect("content");
    assert!(
        content.len() <= 1_024,
        "content len {} > cap",
        content.len()
    );
    assert!(
        content.contains("[[TAIL_SIGNAL]]"),
        "tail signal missing from middle-truncated content: {content:?}"
    );
    assert!(
        content.contains("[[HEAD_SIGNAL]]"),
        "head signal missing from middle-truncated content: {content:?}"
    );
    assert!(result.content["quote_truncated"].as_bool().unwrap_or(false));

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
        .iter()
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "apply_patch",
            "decl_search",
            "definition_search",
            "diff_context",
            "downstream_flow",
            "glob",
            "grep",
            "hierarchy",
            "list_skills",
            "load_skill",
            "notes_recall",
            "notes_remember",
            "plan_patch",
            "read_file",
            "read_slice",
            "read_tool_output",
            "reference_search",
            "refresh_compiler_facts",
            "repo_map",
            "shell",
            "symbol_context",
            "upstream_flow",
            "verify",
            "webfetch",
            "websearch",
            "write_file"
        ]
    );

    let checkpoint_names = registry_with_checkpoints(&root)
        .specs()
        .iter()
        .filter(|spec| spec.name.starts_with("checkpoint_"))
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        checkpoint_names,
        vec![
            "checkpoint_list",
            "checkpoint_revert",
            "checkpoint_show",
            "checkpoint_undo"
        ]
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn tool_registry_specs_returns_same_arc_until_refresh() {
    // F04: per-turn `specs()` must reuse the same allocation across calls
    // and only rebuild when MCP refresh invalidates the cache.
    let root = temp_workspace("specs_arc_cache");
    let registry = ToolRegistry::new(&root).expect("registry");

    let first = registry.specs();
    let second = registry.specs();
    assert!(
        Arc::ptr_eq(&first, &second),
        "specs() did not reuse the cached Arc across consecutive calls"
    );

    // Refreshing MCP (no servers configured here, so this is a no-op refresh)
    // should invalidate the cache so the next call returns a freshly-built
    // Arc.
    let _ = registry.refresh_mcp_tools(CancellationToken::new()).await;
    let third = registry.specs();
    assert!(
        !Arc::ptr_eq(&first, &third),
        "specs() reused stale Arc after refresh_mcp_tools"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn graph_navigation_tools_answer_architecture_calls_and_exact_slices() {
    let root = temp_workspace("graph_navigation_tools");
    write_rust_crate(
        &root,
        r#"
pub mod service {
    pub struct Runner;

    impl Runner {
        pub fn run(&self) {
            helper();
        }
    }

    pub fn helper() {}
}
"#,
    );
    let registry = ToolRegistry::new(&root).expect("registry");

    let repo_map = registry
        .execute(
            ToolCall {
                call_id: "repo_map".to_string(),
                name: "repo_map".to_string(),
                arguments: json!({"max_depth": 4}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(repo_map.status, ToolStatus::Success);
    assert!(repo_map.content["graph_available"].as_bool().unwrap());
    assert!(
        repo_map.content["packets"]
            .as_array()
            .expect("repo_map packets")
            .iter()
            .any(|packet| packet.to_string().contains("src/lib.rs"))
    );

    let decl = registry
        .execute(
            ToolCall {
                call_id: "decl".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "run", "kind": "method"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(decl.status, ToolStatus::Success);
    let packet = &decl.content["packets"][0];
    assert_uniform_evidence_packet(packet);
    let run_id = packet["symbol"]["id"].as_str().expect("symbol id");

    let read = registry
        .execute(
            ToolCall {
                call_id: "read".to_string(),
                name: "read_slice".to_string(),
                arguments: json!({"symbol_id": run_id, "span_kind": "body"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(read.status, ToolStatus::Success);
    assert!(
        read.content["content"]
            .as_str()
            .unwrap()
            .contains("helper();")
    );
    assert_uniform_evidence_packet(&read.content["packets"][0]);

    let upstream = registry
        .execute(
            ToolCall {
                call_id: "upstream".to_string(),
                name: "upstream_flow".to_string(),
                arguments: json!({"query": "helper", "kind": "function"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(upstream.status, ToolStatus::Success);
    assert!(
        upstream.content["packets"]
            .as_array()
            .expect("upstream packets")
            .iter()
            .any(|packet| packet["claim"].as_str().unwrap_or("").contains("run"))
    );

    let hierarchy = registry
        .execute(
            ToolCall {
                call_id: "hierarchy".to_string(),
                name: "hierarchy".to_string(),
                arguments: json!({"query": "service", "kind": "module", "max_depth": 3}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(hierarchy.status, ToolStatus::Success);
    assert!(
        hierarchy.content["hierarchy"]
            .to_string()
            .contains("Runner")
    );

    let context = registry
        .execute(
            ToolCall {
                call_id: "context".to_string(),
                name: "symbol_context".to_string(),
                arguments: json!({"query": "Runner"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(context.status, ToolStatus::Success);
    assert_uniform_evidence_packet(&context.content["packets"][0]);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn refresh_compiler_facts_caches_diagnostics_for_symbol_context() {
    let root = temp_workspace("compiler_facts_symbol_context");
    write_rust_crate(
        &root,
        r#"
pub fn bad() -> i32 {
    "nope"
}
"#,
    );
    let registry = registry_with_shell_sandbox_off(&root);

    let refresh = registry
        .execute(
            ToolCall {
                call_id: "facts".to_string(),
                name: "refresh_compiler_facts".to_string(),
                arguments: json!({"diagnostics": true}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(refresh.status, ToolStatus::Success, "{:?}", refresh.content);
    assert_eq!(refresh.content["summary"]["packages"].as_u64(), Some(1));
    assert_eq!(refresh.content["summary"]["targets"].as_u64(), Some(1));
    assert!(
        refresh.content["summary"]["diagnostics"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "{}",
        refresh.content
    );

    let context = registry
        .execute(
            ToolCall {
                call_id: "context".to_string(),
                name: "symbol_context".to_string(),
                arguments: json!({"query": "bad"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(context.status, ToolStatus::Success);
    let diagnostics = context.content["packets"][0]["diagnostics"]
        .as_array()
        .expect("diagnostics");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic["message"]
            .as_str()
            .unwrap_or("")
            .contains("mismatched types")),
        "{}",
        context.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_accepts_filter_only_callable_java_query() {
    let root = temp_workspace("decl_search_java_callable_count");
    fs::write(
        root.join("Foo.java"),
        r#"
class Foo {
    void alpha() {}
    int beta() { return 1; }
    static void gamma() {}
}
"#,
    )
    .expect("write java source");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "decl_filter".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({
                    "language": "Java",
                    "kind": "callable",
                    "max_results": 10
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success, "{:?}", result.content);
    assert_eq!(result.content["total_matches"].as_u64(), Some(3));
    assert_eq!(result.content["returned_matches"].as_u64(), Some(3));
    assert_eq!(
        result.content["counts_by_language"]["Java"].as_u64(),
        Some(3)
    );
    assert_eq!(result.content["counts_by_kind"]["method"].as_u64(), Some(3));
    assert_eq!(result.content["packets"].as_array().unwrap().len(), 3);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_rejects_empty_unfiltered_query() {
    let root = temp_workspace("decl_search_empty_unfiltered");
    write_rust_crate(&root, "pub fn marker() {}\n");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "decl_empty".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content["error"]
            .as_str()
            .unwrap_or("")
            .contains("requires a query or at least one filter"),
        "{}",
        result.content
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn graph_navigation_tools_return_unsupported_language_fallback() {
    let root = temp_workspace("graph_unsupported_fallback");
    write_rust_crate(&root, "pub fn marker() {}\n");
    fs::write(root.join("notes.foo"), "needle\n").expect("write unsupported");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "unsupported".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "needle", "path": "notes.foo"}),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(
        result.content["fallback"]["status"].as_str(),
        Some("no_graph_evidence")
    );
    assert_eq!(
        result.content["fallback"]["reason"].as_str(),
        Some("path_unsupported")
    );
    assert_eq!(
        result.content["fallback"]["suggested_tools"][0]["tool"].as_str(),
        Some("grep")
    );
    assert!(result.content["packets"].as_array().unwrap().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn definition_search_reference_search_and_downstream_flow_resolve_targets() {
    let root = temp_workspace("graph_definition_reference_downstream");
    write_rust_crate(
        &root,
        r#"
pub mod service {
    pub fn entry() {
        crate::pipeline::stage_one();
    }
}

pub mod pipeline {
    pub fn stage_one() {
        stage_two();
    }

    pub fn stage_two() {
        complete();
    }

    pub fn complete() {}
}
"#,
    );
    let registry = ToolRegistry::new(&root).expect("registry");

    let definition = registry
        .execute(
            ToolCall {
                call_id: "definition".to_string(),
                name: "definition_search".to_string(),
                arguments: json!({"query": "stage_one"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(definition.status, ToolStatus::Success);
    let first_definition = &definition.content["packets"][0];
    assert_uniform_evidence_packet(first_definition);
    let stage_one_id = first_definition["symbol"]["id"]
        .as_str()
        .expect("definition packet carries a symbol id")
        .to_string();
    assert_eq!(
        first_definition["next_action"]["tool"].as_str(),
        Some("read_slice"),
        "definition_search must point at read_slice for the exact declaration"
    );

    let reference_by_text = registry
        .execute(
            ToolCall {
                call_id: "reference_text".to_string(),
                name: "reference_search".to_string(),
                arguments: json!({"text": "stage_one"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(reference_by_text.status, ToolStatus::Success);
    let text_packets = reference_by_text.content["packets"]
        .as_array()
        .expect("reference_search packets");
    assert!(
        text_packets
            .iter()
            .any(|packet| packet["reference"]["text"].as_str() == Some("stage_one")),
        "text-mode reference_search must surface lexical hits, got {text_packets:?}"
    );

    let reference_by_symbol = registry
        .execute(
            ToolCall {
                call_id: "reference_symbol".to_string(),
                name: "reference_search".to_string(),
                arguments: json!({"symbol_id": stage_one_id}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(reference_by_symbol.status, ToolStatus::Success);
    let symbol_packets = reference_by_symbol.content["packets"]
        .as_array()
        .expect("reference_search packets");
    assert!(
        !symbol_packets.is_empty(),
        "symbol-bound reference_search must return at least one reference for stage_one"
    );
    for packet in symbol_packets {
        assert_uniform_evidence_packet(packet);
    }

    let downstream_bfs = registry
        .execute(
            ToolCall {
                call_id: "downstream_bfs".to_string(),
                name: "downstream_flow".to_string(),
                arguments: json!({"query": "stage_one", "max_depth": 2}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(downstream_bfs.status, ToolStatus::Success);
    let bfs_packets = downstream_bfs.content["packets"]
        .as_array()
        .expect("downstream_flow packets");
    let depths = bfs_packets
        .iter()
        .filter_map(|packet| packet["depth"].as_u64())
        .collect::<Vec<_>>();
    assert!(
        depths.contains(&1) && depths.contains(&2),
        "BFS at max_depth=2 must surface both depth 1 (stage_one→stage_two) and depth 2 (stage_two→complete), got depths {depths:?}"
    );

    let entry_definition = registry
        .execute(
            ToolCall {
                call_id: "entry_def".to_string(),
                name: "definition_search".to_string(),
                arguments: json!({"query": "entry"}),
            },
            CancellationToken::new(),
        )
        .await;
    let entry_id = entry_definition.content["packets"][0]["symbol"]["id"]
        .as_str()
        .expect("entry symbol id")
        .to_string();

    let downstream_chain = registry
        .execute(
            ToolCall {
                call_id: "downstream_chain".to_string(),
                name: "downstream_flow".to_string(),
                arguments: json!({
                    "symbol_id": entry_id,
                    "target_query": "complete",
                    "max_depth": 5,
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(downstream_chain.status, ToolStatus::Success);
    let chain_packets = downstream_chain.content["packets"]
        .as_array()
        .expect("downstream_flow chain packets");
    assert!(
        chain_packets.iter().any(|packet| packet["claim"]
            .as_str()
            .unwrap_or("")
            .contains("call chain found")),
        "downstream_flow with target_query must emit a call_chain packet, got {chain_packets:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn upstream_flow_truncates_when_callers_exceed_max_results() {
    let root = temp_workspace("graph_upstream_truncation");
    write_rust_crate(
        &root,
        r#"
pub fn target() {}

pub fn caller_a() { target(); }
pub fn caller_b() { target(); }
pub fn caller_c() { target(); }
pub fn caller_d() { target(); }
"#,
    );
    let registry = ToolRegistry::new(&root).expect("registry");

    let upstream = registry
        .execute(
            ToolCall {
                call_id: "upstream_truncated".to_string(),
                name: "upstream_flow".to_string(),
                arguments: json!({"query": "target", "kind": "function", "max_results": 2}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(upstream.status, ToolStatus::Success);
    assert_eq!(
        upstream.content["truncated"].as_bool(),
        Some(true),
        "upstream_flow must report truncated=true when callers exceed max_results"
    );
    assert_eq!(
        upstream.content["packets"].as_array().map(Vec::len),
        Some(2)
    );
    assert!(upstream.cost_hint.truncated);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_emits_confidence_distribution() {
    let root = temp_workspace("graph_confidence_distribution");
    write_rust_crate(
        &root,
        r#"
pub fn alpha() {}
pub fn beta() {}
pub fn gamma() {}
"#,
    );
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "decl_distribution".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "alpha"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    let distribution = &result.cost_hint.confidence_distribution;
    assert!(
        !distribution.is_empty(),
        "decl_search must populate confidence_distribution"
    );
    let total: u32 = distribution.values().copied().sum();
    let returned = result.content["returned_matches"].as_u64().unwrap();
    assert_eq!(
        total as u64, returned,
        "distribution counts must sum to returned_matches"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_resolves_fuzzy_symbol_query() {
    let root = temp_workspace("graph_fuzzy_symbol");
    write_rust_crate(
        &root,
        r#"
pub struct GraphManager;
pub struct PageRenderer;
pub fn unrelated() {}
"#,
    );
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "fuzzy_symbol".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "graphmgr"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    let packets = result.content["packets"]
        .as_array()
        .expect("decl_search packets array");
    assert!(
        packets
            .iter()
            .any(|packet| packet["symbol"]["name"].as_str() == Some("GraphManager")),
        "fuzzy `graphmgr` query must resolve to `GraphManager`, got {packets:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_path_filter_accepts_fuzzy_path_token() {
    let root = temp_workspace("graph_fuzzy_path");
    write_rust_crate(&root, "pub fn entry() {}\n");
    fs::create_dir_all(root.join("crates/squeezy-graph/src")).expect("create graph dirs");
    fs::write(
        root.join("crates/squeezy-graph/src/lib.rs"),
        "pub fn open() {}\n",
    )
    .expect("graph src");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "fuzzy_path".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "open", "path": "squeezy_graph"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    let packets = result.content["packets"]
        .as_array()
        .expect("decl_search packets array");
    assert!(
        packets.iter().any(|packet| packet["symbol"]["path"]
            .as_str()
            .map(|p| p.contains("squeezy-graph"))
            .unwrap_or(false)),
        "fuzzy `path: squeezy_graph` must match `squeezy-graph`, got {packets:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_zero_hit_emits_grep_fallback_for_supported_path() {
    let root = temp_workspace("graph_zero_hit_supported");
    write_rust_crate(
        &root,
        r#"
pub fn alpha() {}
pub fn beta() {}
"#,
    );
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "zero_supported".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({
                    "query": "no_such_symbol",
                    "path": "src/lib.rs",
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    let fallback = &result.content["fallback"];
    assert_eq!(fallback["status"].as_str(), Some("no_graph_evidence"));
    assert_eq!(
        fallback["reason"].as_str(),
        Some("supported_language_no_match")
    );
    assert_eq!(fallback["path"].as_str(), Some("src/lib.rs"));
    let tools = fallback["suggested_tools"]
        .as_array()
        .expect("suggested_tools array");
    let grep = tools
        .iter()
        .find(|tool| tool["tool"].as_str() == Some("grep"))
        .expect("grep entry");
    assert_eq!(grep["arguments"]["path"].as_str(), Some("src/lib.rs"));
    assert_eq!(
        grep["arguments"]["pattern"].as_str(),
        Some("no_such_symbol")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_zero_hit_no_path_scope() {
    let root = temp_workspace("graph_zero_hit_no_path");
    write_rust_crate(&root, "pub fn alpha() {}\n");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "zero_no_path".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "no_such_symbol_unique_xyzzy"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    let fallback = &result.content["fallback"];
    assert_eq!(fallback["reason"].as_str(), Some("no_path_scope"));
    assert!(fallback["path"].is_null());
    let tools = fallback["suggested_tools"]
        .as_array()
        .expect("suggested_tools array");
    let grep = tools
        .iter()
        .find(|tool| tool["tool"].as_str() == Some("grep"))
        .expect("grep entry");
    assert_eq!(grep["arguments"]["path"].as_str(), Some("."));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_zero_hit_regex_escapes_query() {
    let root = temp_workspace("graph_zero_hit_regex");
    write_rust_crate(&root, "pub fn alpha() {}\n");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "zero_regex".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "foo.bar()"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    let pattern = result.content["fallback"]["suggested_tools"][0]["arguments"]["pattern"]
        .as_str()
        .expect("pattern string");
    assert!(
        pattern.contains(r"\."),
        "regex metacharacters must be escaped; got {pattern:?}"
    );
    assert!(
        pattern.contains(r"\(") && pattern.contains(r"\)"),
        "parentheses must be escaped; got {pattern:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_non_empty_packets_keeps_null_fallback() {
    let root = temp_workspace("graph_fallback_null_when_hits");
    write_rust_crate(&root, "pub fn alpha() {}\n");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "non_empty".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "alpha"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert!(
        result.content["fallback"].is_null(),
        "fallback must be null when graph returned packets, got {}",
        result.content["fallback"]
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn decl_search_distribution_absent_when_no_matches() {
    let root = temp_workspace("graph_confidence_distribution_empty");
    write_rust_crate(&root, "pub fn alpha() {}\n");
    let registry = ToolRegistry::new(&root).expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "decl_distribution_empty".to_string(),
                name: "decl_search".to_string(),
                arguments: json!({"query": "no_such_symbol_xyzzy"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert!(result.cost_hint.confidence_distribution.is_empty());
    let cost_hint_json = serde_json::to_value(&result.cost_hint).expect("cost_hint serialises");
    assert!(
        cost_hint_json.get("confidence_distribution").is_none(),
        "empty distribution must be skipped in serialised cost_hint, got {cost_hint_json}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn downstream_flow_surfaces_candidate_set_on_ambiguous_call() {
    let root = temp_workspace("graph_candidate_set_packet");
    fs::write(
        root.join("dispatch.py"),
        r#"class Alpha:
    def do_thing(self):
        return 1

class Beta:
    def do_thing(self):
        return 2

def caller(obj):
    return obj.do_thing()
"#,
    )
    .expect("write python source");
    let registry = ToolRegistry::new(&root).expect("registry");

    let downstream = registry
        .execute(
            ToolCall {
                call_id: "candidate_downstream".to_string(),
                name: "downstream_flow".to_string(),
                arguments: json!({"query": "caller", "max_depth": 1}),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(downstream.status, ToolStatus::Success);
    let packets = downstream.content["packets"]
        .as_array()
        .expect("downstream packets");
    let candidate_packet = packets
        .iter()
        .find(|packet| packet["edge"]["confidence"].as_str() == Some("CandidateSet"))
        .unwrap_or_else(|| panic!("expected at least one CandidateSet packet: {packets:?}"));
    let candidates = candidate_packet["candidates"]
        .as_array()
        .unwrap_or_else(|| {
            panic!("CandidateSet packet must include candidates: {candidate_packet}")
        });
    assert_eq!(candidates.len(), 2);
    for entry in candidates {
        assert_eq!(entry["name"].as_str(), Some("do_thing"));
    }
    let fanout = candidate_packet["next_action"]["fanout"]
        .as_array()
        .expect("candidate set packet must carry a read_slice fanout");
    assert!(
        !fanout.is_empty(),
        "fanout must contain at least one read_slice candidate"
    );
    for entry in fanout {
        assert_eq!(entry["tool"].as_str(), Some("read_slice"));
        assert!(entry["arguments"]["path"].as_str().is_some());
    }

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
            ..Default::default()
        },
        &GraphConfig::default(),
        squeezy_core::ShellSandboxConfig::default(),
        ToolRegistryRuntime::default(),
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

#[tokio::test]
async fn disabled_skill_is_listed_but_not_loaded() {
    let root = temp_workspace("skill_tools_disabled");
    write_skill(&root.join(".agents/skills/rust-nav"), "rust-nav");
    let registry = ToolRegistry::new_with_configs_and_skills(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        SkillsConfig {
            user_dir: root.join("user-skills"),
            compat_user_dir: root.join("compat-skills"),
            config: vec![SkillConfigEntry {
                name: Some("rust-nav".to_string()),
                enabled: false,
                ..Default::default()
            }],
            ..Default::default()
        },
        &GraphConfig::default(),
        squeezy_core::ShellSandboxConfig::default(),
        ToolRegistryRuntime::default(),
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
    assert_eq!(list.content["skills"][0]["disabled"], true);

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
    assert_eq!(loaded.status, ToolStatus::Error);
    assert!(
        loaded.content["error"]
            .as_str()
            .is_some_and(|error| error.contains("skill disabled"))
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_result_records_implicit_skill_activation() {
    let root = temp_workspace("skill_tools_implicit_shell");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav");
    let scripts = skill_dir.join("scripts");
    fs::create_dir_all(&scripts).expect("mkdir scripts");
    fs::write(scripts.join("init.sh"), "printf ok\n").expect("write script");
    let registry = ToolRegistry::new_with_configs_and_skills(
        &root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        SkillsConfig {
            user_dir: root.join("user-skills"),
            compat_user_dir: root.join("compat-skills"),
            ..Default::default()
        },
        &GraphConfig::default(),
        squeezy_core::ShellSandboxConfig {
            mode: squeezy_core::ShellSandboxMode::Off,
            ..Default::default()
        },
        ToolRegistryRuntime::default(),
    )
    .expect("registry");

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "sh .squeezy/skills/rust-nav/scripts/init.sh",
                    "description": "run skill script"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(
        result.content["implicit_skill_activation"]["name"],
        "rust-nav"
    );
    assert_eq!(
        result.content["implicit_skill_activation"]["source"],
        "implicit"
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

fn assert_uniform_evidence_packet(packet: &Value) {
    for key in [
        "claim",
        "spans",
        "confidence",
        "freshness",
        "provenance",
        "cost_hint",
        "next_action",
    ] {
        assert!(
            packet.get(key).is_some(),
            "missing evidence key {key}: {packet}"
        );
    }
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

// =====================================================================
// Hardening tests: wrapper bypasses, redirect detection, sensitive
// paths, audit concurrency, and approval metadata. These pin the
// review-driven behavior so regressions break CI rather than silently
// loosening the security floor.
// =====================================================================

#[test]
fn wrapper_unwrap_propagates_destructive_through_sh_c() {
    let analysis = analyze_shell_command("sh -c \"rm -rf /tmp/work\"");
    assert_eq!(analysis.capability, PermissionCapability::Destructive);
    assert!(analysis.destructive, "destructive flag must propagate");
    assert_eq!(analysis.risk, PermissionRisk::Critical);
}

#[test]
fn wrapper_unwrap_propagates_destructive_through_bash_lc() {
    let analysis = analyze_shell_command("bash -lc 'rm -rf target'");
    assert_eq!(analysis.capability, PermissionCapability::Destructive);
    assert!(analysis.destructive);
}

#[test]
fn wrapper_unwrap_propagates_destructive_through_nohup_and_env() {
    let nohup = analyze_shell_command("nohup rm -rf target");
    assert_eq!(nohup.capability, PermissionCapability::Destructive);
    assert!(nohup.destructive);

    let env_wrap = analyze_shell_command("env CARGO_TERM_COLOR=never rm -rf target");
    assert_eq!(env_wrap.capability, PermissionCapability::Destructive);
    assert!(env_wrap.destructive);
}

#[test]
fn wrapper_unwrap_propagates_destructive_through_xargs_and_sudo() {
    let xargs = analyze_shell_command("xargs -I{} rm -rf {}");
    assert_eq!(xargs.capability, PermissionCapability::Destructive);

    // `sudo` is intrinsically destructive (first-token match), but the
    // unwrap should still surface inner network classification.
    let sudo_curl = analyze_shell_command("sudo curl https://example.com");
    assert_eq!(sudo_curl.capability, PermissionCapability::Destructive);
    assert!(
        sudo_curl.network,
        "inner network classification must bubble up through sudo",
    );
}

#[test]
fn wrapper_unwrap_propagates_network_through_sh_c() {
    let analysis = analyze_shell_command("sh -c 'curl https://example.com'");
    assert!(
        analysis.network,
        "network classification must propagate through sh -c"
    );
    // It's still Shell capability (the wrapper itself is Shell), but the
    // network bit is what drives sandbox/approval surface.
    assert!(matches!(
        analysis.capability,
        PermissionCapability::Network | PermissionCapability::Shell
    ));
}

#[test]
fn wrapper_unwrap_is_bounded_and_does_not_loop() {
    // Pathological deeply-nested wrappers should be analysed bounded.
    let analysis = analyze_shell_command(
        "nohup nice -n 5 timeout 30 sh -c \"env FOO=bar bash -c 'rm -rf target'\"",
    );
    assert!(analysis.destructive);
    assert_eq!(analysis.capability, PermissionCapability::Destructive);
}

#[test]
fn destructive_redirect_detection_ignores_fd_duplication_and_quotes() {
    // `2>&1` is fd duplication, not a write to file → not destructive.
    let test_stderr = analyze_shell_command("cargo test 2>&1");
    assert_eq!(test_stderr.capability, PermissionCapability::Compiler);
    assert!(!test_stderr.destructive);

    // Quoted `>` is not a redirect.
    let echo_arrow = analyze_shell_command("echo 'a>b'");
    assert!(!echo_arrow.destructive);

    // Real output redirect to a filename IS destructive.
    let true_redirect = analyze_shell_command("echo hi > out.txt");
    assert_eq!(true_redirect.capability, PermissionCapability::Destructive);
    assert!(true_redirect.destructive);

    // `>&-` closes a fd; not a write.
    let close_fd = analyze_shell_command("cargo test 1>&-");
    assert!(!close_fd.destructive);
}

#[test]
fn destructive_git_detection_requires_token_boundaries() {
    // `git push --force-with-lease` is destructive.
    let force_lease = analyze_shell_command("git push --force-with-lease");
    assert!(force_lease.destructive);

    // `git push origin main` is not (no --force).
    let safe_push = analyze_shell_command("git push origin main");
    assert!(!safe_push.destructive);

    // Any flag starting with `--force` is treated as destructive: typo or
    // not, we'd rather over-prompt than miss a real force push.
    let force_variant = analyze_shell_command("git push --force-lease=origin/main");
    assert!(force_variant.destructive);

    // Quoted occurrences of the word "force" inside an argument do NOT
    // trigger destructive classification.
    let safe_grep = analyze_shell_command("git log --grep 'force'");
    assert!(!safe_grep.destructive);

    // `git branch -D foo` is destructive (forced delete).
    let force_delete = analyze_shell_command("git branch -D feature/x");
    assert!(force_delete.destructive);

    // `git branch foo` is not.
    let safe_branch = analyze_shell_command("git branch new-feature");
    assert!(!safe_branch.destructive);
}

#[test]
fn sensitive_path_matcher_ignores_substring_false_positives() {
    let patterns = squeezy_core::ShellSandboxConfig::default().sensitive_path_patterns;
    // `.environment` should NOT match `.env*`.
    assert!(
        shell_command_references_sensitive_path("cat .environment", &patterns).is_none(),
        "matcher must not false-positive on .environment",
    );
    // `cat Cargo.envelope` ditto.
    assert!(shell_command_references_sensitive_path("cat Cargo.envelope", &patterns).is_none(),);
}

#[test]
fn sensitive_path_matcher_catches_quoted_and_expanded_bypasses() {
    let patterns = squeezy_core::ShellSandboxConfig::default().sensitive_path_patterns;
    assert!(shell_command_references_sensitive_path("cat .env", &patterns).is_some());
    assert!(shell_command_references_sensitive_path("cat ./.env.production", &patterns).is_some());
    assert!(shell_command_references_sensitive_path("cat ~/.ssh/id_rsa", &patterns).is_some());
    // $HOME expansion: only catches when HOME is set; test the
    // token-shape detection by setting a known HOME.
    unsafe {
        env::set_var("HOME", "/tmp/sensitive-home-test");
    }
    assert!(shell_command_references_sensitive_path("cat $HOME/.ssh/id_rsa", &patterns).is_some(),);
    unsafe {
        env::remove_var("HOME");
    }
}

#[test]
fn shell_audit_store_is_safe_under_concurrent_appends() {
    let root = temp_workspace("shell_audit_concurrent");
    let store = Arc::new(ShellAuditStore::new(&root));
    let mut handles = Vec::new();
    for worker in 0..8 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                store
                    .append(&json!({
                        "worker": worker,
                        "i": i,
                        // Realistic payload to exercise multi-write paths.
                        "payload": "x".repeat(256),
                    }))
                    .expect("audit append");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("audit worker");
    }
    let log =
        fs::read_to_string(root.join(".squeezy/audit/shell.jsonl")).expect("audit log present");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 8 * 50, "every append must produce one line");
    for line in &lines {
        let parsed: Value = serde_json::from_str(line).expect("each line must be valid JSON");
        assert!(parsed.get("worker").is_some());
        assert!(parsed.get("i").is_some());
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn shell_approval_metadata_includes_widened_keys() {
    let root = temp_workspace("approval_metadata_keys");
    let registry = registry_with_shell_sandbox_off(&root);
    let request = registry.permission_request(&ToolCall {
        call_id: "cmd".to_string(),
        name: "shell".to_string(),
        arguments: json!({
            "command": "cargo test --workspace",
            "description": "run tests",
            "timeout_ms": 60_000,
            "output_byte_cap": 16_000,
        }),
    });
    for key in [
        "command",
        "cwd",
        "description",
        "env",
        "network",
        "destructive",
        "timeout_ms",
        "output_byte_cap",
        "sandbox",
        "sandbox_network",
        "parser_backed",
    ] {
        assert!(
            request.metadata.contains_key(key),
            "metadata missing key {key}",
        );
    }
    assert_eq!(request.metadata["timeout_ms"], "60000");
    assert_eq!(request.metadata["output_byte_cap"], "16000");
    // env value must NOT contain raw env var values; only the policy
    // label is allowed.
    assert!(request.metadata["env"].contains("allowlist"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn shell_approval_summary_no_longer_duplicates_command_or_cwd() {
    let root = temp_workspace("approval_summary_dedupe");
    let registry = registry_with_shell_sandbox_off(&root);
    let request = registry.permission_request(&ToolCall {
        call_id: "cmd".to_string(),
        name: "shell".to_string(),
        arguments: json!({"command": "cargo test", "description": "tests"}),
    });
    assert!(
        !request.summary.contains("cargo test"),
        "summary must not duplicate the command (in metadata)"
    );
    assert!(
        !request.summary.contains("cwd="),
        "summary must not duplicate cwd"
    );
    assert!(
        !request.summary.contains("env="),
        "summary must not duplicate env policy"
    );
    assert!(request.summary.contains("description=\"tests\""));
    let _ = fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// Shell sandbox planner + runtime-detection unit tests.
//
// These cover internal seams (`prepare_shell_sandbox_plan_with_probe`,
// `shell_sandbox_runtime_unavailable_with_probe`, `ShellSandboxPlan`,
// `analyze_shell_command`) and therefore must stay in the crate as unit
// tests. The host-backed smoke tests that exercise the sandboxed
// `ToolRegistry::execute` path live in
// `crates/squeezy-tools/tests/shell_sandbox_smoke.rs` so they can use the
// public API surface only.

fn sandbox_config(
    mode: ShellSandboxMode,
    network: ShellSandboxNetworkPolicy,
) -> ShellSandboxConfig {
    ShellSandboxConfig {
        mode,
        network,
        ..ShellSandboxConfig::default()
    }
}

fn fake_sandbox_plan(backend: &'static str, required: bool) -> ShellSandboxPlan {
    ShellSandboxPlan {
        program: "sh".to_string(),
        args: vec!["-lc".to_string(), "true".to_string()],
        backend,
        mode: if required { "required" } else { "best_effort" },
        network: "denied",
        filesystem: "enforced",
        required,
        configured_read_roots: Vec::new(),
        configured_write_roots: Vec::new(),
        filesystem_read_roots: Vec::new(),
        filesystem_write_roots: Vec::new(),
        fallback_reason: None,
    }
}

fn prepare_sandbox_plan_with_probes(
    command: &str,
    config: &ShellSandboxConfig,
    macos_available: bool,
    linux_available: bool,
) -> std::result::Result<ShellSandboxPlan, String> {
    let analysis = analyze_shell_command(command);
    prepare_shell_sandbox_plan_with_probe(
        command,
        &analysis,
        Path::new("/tmp"),
        config,
        macos_available,
        linux_available,
        true,
    )
}

#[test]
fn shell_sandbox_plan_mode_off_returns_direct() {
    let plan = prepare_sandbox_plan_with_probes(
        "printf ok",
        &sandbox_config(
            ShellSandboxMode::Off,
            ShellSandboxNetworkPolicy::DenyByDefault,
        ),
        true,
        true,
    )
    .expect("plan");

    assert_eq!(plan.backend, "none");
    assert_eq!(plan.mode, "off");
    assert_eq!(plan.program, "sh");
    assert!(!plan.required);
}

#[test]
fn shell_sandbox_plan_external_skips_inner_backend() {
    let plan = prepare_sandbox_plan_with_probes(
        "printf ok",
        &sandbox_config(
            ShellSandboxMode::External,
            ShellSandboxNetworkPolicy::DenyByDefault,
        ),
        true,
        true,
    )
    .expect("external plan");

    assert_eq!(plan.backend, "external");
    assert_eq!(plan.mode, "external");
    assert_eq!(plan.network, "external");
    assert_eq!(plan.filesystem, "external");
    assert!(!plan.required);
}

#[test]
#[cfg(target_os = "macos")]
fn macos_sandbox_profile_deny_lists_protected_metadata_under_write_roots() {
    let root = temp_workspace("macos_profile_metadata");
    let profile = macos_shell_sandbox_profile(&root, &ShellSandboxConfig::default(), false);
    let git_path = root.join(".git").display().to_string();

    assert!(profile.contains("require-not"), "{profile}");
    assert!(profile.contains(&git_path), "{profile}");

    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(target_os = "macos")]
fn shell_sandbox_plan_required_when_sandbox_exec_absent() {
    let err = prepare_sandbox_plan_with_probes(
        "printf ok",
        &sandbox_config(
            ShellSandboxMode::Required,
            ShellSandboxNetworkPolicy::DenyByDefault,
        ),
        false,
        true,
    )
    .expect_err("required mode must fail closed");

    assert!(err.contains("/usr/bin/sandbox-exec not found"));
}

#[test]
#[cfg(target_os = "macos")]
fn shell_sandbox_plan_best_effort_when_sandbox_exec_absent() {
    let plan = prepare_sandbox_plan_with_probes(
        "printf ok",
        &sandbox_config(
            ShellSandboxMode::BestEffort,
            ShellSandboxNetworkPolicy::DenyByDefault,
        ),
        false,
        true,
    )
    .expect("best effort falls back");

    assert_eq!(plan.backend, "none");
    assert_eq!(plan.mode, "best_effort");
    assert!(
        plan.fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("sandbox unavailable")),
        "{:?}",
        plan.fallback_reason
    );
}

#[test]
#[cfg(target_os = "linux")]
fn shell_sandbox_plan_required_when_userns_unavailable() {
    let err = prepare_sandbox_plan_with_probes(
        "printf ok",
        &sandbox_config(
            ShellSandboxMode::Required,
            ShellSandboxNetworkPolicy::DenyByDefault,
        ),
        true,
        false,
    )
    .expect_err("required mode must fail closed");

    assert!(err.contains("required shell sandbox unavailable: linux unshare"));
}

#[test]
#[cfg(target_os = "linux")]
fn shell_sandbox_plan_best_effort_when_userns_unavailable() {
    let plan = prepare_sandbox_plan_with_probes(
        "printf ok",
        &sandbox_config(
            ShellSandboxMode::BestEffort,
            ShellSandboxNetworkPolicy::DenyByDefault,
        ),
        true,
        false,
    )
    .expect("best effort falls back");

    assert_eq!(plan.backend, "none");
    assert_eq!(plan.mode, "best_effort");
    assert!(
        plan.fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("linux unshare")),
        "{:?}",
        plan.fallback_reason
    );
}

#[test]
fn shell_termination_reason_reports_missing_exit_status() {
    assert_eq!(
        shell_termination_reason(false, 120_000, None, None).as_deref(),
        Some("shell command ended without an exit code")
    );
    assert_eq!(
        shell_termination_reason(false, 120_000, None, Some(9)).as_deref(),
        Some("shell command terminated by signal 9")
    );
    assert_eq!(
        shell_termination_reason(false, 120_000, Some(1), None),
        None
    );
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn shell_sandbox_plan_network_posture_allow_when_approved() {
    let plan = prepare_sandbox_plan_with_probes(
        "curl https://example.com",
        &sandbox_config(
            ShellSandboxMode::Required,
            ShellSandboxNetworkPolicy::AllowWhenApproved,
        ),
        true,
        true,
    )
    .expect("plan");

    assert_eq!(plan.network, "allowed_approved");
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn shell_sandbox_plan_network_posture_denied_classified() {
    let plan = prepare_sandbox_plan_with_probes(
        "curl https://example.com",
        &sandbox_config(
            ShellSandboxMode::Required,
            ShellSandboxNetworkPolicy::DenyByDefault,
        ),
        true,
        true,
    )
    .expect("plan");

    assert_eq!(plan.network, "denied_classified");
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn shell_sandbox_plan_network_posture_denied_non_network() {
    let plan = prepare_sandbox_plan_with_probes(
        "printf ok",
        &sandbox_config(
            ShellSandboxMode::Required,
            ShellSandboxNetworkPolicy::AllowWhenApproved,
        ),
        true,
        true,
    )
    .expect("plan");

    assert_eq!(plan.network, "denied");
}

#[test]
#[cfg(unix)]
fn shell_best_effort_falls_back_when_sandbox_dies_without_output() {
    use std::os::unix::process::ExitStatusExt;

    let plan = fake_sandbox_plan("macos-sandbox-exec", false);
    let run = ShellRunOutcome {
        exit_status: Some(std::process::ExitStatus::from_raw(6)),
        timed_out: false,
        stdout_bytes: Vec::new(),
        stdout_truncated: false,
        stderr_bytes: Vec::new(),
        stderr_truncated: false,
        preserved_env: Vec::new(),
    };

    let reason =
        shell_sandbox_direct_fallback_reason(&plan, &run).expect("best effort fallback reason");

    assert!(reason.contains("signal 6"), "{reason}");
    assert!(reason.contains("best_effort"), "{reason}");
}

#[test]
fn shell_sandbox_backend_health_skips_probe_after_best_effort_failure() {
    let health = ShellSandboxHealth::default();
    health.mark_unavailable("macos-sandbox-exec", "probe exited with code 71");
    let config = sandbox_config(
        ShellSandboxMode::BestEffort,
        ShellSandboxNetworkPolicy::DenyByDefault,
    );
    let probed = std::cell::Cell::new(false);

    let plan = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(apply_shell_sandbox_backend_health(
            "printf ok",
            &config,
            &health,
            fake_sandbox_plan("macos-sandbox-exec", false),
            |_, _| {
                probed.set(true);
                std::future::ready(None)
            },
        ))
        .expect("best effort direct fallback");

    assert!(!probed.get(), "cached failure should skip the probe");
    assert_eq!(plan.backend, "none");
    assert!(
        plan.fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("probe exited with code 71")),
        "{:?}",
        plan.fallback_reason
    );
}

#[test]
fn shell_sandbox_backend_health_fails_closed_for_required_mode() {
    let health = ShellSandboxHealth::default();
    health.mark_unavailable("macos-sandbox-exec", "probe exited with code 71");
    let config = sandbox_config(
        ShellSandboxMode::Required,
        ShellSandboxNetworkPolicy::DenyByDefault,
    );
    let probed = std::cell::Cell::new(false);

    let err = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(apply_shell_sandbox_backend_health(
            "printf ok",
            &config,
            &health,
            fake_sandbox_plan("macos-sandbox-exec", true),
            |_, _| {
                probed.set(true);
                std::future::ready(None)
            },
        ))
        .expect_err("required mode fails closed");

    assert!(!probed.get(), "cached failure should skip the probe");
    assert!(err.contains("required shell sandbox backend macos-sandbox-exec unavailable"));
    assert!(err.contains("probe exited with code 71"));
}

#[test]
fn shell_sandbox_backend_health_caches_probe_failure() {
    let health = ShellSandboxHealth::default();
    let config = sandbox_config(
        ShellSandboxMode::BestEffort,
        ShellSandboxNetworkPolicy::DenyByDefault,
    );

    let plan = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(apply_shell_sandbox_backend_health(
            "printf ok",
            &config,
            &health,
            fake_sandbox_plan("macos-sandbox-exec", false),
            |_, _| std::future::ready(Some("probe timed out after 500 ms".to_string())),
        ))
        .expect("best effort direct fallback");

    assert_eq!(plan.backend, "none");
    assert!(
        matches!(
            health.status("macos-sandbox-exec"),
            Some(ShellSandboxBackendStatus::Unavailable(reason))
                if reason.contains("probe timed out")
        ),
        "{:?}",
        health.status("macos-sandbox-exec")
    );
}

#[test]
fn shell_sandbox_backend_health_caches_probe_success() {
    let health = ShellSandboxHealth::default();
    let config = sandbox_config(
        ShellSandboxMode::BestEffort,
        ShellSandboxNetworkPolicy::DenyByDefault,
    );

    let plan = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(apply_shell_sandbox_backend_health(
            "printf ok",
            &config,
            &health,
            fake_sandbox_plan("macos-sandbox-exec", false),
            |_, _| std::future::ready(None),
        ))
        .expect("healthy backend");

    assert_eq!(plan.backend, "macos-sandbox-exec");
    assert!(matches!(
        health.status("macos-sandbox-exec"),
        Some(ShellSandboxBackendStatus::Available)
    ));
}

#[test]
#[cfg(unix)]
fn shell_best_effort_falls_back_when_sandbox_apply_fails_at_runtime() {
    use std::os::unix::process::ExitStatusExt;

    let plan = fake_sandbox_plan("macos-sandbox-exec", false);
    let run = ShellRunOutcome {
        exit_status: Some(std::process::ExitStatus::from_raw(71 << 8)),
        timed_out: false,
        stdout_bytes: Vec::new(),
        stdout_truncated: false,
        stderr_bytes: b"sandbox_apply: Operation not permitted".to_vec(),
        stderr_truncated: false,
        preserved_env: Vec::new(),
    };

    let reason = shell_sandbox_best_effort_fallback_reason(&plan, &run)
        .expect("best effort runtime fallback reason");

    assert!(reason.contains("failed at runtime"), "{reason}");
    assert!(reason.contains("best_effort"), "{reason}");
}

#[test]
fn shell_checkpoint_policy_skips_read_only_commands() {
    let ls = analyze_shell_command("ls -la");
    assert!(!shell_command_needs_checkpoint(false, &ls));

    let git_status = analyze_shell_command("git status --short");
    assert!(!shell_command_needs_checkpoint(false, &git_status));

    let write = analyze_shell_command("printf created > created.txt");
    assert!(shell_command_needs_checkpoint(false, &write));
    assert!(!shell_command_needs_checkpoint(true, &write));
}

#[test]
fn shell_sandbox_runtime_unavailable_detects_macos_exit_71_with_sandbox_apply() {
    let plan = fake_sandbox_plan("macos-sandbox-exec", true);

    assert!(shell_sandbox_runtime_unavailable_with_probe(
        &plan,
        Some(71),
        "sandbox_apply: Operation not permitted",
        true,
    ));
}

#[test]
fn shell_sandbox_runtime_unavailable_detects_linux_exit_1_empty_stderr_when_userns_gone() {
    let plan = fake_sandbox_plan("linux-direct-syscalls", true);

    assert!(shell_sandbox_runtime_unavailable_with_probe(
        &plan,
        Some(1),
        "",
        false,
    ));
}

#[test]
fn shell_sandbox_runtime_unavailable_ignores_nonzero_exit_with_stderr() {
    let linux_plan = fake_sandbox_plan("linux-direct-syscalls", true);
    let macos_plan = fake_sandbox_plan("macos-sandbox-exec", true);

    assert!(!shell_sandbox_runtime_unavailable_with_probe(
        &linux_plan,
        Some(1),
        "command failed",
        false,
    ));
    assert!(!shell_sandbox_runtime_unavailable_with_probe(
        &macos_plan,
        Some(71),
        "ordinary exit",
        true,
    ));
}

#[test]
fn shell_sandbox_runtime_unavailable_ignores_direct_backend() {
    let plan = fake_sandbox_plan("none", true);

    assert!(!shell_sandbox_runtime_unavailable_with_probe(
        &plan,
        Some(1),
        "",
        false,
    ));
}

#[test]
fn grep_spec_promotes_graph_first() {
    let description = grep_spec().description;
    for marker in [
        "decl_search",
        "reference_search",
        "symbol_context",
        "graph returned zero packets",
    ] {
        assert!(
            description.contains(marker),
            "grep_spec must mention `{marker}`; got: {description}"
        );
    }
    for family in squeezy_core::LanguageFamily::all() {
        assert!(
            description.contains(family.display_name()),
            "grep_spec must name `{}` from LanguageFamily::all(); got: {description}",
            family.display_name(),
        );
    }
    let golden =
        include_str!("../tests/artifacts/tool-spec-descriptions/grep_spec_description.txt").trim();
    assert_eq!(description.trim(), golden);
}

#[test]
fn glob_spec_promotes_graph_first() {
    let description = glob_spec().description;
    assert!(description.contains("decl_search"));
    assert!(description.contains("graph returned zero packets"));
    let golden =
        include_str!("../tests/artifacts/tool-spec-descriptions/glob_spec_description.txt").trim();
    assert_eq!(description.trim(), golden);
}

#[test]
fn read_file_spec_promotes_graph_first() {
    let description = read_file_spec().description;
    assert!(description.contains("decl_search"));
    assert!(description.contains("symbol_context"));
    let golden =
        include_str!("../tests/artifacts/tool-spec-descriptions/read_file_spec_description.txt")
            .trim();
    assert_eq!(description.trim(), golden);
}

#[tokio::test]
async fn notes_remember_then_recall_round_trip() {
    let root = temp_workspace("notes_round_trip");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("open store"));
    let registry = registry_with_state_store(&root, store.clone());

    let remember_result = registry
        .execute(
            ToolCall {
                call_id: "call_remember".to_string(),
                name: "notes_remember".to_string(),
                arguments: json!({
                    "kind": "decision",
                    "text": "Prefer rg over grep for workspace search.",
                    "tags": ["search", "tooling"],
                    "source": "test-suite"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(remember_result.status, ToolStatus::Success);

    let recall_result = registry
        .execute(
            ToolCall {
                call_id: "call_recall".to_string(),
                name: "notes_recall".to_string(),
                arguments: json!({ "query": "search" }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(recall_result.status, ToolStatus::Success);
    let matches = recall_result.content["matches"]
        .as_array()
        .expect("matches array");
    assert!(
        matches
            .iter()
            .any(|item| item["text"].as_str().unwrap_or("").contains("Prefer rg")),
        "recall should return the persisted decision: {recall_result:?}",
    );
}

#[tokio::test]
async fn notes_remember_rejects_unknown_kind() {
    let root = temp_workspace("notes_invalid_kind");
    let store = Arc::new(SqueezyStore::open(&root, None).expect("open store"));
    let registry = registry_with_state_store(&root, store);

    let result = registry
        .execute(
            ToolCall {
                call_id: "call_invalid".to_string(),
                name: "notes_remember".to_string(),
                arguments: json!({
                    "kind": "unsupported_kind",
                    "text": "this should be rejected"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Error);
}

#[tokio::test]
async fn notes_tools_fail_when_no_store_handle_available() {
    let root = temp_workspace("notes_no_store");
    let registry = registry_with_runtime_config(&root, ToolRuntimeConfig::default());
    let result = registry
        .execute(
            ToolCall {
                call_id: "call_remember".to_string(),
                name: "notes_remember".to_string(),
                arguments: json!({
                    "kind": "note",
                    "text": "no store available"
                }),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ToolStatus::Error);
}
